//! The C producer (`include/limina_gpu_ring.h`) against the Rust consumer, in two processes
//! sharing one mapping, as the guest driver and the GPU host do.
//!
//! The consumer parks whenever the ring is empty and then waits only for a kick, never for `tail`:
//! a producer that fails to see the parked state hangs the test instead of passing it.

use std::ffi::c_void;
use std::process::Command;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use limina_gpu_wire::cmd::{Commit, FillBuffer, decode, op};
use limina_gpu_wire::ring::{Consumer, DATA_OFFSET};

const DATA_SIZE: u32 = 16 * 1024;
const COUNT: u64 = 50_000;
const READY: usize = 0;
const ATTACHED: usize = 1;
const KICKS: usize = 2;
const DONE: usize = 3;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
}

fn wait(what: &str, deadline: Instant, mut f: impl FnMut() -> bool) {
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

#[test]
fn c_producer_round_trips_through_the_rust_consumer() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let manifest = env!("CARGO_MANIFEST_DIR");
    let exe = dir.join("lmn-c-producer");
    let out = Command::new("cc")
        .args(["-std=c11", "-D_DEFAULT_SOURCE", "-O2", "-Wall", "-Werror"])
        .arg(format!("-I{manifest}/include"))
        .arg(format!("{manifest}/tests/c_producer.c"))
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let region = DATA_OFFSET + DATA_SIZE as usize;
    let path = dir.join("lmn-c-producer.ring");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len((region + 16) as u64).unwrap();
    use std::os::fd::AsRawFd;
    // PROT_READ|PROT_WRITE, MAP_SHARED; page-aligned, so 128-aligned.
    let base = unsafe { mmap(std::ptr::null_mut(), region + 16, 3, 1, file.as_raw_fd(), 0) };
    assert!(!base.is_null() && base as isize != -1, "mmap");
    let base = NonNull::new(base.cast::<u8>()).unwrap();
    let words =
        unsafe { std::slice::from_raw_parts(base.as_ptr().add(region).cast::<AtomicU32>(), 4) };

    let mut child = Command::new(&exe)
        .arg(&path)
        .arg(DATA_SIZE.to_string())
        .arg(COUNT.to_string())
        .spawn()
        .expect("spawn the C producer");
    let deadline = Instant::now() + Duration::from_secs(60);

    wait("the ring", deadline, || {
        words[READY].load(Ordering::Acquire) != 0
    });
    let mut cons = unsafe { Consumer::attach(base, DATA_SIZE) }.expect("attach");
    words[ATTACHED].store(1, Ordering::Release);

    let mut scratch = vec![0u8; DATA_SIZE as usize / 2];
    let (mut fills, mut commits, mut parks) = (0u64, 0u64, 0u64);
    while commits < COUNT {
        match cons.next(&mut scratch).expect("ring error") {
            Some(n) => match n.op {
                op::FILL_BUFFER => {
                    let (f, _) = decode::<FillBuffer>(n.payload).unwrap();
                    assert_eq!(
                        (f.buffer.arena, f.buffer.offset, f.value),
                        (1, fills, fills as u32)
                    );
                    assert_eq!(f.length, 64);
                    fills += 1;
                }
                op::COMMIT => {
                    let (c, rest) = decode::<Commit>(n.payload).unwrap();
                    let i = commits;
                    assert_eq!(c.seqno, i);
                    let extra = (i % 37) as usize;
                    for (j, b) in rest[..extra].iter().enumerate() {
                        assert_eq!(*b, (i + j as u64) as u8, "trailing byte {j} of commit {i}");
                    }
                    assert!(rest[extra..].iter().all(|&b| b == 0), "padding is zeroed");
                    commits += 1;
                }
                other => panic!("op {other:#x}"),
            },
            None => {
                let kicks = words[KICKS].load(Ordering::Acquire);
                if cons.park() {
                    parks += 1;
                    wait("a kick", deadline, || {
                        words[KICKS].load(Ordering::Acquire) != kicks
                    });
                    cons.unpark();
                }
            }
        }
    }
    assert_eq!(fills, COUNT);
    let status = child.wait().unwrap();
    assert!(status.success(), "C producer: {status}");
    assert_eq!(words[DONE].load(Ordering::Acquire), 1);
    assert!(!cons.has_work());
    assert!(
        parks > 0,
        "the consumer never parked, so the kick path went untested"
    );
    eprintln!(
        "parked {parks} times, kicked {}",
        words[KICKS].load(Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
}
