//! PID 1 for the nested-virt probe: does the guest get a working KVM, and what does a nested
//! exit cost?
//!
//! Prints the kernel's kvm lines, then runs a one-vCPU KVM VM whose code stores an incrementing
//! counter to an unmapped address in a loop. Every store is an MMIO exit back to us, so the
//! probe checks the exit reason, address and value of each one and times them. A run that
//! comes back with the right counter values is a nested guest that really executed.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

const KVM_GET_API_VERSION: u64 = 0xAE00;
const KVM_CREATE_VM: u64 = 0xAE01;
const KVM_CHECK_EXTENSION: u64 = 0xAE03;
const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xAE04;
const KVM_CREATE_VCPU: u64 = 0xAE41;
const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_AE46;
const KVM_RUN: u64 = 0xAE80;
const KVM_SET_ONE_REG: u64 = 0x4010_AEAC;
const KVM_ARM_VCPU_INIT: u64 = 0x4020_AEAE;
const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_AEAF;
const KVM_CAP_ARM_VM_IPA_SIZE: u64 = 165;
const KVM_EXIT_MMIO: u32 = 6;
/// KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | offsetof(user_pt_regs, pc) / 4.
const REG_PC: u64 = 0x6030_0000_0010_0040;

const CODE_GPA: u64 = 0x1_0000;
const MMIO_GPA: u64 = 0x2000_0000;
const EXITS: u64 = 20_000;

/// movz x1, #0x2000, lsl #16 ; movz x2, #0 ; loop: str x2, [x1] ; add x2, x2, #1 ; b loop
const CODE: [u32; 5] = [
    0xD2A4_0001,
    0xD280_0002,
    0xF900_0022,
    0x9100_0442,
    0x17FF_FFFE,
];

#[repr(C)]
struct MemRegion {
    slot: u32,
    flags: u32,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

#[repr(C)]
struct VcpuInit {
    target: u32,
    features: [u32; 7],
}

#[repr(C)]
struct OneReg {
    id: u64,
    addr: u64,
}

fn ioctl(fd: i32, req: u64, arg: u64) -> Result<i32, String> {
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 {
        Err(format!(
            "ioctl {req:#x}: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(r)
    }
}

fn mount(src: &str, dst: &str, fstype: &str) {
    let c = |s: &str| std::ffi::CString::new(s).unwrap();
    unsafe {
        libc::mount(
            c(src).as_ptr(),
            c(dst).as_ptr(),
            c(fstype).as_ptr(),
            0,
            std::ptr::null(),
        )
    };
}

fn kvm_lines() {
    // /dev/kmsg is one record per read; stop at the end of the buffer.
    let Ok(f) = File::open("/dev/kmsg") else {
        return;
    };
    unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        let msg = line.split_once(';').map_or(line.as_str(), |(_, m)| m);
        let low = msg.to_ascii_lowercase();
        if low.contains("kvm") || low.contains("hyp") || low.contains("el2") {
            println!("KMSG {msg}");
        }
    }
}

fn run() -> Result<(), String> {
    let kvm = File::options()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|e| format!("/dev/kvm: {e}"))?;
    let k = kvm.as_raw_fd();
    println!("RESULT api_version={}", ioctl(k, KVM_GET_API_VERSION, 0)?);
    let ipa = ioctl(k, KVM_CHECK_EXTENSION, KVM_CAP_ARM_VM_IPA_SIZE)?;
    println!("RESULT max_ipa_bits={ipa}");
    let vm_type = if ipa > 0 { ipa.min(40) as u64 } else { 0 };
    let vm = unsafe { OwnedFd::from_raw_fd(ioctl(k, KVM_CREATE_VM, vm_type)?) };

    let mem_size = 0x1_0000usize;
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mem_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        return Err("mmap guest memory failed".into());
    }
    unsafe { std::ptr::copy_nonoverlapping(CODE.as_ptr(), mem as *mut u32, CODE.len()) };
    let region = MemRegion {
        slot: 0,
        flags: 0,
        guest_phys_addr: CODE_GPA,
        memory_size: mem_size as u64,
        userspace_addr: mem as u64,
    };
    ioctl(
        vm.as_raw_fd(),
        KVM_SET_USER_MEMORY_REGION,
        &region as *const _ as u64,
    )?;

    let vcpu = unsafe { OwnedFd::from_raw_fd(ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU, 0)?) };
    let mut init = VcpuInit {
        target: 0,
        features: [0; 7],
    };
    ioctl(
        vm.as_raw_fd(),
        KVM_ARM_PREFERRED_TARGET,
        &mut init as *mut _ as u64,
    )?;
    ioctl(
        vcpu.as_raw_fd(),
        KVM_ARM_VCPU_INIT,
        &init as *const _ as u64,
    )?;
    let pc = CODE_GPA;
    let reg = OneReg {
        id: REG_PC,
        addr: &pc as *const _ as u64,
    };
    ioctl(vcpu.as_raw_fd(), KVM_SET_ONE_REG, &reg as *const _ as u64)?;

    let run_size = ioctl(k, KVM_GET_VCPU_MMAP_SIZE, 0)? as usize;
    let run = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            run_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            vcpu.as_raw_fd(),
            0,
        )
    } as *const u8;
    if run as *mut libc::c_void == libc::MAP_FAILED {
        return Err("mmap kvm_run failed".into());
    }

    let mut lat = Vec::with_capacity(EXITS as usize);
    let start = Instant::now();
    for want in 0..EXITS {
        let t = Instant::now();
        ioctl(vcpu.as_raw_fd(), KVM_RUN, 0)?;
        lat.push(t.elapsed().as_nanos() as u64);
        let reason = unsafe { *(run.add(8) as *const u32) };
        let addr = unsafe { *(run.add(32) as *const u64) };
        let data = unsafe { *(run.add(40) as *const u64) };
        let is_write = unsafe { *run.add(52) };
        if reason != KVM_EXIT_MMIO || addr != MMIO_GPA || data != want || is_write != 1 {
            return Err(format!(
                "exit {want}: reason={reason} addr={addr:#x} data={data} write={is_write}"
            ));
        }
    }
    let total = start.elapsed();
    lat.sort_unstable();
    let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
    println!(
        "RESULT nested_kvm=PASS exits={EXITS} total_ms={:.1} p50_us={:.2} p99_us={:.2} max_us={:.2}",
        total.as_secs_f64() * 1000.0,
        p(0.5),
        p(0.99),
        p(1.0)
    );
    Ok(())
}

fn main() {
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");
    kvm_lines();
    if let Err(e) = run() {
        println!("RESULT nested_kvm=FAIL {e}");
    }
    let _ = std::io::stdout().flush();
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
}
