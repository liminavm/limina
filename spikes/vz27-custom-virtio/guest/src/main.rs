//! vzprobe-guest: PID 1 of a throwaway initramfs, run under `vzprobe` (the host half, in
//! `../host/`). It drives every experiment from inside the guest and reports over vsock:
//!
//! 1. echo latency/throughput on each extra hvc port: the custom virtio console (identity echo)
//!    and Apple's built-in virtio console backed by a pipe (echo XOR 0x20);
//! 2. the custom device's virtio shared-memory region: host maps a buffer of each kind, both
//!    sides write and verify, then the host GPU fills it and the guest checks;
//! 3. reclaim: the guest hands the host PFN runs of touched memory, the host madvises its
//!    mapping of them and reports the VM process footprint;
//! 4. save/restore: the host saves, tears down, restores; the guest re-runs the echo after.

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

const CMD_PORT: u32 = 5000;
const HOST_CID: u32 = 2;

fn log(msg: &str) {
    // fd 1 is /dev/console (hvc0, Apple's log port) when the cpio carries /dev/console.
    let line = format!("[vzprobe-guest] {msg}\n");
    unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
}

fn mount(src: &str, dst: &str, fstype: &str) {
    let _ = fs::create_dir_all(dst);
    let (s, d, t) = (
        CString::new(src).unwrap(),
        CString::new(dst).unwrap(),
        CString::new(fstype).unwrap(),
    );
    let rc = unsafe { libc::mount(s.as_ptr(), d.as_ptr(), t.as_ptr(), 0, std::ptr::null()) };
    if rc != 0 {
        log(&format!(
            "mount {dst} failed: {}",
            std::io::Error::last_os_error()
        ));
    }
}

fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

// ---------- vsock command channel ----------

struct Chan {
    f: Option<fs::File>,
    buf: Vec<u8>,
}

fn vsock_connect(port: u32, secs: u64) -> Option<fs::File> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
        let addr = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as _,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: HOST_CID,
            svm_zero: [0; 4],
        };
        let rc = unsafe {
            libc::connect(
                fd,
                (&addr as *const libc::sockaddr_vm).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as u32,
            )
        };
        if rc == 0 {
            return Some(unsafe { fs::File::from_raw_fd(fd) });
        }
        unsafe { libc::close(fd) };
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Chan {
    fn connect() -> Chan {
        let f = vsock_connect(CMD_PORT, 20);
        if f.is_none() {
            log("no vsock command channel: log-only mode");
        }
        Chan { f, buf: Vec::new() }
    }
    fn raw(&mut self, b: &[u8]) {
        if let Some(f) = self.f.as_mut() {
            let _ = f.write_all(b);
        }
    }
    fn send(&mut self, line: &str) {
        log(&format!("-> {line}"));
        self.raw(format!("{line}\n").as_bytes());
    }
    fn recv(&mut self) -> String {
        if self.f.is_none() {
            return String::from("NOCHAN");
        }
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let s = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                log(&format!("<- {s}"));
                return s;
            }
            let mut tmp = [0u8; 4096];
            match self.f.as_mut().unwrap().read(&mut tmp) {
                Ok(0) | Err(_) => return String::from("EOF"),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            }
        }
    }
    fn call(&mut self, line: &str) -> String {
        self.send(line);
        self.recv()
    }
}

// ---------- hvc echo ports ----------

fn open_raw(path: &str) -> Option<fs::File> {
    let c = CString::new(path).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return None;
    }
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
    }
    Some(unsafe { fs::File::from_raw_fd(fd) })
}

fn read_timeout(f: &mut fs::File, buf: &mut [u8], ms: i32) -> usize {
    let mut p = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, ms) } <= 0 {
        return 0;
    }
    f.read(buf).unwrap_or(0)
}

fn read_exact_timeout(f: &mut fs::File, buf: &mut [u8], ms: i32) -> bool {
    let mut got = 0;
    while got < buf.len() {
        let n = read_timeout(f, &mut buf[got..], ms);
        if n == 0 {
            return false;
        }
        got += n;
    }
    true
}

/// Which hvc ports answer, and how: 'a' -> 'a' is the custom device, 'a' -> 'A' is Apple's.
fn find_ports() -> Vec<(String, String, fs::File)> {
    let console = fs::read_to_string("/sys/class/tty/console/active").unwrap_or_default();
    log(&format!("active console: {}", console.trim()));
    let mut out = Vec::new();
    let mut names: Vec<String> = fs::read_dir("/sys/class/tty")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.retain(|n| n.starts_with("hvc"));
    names.sort();
    for n in names {
        if console.split_whitespace().any(|c| c == n) {
            continue;
        }
        let Some(mut f) = open_raw(&format!("/dev/{n}")) else {
            continue;
        };
        let _ = f.write_all(b"a");
        let mut b = [0u8; 1];
        let kind = match read_timeout(&mut f, &mut b, 1000) {
            1 if b[0] == b'a' => "custom",
            1 if b[0] == b'A' => "apple",
            1 => "unknown",
            _ => "silent",
        };
        log(&format!("{n}: {kind}"));
        if kind == "custom" || kind == "apple" {
            out.push((n, kind.to_string(), f));
        }
    }
    out
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    v.sort_unstable();
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn echo_tests(ch: &mut Chan, tag: &str, ports: &mut [(String, String, fs::File)]) {
    ports.sort_by_key(|p| p.1 != "custom");
    for (name, kind, f) in ports.iter_mut() {
        let xor = if kind == "apple" { 0x20u8 } else { 0 };
        // warm up
        for _ in 0..200 {
            let _ = f.write_all(b"a");
            let mut b = [0u8; 1];
            read_exact_timeout(f, &mut b, 1000);
        }
        let n = 20000;
        let mut rt = Vec::with_capacity(n);
        let mut bad = 0;
        for i in 0..n {
            let c = b'a' + (i % 26) as u8;
            let t0 = now_ns();
            let _ = f.write_all(&[c]);
            let mut b = [0u8; 1];
            if !read_exact_timeout(f, &mut b, 1000) {
                bad += 1;
                continue;
            }
            rt.push(now_ns() - t0);
            if b[0] != c ^ xor {
                bad += 1;
            }
        }
        let mean = rt.iter().sum::<u64>() / rt.len().max(1) as u64;
        let (p50, p90, p99, max) = (
            pct(&mut rt, 0.5),
            pct(&mut rt, 0.9),
            pct(&mut rt, 0.99),
            pct(&mut rt, 1.0),
        );
        ch.send(&format!(
            "RESULT {tag} echo1 {kind}({name}) n={n} bad={bad} mean_us={:.1} p50_us={:.1} p90_us={:.1} p99_us={:.1} max_us={:.1}",
            mean as f64 / 1e3, p50 as f64 / 1e3, p90 as f64 / 1e3, p99 as f64 / 1e3, max as f64 / 1e3
        ));
        if fs::read_to_string("/proc/cmdline")
            .unwrap_or_default()
            .contains("vzprobe.nobulk")
        {
            continue;
        }
        if kind == "apple" {
            // Apple's console behind a pipe wedges the guest tty under full-duplex bulk load
            // (measured: the run hangs); latency is the comparison that matters here.
            continue;
        }
        // throughput: 16 KiB out, 16 KiB back, 1024 rounds, writer thread so neither side
        // stalls; a stall is reported with the bytes that got through, never joined on.
        let chunk = 16 * 1024;
        let rounds = 1024;
        let mut wf = f.try_clone().unwrap();
        let t0 = now_ns();
        let w = std::thread::spawn(move || {
            let data: Vec<u8> = (0..chunk).map(|i| (i % 251) as u8).collect();
            for _ in 0..rounds {
                if wf.write_all(&data).is_err() {
                    break;
                }
            }
        });
        let mut back = vec![0u8; chunk];
        let mut got = 0usize;
        let mut errs = 0usize;
        let mut ok = true;
        for _ in 0..rounds {
            let mut have = 0;
            while have < chunk {
                let n = read_timeout(f, &mut back[have..], 3000);
                if n == 0 {
                    break;
                }
                have += n;
            }
            for (i, b) in back[..have].iter().enumerate() {
                if *b != ((i % 251) as u8) ^ xor {
                    errs += 1;
                }
            }
            got += have;
            if have < chunk {
                ok = false;
                break;
            }
        }
        let dt = now_ns() - t0;
        if ok {
            let _ = w.join();
        }
        let mb = got as f64 / (1024.0 * 1024.0);
        ch.send(&format!(
            "RESULT {tag} bulk {kind}({name}) complete={ok} got_bytes={got}/{} byte_errs={errs} MiB_s={:.1}",
            chunk * rounds,
            mb / (dt as f64 / 1e9)
        ));
    }
}

// ---------- shared memory region ----------

struct Shm {
    resource: String,
    offset: u64,
    len: u64,
    id: u8,
}

fn find_shm() -> Vec<Shm> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/bus/pci/devices") else {
        return out;
    };
    for e in dir.filter_map(|e| e.ok()) {
        let p = e.path();
        let cfg = fs::read(p.join("config")).unwrap_or_default();
        if cfg.len() < 64 || cfg[0..2] != [0xf4, 0x1a] {
            continue;
        }
        let dev = u16::from_le_bytes([cfg[2], cfg[3]]);
        let mut cap = cfg[0x34] as usize;
        let mut guard = 0;
        while cap != 0 && cap + 24 <= cfg.len() && guard < 48 {
            guard += 1;
            let (vndr, next, cfg_type) = (cfg[cap], cfg[cap + 1] as usize, cfg[cap + 3]);
            if vndr == 0x09 && cfg_type == 8 {
                let bar = cfg[cap + 4];
                let id = cfg[cap + 5];
                let rd = |o: usize| {
                    u32::from_le_bytes(cfg[cap + o..cap + o + 4].try_into().unwrap()) as u64
                };
                let offset = rd(8) | (rd(16) << 32);
                let len = rd(12) | (rd(20) << 32);
                log(&format!(
                    "{} dev={dev:#06x}: SHM cap id={id} bar={bar} offset={offset:#x} len={len:#x}",
                    e.file_name().to_string_lossy()
                ));
                out.push(Shm {
                    resource: format!("{}/resource{bar}", p.display()),
                    offset,
                    len,
                    id,
                });
            }
            cap = next;
        }
    }
    out
}

fn shm_tests(ch: &mut Chan, tag: &str) {
    let shms = find_shm();
    let Some(shm) = shms.first() else {
        ch.send(&format!("RESULT {tag} shm none-found"));
        return;
    };
    let win: u64 = 16 << 20;
    let c = CString::new(shm.resource.clone()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_SYNC) };
    if fd < 0 {
        ch.send(&format!(
            "RESULT {tag} shm open-failed {}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    ch.send(&format!("INFO shm id={} region_len={:#x}", shm.id, shm.len));
    for kind in ["anon", "mtl", "mtlnocopy", "heap", "iosurface"] {
        let r = ch.call(&format!("SHM_MAP {kind} {win}"));
        if !r.starts_with("OK") {
            ch.send(&format!("RESULT {tag} shm {kind} host-map-failed {r}"));
            continue;
        }
        let seed: u32 = r
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                win as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                shm.offset as i64,
            )
        };
        if p == libc::MAP_FAILED {
            ch.send(&format!(
                "RESULT {tag} shm {kind} guest-mmap-failed {}",
                std::io::Error::last_os_error()
            ));
            ch.call(&format!("SHM_UNMAP {kind}"));
            continue;
        }
        let words = (win / 4) as usize;
        let half = words / 2;
        let w = unsafe { std::slice::from_raw_parts_mut(p as *mut u32, words) };
        // host wrote word[i] = i ^ seed over the first half
        let t0 = now_ns();
        let host_bad = (0..half)
            .filter(|&i| unsafe { std::ptr::read_volatile(&w[i]) } != (i as u32) ^ seed)
            .count();
        let rd_ns = now_ns() - t0;
        let seed2 = seed.wrapping_mul(2654435761).wrapping_add(1);
        let t1 = now_ns();
        for i in half..words {
            unsafe { std::ptr::write_volatile(&mut w[i], !(i as u32) ^ seed2) };
        }
        unsafe { std::arch::asm!("dsb sy") };
        let wr_ns = now_ns() - t1;
        let mbs = |ns: u64| (half * 4) as f64 / (1024.0 * 1024.0) / (ns as f64 / 1e9);
        let chk = ch.call(&format!("SHM_CHECK {kind} {seed2}"));
        let mut gpu = String::from("n/a");
        if kind != "iosurface" && kind != "anon" {
            let val: u8 = 0x5a;
            let r = ch.call(&format!("SHM_GPUFILL {kind} {val}"));
            if r.starts_with("OK") {
                let b = unsafe { std::slice::from_raw_parts(p as *const u8, win as usize) };
                let bad = (0..win as usize)
                    .filter(|&i| unsafe { std::ptr::read_volatile(&b[i]) } != val)
                    .count();
                gpu = format!("gpu_fill_bad_bytes={bad}");
            } else {
                gpu = format!("gpu_fill_failed({r})");
            }
        }
        ch.send(&format!(
            "RESULT {tag} shm {kind} guest_sees_host_bad_words={host_bad}/{half} host_check=[{chk}] {gpu} guest_uc_read_MiB_s={:.0} guest_uc_write_MiB_s={:.0}",
            mbs(rd_ns), mbs(wr_ns)
        ));
        unsafe { libc::munmap(p, win as usize) };
        ch.call(&format!("SHM_UNMAP {kind}"));
    }
    unsafe { libc::close(fd) };
}

// ---------- reclaim (balloon mechanism) ----------

fn reclaim_test(ch: &mut Chan, tag: &str) {
    let pg = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let size: usize = 512 << 20;
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            -1,
            0,
        )
    } as *mut u8;
    if p as *mut libc::c_void == libc::MAP_FAILED {
        ch.send(&format!("RESULT {tag} reclaim alloc-failed"));
        return;
    }
    unsafe { libc::mlock(p.cast(), size) };
    for off in (0..size).step_by(pg) {
        unsafe {
            std::ptr::write_volatile(p.add(off) as *mut u64, 0xC0FFEE00_0000_0000 | off as u64)
        };
        unsafe { std::ptr::write_volatile(p.add(off + pg - 8) as *mut u64, 0xFEED) };
    }
    ch.call("FOOTPRINT after-guest-touch-512MiB");
    // PFNs via pagemap
    let mut pm = fs::File::open("/proc/self/pagemap").unwrap();
    let npages = size / pg;
    let mut raw = vec![0u8; npages * 8];
    use std::io::{Seek, SeekFrom};
    pm.seek(SeekFrom::Start((p as u64 / pg as u64) * 8))
        .unwrap();
    pm.read_exact(&mut raw).unwrap();
    let pfns: Vec<u64> = raw
        .chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let present = pfns.iter().filter(|e| *e >> 63 == 1).count();
    // contiguous GPA runs, then trim each to 16 KiB host pages
    let host_pg: u64 = 16384;
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for e in &pfns {
        if e >> 63 == 0 {
            continue;
        }
        let gpa = (e & ((1u64 << 55) - 1)) * pg as u64;
        match runs.last_mut() {
            Some((s, l)) if *s + *l == gpa => *l += pg as u64,
            _ => runs.push((gpa, pg as u64)),
        }
    }
    let mut aligned: Vec<(u64, u64)> = Vec::new();
    for (s, l) in &runs {
        let a = s.div_ceil(host_pg) * host_pg;
        let b = (s + l) / host_pg * host_pg;
        if b > a {
            aligned.push((a, b - a));
        }
    }
    let total: u64 = aligned.iter().map(|r| r.1).sum();
    ch.send(&format!(
        "INFO reclaim pages={npages} present={present} runs={} aligned_runs={} aligned_MiB={}",
        runs.len(),
        aligned.len(),
        total >> 20
    ));
    let pattern = |p: *mut u8| {
        for off in (0..size).step_by(pg) {
            unsafe {
                std::ptr::write_volatile(p.add(off) as *mut u64, 0xC0FFEE00_0000_0000 | off as u64)
            };
        }
    };
    for mode in ["reusable", "dontneed", "free"] {
        pattern(p);
        ch.call(&format!("FOOTPRINT before-{mode}"));
        ch.send(&format!("RECLAIM_BEGIN {mode} {}", aligned.len()));
        let mut body = String::new();
        for (s, l) in &aligned {
            body.push_str(&format!("{s} {l}\n"));
        }
        ch.raw(body.as_bytes());
        let r = ch.recv();
        // what does the guest read back now?
        let mut zero = 0usize;
        let mut kept = 0usize;
        let mut other = 0usize;
        for off in (0..size).step_by(pg) {
            let v = unsafe { std::ptr::read_volatile(p.add(off) as *const u64) };
            if v == 0 {
                zero += 1
            } else if v == 0xC0FFEE00_0000_0000 | off as u64 {
                kept += 1
            } else {
                other += 1
            }
        }
        ch.send(&format!(
            "RESULT {tag} reclaim {mode} host=[{r}] guest_readback zero_pages={zero} kept_pages={kept} other={other}"
        ));
        pattern(p);
        ch.call(&format!("FOOTPRINT after-{mode}-retouch"));
    }
    unsafe {
        libc::munlock(p.cast(), size);
        libc::munmap(p.cast(), size);
    }
}

fn vsock_echo_test(ch: &mut Chan, tag: &str) {
    let Some(mut f) = vsock_connect(5001, 5) else {
        ch.send(&format!("RESULT {tag} vsock-echo no-listener"));
        return;
    };
    let one = 1i32;
    unsafe {
        libc::setsockopt(
            f.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&one as *const i32).cast(),
            4,
        );
    }
    let n = 20000;
    let mut rt = Vec::with_capacity(n);
    for i in 0..n + 200 {
        let t0 = now_ns();
        let _ = f.write_all(&[b'x']);
        let mut b = [0u8; 1];
        if !read_exact_timeout(&mut f, &mut b, 1000) {
            ch.send(&format!("RESULT {tag} vsock-echo stalled at {i}"));
            return;
        }
        if i >= 200 {
            rt.push(now_ns() - t0);
        }
    }
    let mean = rt.iter().sum::<u64>() / rt.len() as u64;
    let (p50, p90, p99, max) = (
        pct(&mut rt, 0.5),
        pct(&mut rt, 0.9),
        pct(&mut rt, 0.99),
        pct(&mut rt, 1.0),
    );
    ch.send(&format!(
        "RESULT {tag} vsock-echo1 n={n} mean_us={:.1} p50_us={:.1} p90_us={:.1} p99_us={:.1} max_us={:.1}",
        mean as f64 / 1e3, p50 as f64 / 1e3, p90 as f64 / 1e3, p99 as f64 / 1e3, max as f64 / 1e3
    ));
}

fn meminfo(key: &str) -> u64 {
    fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

/// Apple's own balloon, for comparison: the host asks for 1 GiB back, then returns it.
fn balloon_test(ch: &mut Chan, tag: &str) {
    ch.call("FOOTPRINT before-balloon");
    let r = ch.call("BALLOON 1024");
    std::thread::sleep(Duration::from_secs(8));
    let r2 = ch.call("FOOTPRINT balloon-inflated");
    ch.send(&format!(
        "RESULT {tag} apple-balloon inflate=[{r}] MemTotal_kB={} MemFree_kB={} footprint=[{r2}]",
        meminfo("MemTotal:"),
        meminfo("MemFree:")
    ));
    ch.call("BALLOON 0");
    std::thread::sleep(Duration::from_secs(4));
    ch.send(&format!(
        "INFO balloon deflated MemFree_kB={}",
        meminfo("MemFree:")
    ));
}

fn poweroff() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    let pg = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let uname = fs::read_to_string("/proc/version").unwrap_or_default();
    log(&format!("start: pagesize={pg} {}", uname.trim()));
    let mut ch = Chan::connect();
    let tag = format!("pg{}k", pg / 1024);
    ch.send(&format!(
        "HELLO pagesize={pg} kernel={}",
        uname.split_whitespace().nth(2).unwrap_or("?")
    ));
    let mut ports = find_ports();
    ch.send(&format!(
        "INFO ports {}",
        ports
            .iter()
            .map(|p| format!("{}={}", p.0, p.1))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let only = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let want =
        |t: &str| !only.contains("vzprobe.only=") || only.contains(&format!("vzprobe.only={t}"));
    if want("echo") {
        echo_tests(&mut ch, &tag, &mut ports);
    }
    ch.call("FOOTPRINT boot");
    if want("echo") {
        vsock_echo_test(&mut ch, &tag);
    }
    if want("shm") {
        shm_tests(&mut ch, &tag);
    }
    if want("reclaim") {
        reclaim_test(&mut ch, &tag);
    }
    if want("balloon") {
        balloon_test(&mut ch, &tag);
    }
    if want("save") {
        let r = ch.call("SAVE");
        if r.starts_with("OK") {
            // The host pauses, saves, stops, restores and resumes us somewhere in here; vsock
            // connections do not survive that, so reconnect and re-run the echo.
            std::thread::sleep(Duration::from_secs(4));
            drop(ch);
            ch = Chan::connect();
            ch.send("HELLO post-restore");
            let r = ch.call("SAVESTATUS");
            ch.send(&format!("RESULT {tag} save host=[{r}]"));
            let irqs = || {
                fs::read_to_string("/proc/interrupts")
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains("virtio"))
                    .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join(" | ")
            };
            ch.send(&format!("INFO irqs-before-probe {}", irqs()));
            let mut ports = find_ports();
            ch.send(&format!("INFO irqs-after-probe {}", irqs()));
            echo_tests(&mut ch, &format!("{tag}-postrestore"), &mut ports);
        } else {
            ch.send(&format!("RESULT {tag} save refused [{r}]"));
        }
    }
    ch.call("DONE");
    poweroff();
}
