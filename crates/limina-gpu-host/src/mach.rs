//! The Mach primitives both processes use: ports, messages with port descriptors, semaphores and
//! memory entries. Hand-rolled FFI, like `crates/limina-launch`: `libc` covers only a few of these.
#![allow(deprecated)] // libc deprecates mach_task_self in favour of the mach2 crate.

use std::io;

use libc::mach_port_t;

pub const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;

pub const MSG_TYPE_MOVE_SEND: u8 = 17;
pub const MSG_TYPE_MOVE_SEND_ONCE: u8 = 18;
pub const MSG_TYPE_COPY_SEND: u8 = 19;
pub const MSG_TYPE_MAKE_SEND: u8 = 20;
pub const MSG_TYPE_MAKE_SEND_ONCE: u8 = 21;
const MSG_PORT_DESCRIPTOR: u32 = 0;
const MSGH_BITS_COMPLEX: u32 = 0x8000_0000;

const SEND_MSG: i32 = 0x1;
const RCV_MSG: i32 = 0x2;
const RCV_TIMEOUT: i32 = 0x100;
pub const RCV_TIMED_OUT: i32 = 0x1000_4003;

const HEADER_BYTES: usize = 24;
const BODY_BYTES: usize = 4;
const PORT_DESCRIPTOR_BYTES: usize = 12;
/// Largest message either side sends, trailer included.
const RECV_BUFFER_BYTES: usize = 4096;
/// Most ports one message carries.
const MAX_PORTS: usize = 4;

const VM_PROT_READ: i32 = 1;
const VM_PROT_WRITE: i32 = 2;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_INHERIT_NONE: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimespec {
    tv_sec: u32,
    tv_nsec: i32,
}

unsafe extern "C" {
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> i32;
    fn mach_port_insert_right(
        task: mach_port_t,
        name: mach_port_t,
        poly: mach_port_t,
        poly_poly: u32,
    ) -> i32;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> i32;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: u32, delta: i32) -> i32;
    fn mach_msg(
        msg: *mut u32,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: mach_port_t,
        timeout: u32,
        notify: mach_port_t,
    ) -> i32;
    fn mach_msg_destroy(msg: *mut u32);
    fn mach_ports_register(task: mach_port_t, ports: *const mach_port_t, count: u32) -> i32;
    fn mach_ports_lookup(task: mach_port_t, ports: *mut *mut mach_port_t, count: *mut u32) -> i32;
    fn semaphore_create(task: mach_port_t, sem: *mut mach_port_t, policy: i32, value: i32) -> i32;
    fn semaphore_destroy(task: mach_port_t, sem: mach_port_t) -> i32;
    fn semaphore_signal(sem: mach_port_t) -> i32;
    fn semaphore_wait(sem: mach_port_t) -> i32;
    fn semaphore_timedwait(sem: mach_port_t, wait: MachTimespec) -> i32;
    fn mach_make_memory_entry_64(
        task: mach_port_t,
        size: *mut u64,
        offset: u64,
        permission: i32,
        object: *mut mach_port_t,
        parent: mach_port_t,
    ) -> i32;
    fn mach_vm_map(
        task: mach_port_t,
        address: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: mach_port_t,
        offset: u64,
        copy: i32,
        cur: i32,
        max: i32,
        inheritance: u32,
    ) -> i32;
    fn mach_vm_deallocate(task: mach_port_t, address: u64, size: u64) -> i32;
    fn mach_vm_allocate(task: mach_port_t, address: *mut u64, size: u64, flags: i32) -> i32;
}

pub fn task() -> mach_port_t {
    // SAFETY: `mach_task_self()` returns this task's self port; always valid.
    unsafe { libc::mach_task_self() }
}

pub fn kr_err(what: &str, kr: i32) -> io::Error {
    io::Error::other(format!("{what}: mach error {kr} (0x{kr:x})"))
}

fn check(what: &str, kr: i32) -> io::Result<()> {
    if kr == 0 {
        Ok(())
    } else {
        Err(kr_err(what, kr))
    }
}

/// A port name this task holds one user reference of a right to; dropping it releases that
/// reference.
#[derive(Debug)]
pub struct Port(mach_port_t);

impl Port {
    /// Takes ownership of one reference to a send (or send-once) right.
    ///
    /// # Safety
    /// The caller must own that reference and not release it elsewhere.
    pub unsafe fn from_send(name: mach_port_t) -> Self {
        Self(name)
    }

    pub fn name(&self) -> mach_port_t {
        self.0
    }

    /// Gives up ownership without releasing (the right was moved in a message).
    pub fn into_name(self) -> mach_port_t {
        let n = self.0;
        std::mem::forget(self);
        n
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if self.0 != MACH_PORT_NULL {
            // SAFETY: we own one reference to this name's send right.
            unsafe { mach_port_deallocate(task(), self.0) };
        }
    }
}

/// A receive right this task owns, plus nothing else.
#[derive(Debug)]
pub struct ReceivePort(mach_port_t);

impl ReceivePort {
    pub fn new() -> io::Result<Self> {
        let mut name = MACH_PORT_NULL;
        // SAFETY: plain allocation into `name`.
        check("mach_port_allocate", unsafe {
            mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut name)
        })?;
        Ok(Self(name))
    }

    pub fn name(&self) -> mach_port_t {
        self.0
    }

    /// A send right to this port, owned by the returned `Port`.
    pub fn make_send(&self) -> io::Result<Port> {
        // SAFETY: we hold the receive right; this adds a send right under the same name.
        check("mach_port_insert_right", unsafe {
            mach_port_insert_right(task(), self.0, self.0, MSG_TYPE_MAKE_SEND as u32)
        })?;
        // SAFETY: the reference just created is ours.
        Ok(unsafe { Port::from_send(self.0) })
    }

    /// Receives one message. `Ok(None)` on timeout.
    pub fn receive(&self, timeout_ms: Option<u32>) -> io::Result<Option<Message>> {
        let mut buf = vec![0u32; RECV_BUFFER_BYTES / 4];
        let mut opts = RCV_MSG;
        if timeout_ms.is_some() {
            opts |= RCV_TIMEOUT;
        }
        // SAFETY: `buf` is RECV_BUFFER_BYTES long; receive only.
        let kr = unsafe {
            mach_msg(
                buf.as_mut_ptr(),
                opts,
                0,
                RECV_BUFFER_BYTES as u32,
                self.0,
                timeout_ms.unwrap_or(0),
                MACH_PORT_NULL,
            )
        };
        if kr == RCV_TIMED_OUT {
            return Ok(None);
        }
        check("mach_msg(receive)", kr)?;
        Message::parse(&mut buf).map(Some)
    }
}

impl Drop for ReceivePort {
    fn drop(&mut self) {
        // SAFETY: we own the receive right.
        unsafe { mach_port_mod_refs(task(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

/// A message: an id, rights, and inline bytes.
#[derive(Debug, Default)]
pub struct Message {
    pub id: i32,
    /// Ports carried, owned by this message once received.
    pub ports: Vec<Port>,
    /// The reply right a request arrived with.
    pub reply: Option<Port>,
    pub data: Vec<u8>,
}

impl Message {
    pub fn new(id: i32, data: &[u8]) -> Self {
        Self {
            id,
            data: data.to_vec(),
            ..Default::default()
        }
    }

    fn parse(buf: &mut [u32]) -> io::Result<Self> {
        let destroy = |buf: &mut [u32]| {
            // SAFETY: the message is in `buf`; destroying it releases every right it carried.
            unsafe { mach_msg_destroy(buf.as_mut_ptr()) }
        };
        let bits = buf[0];
        let size = buf[1] as usize;
        if !(HEADER_BYTES..=RECV_BUFFER_BYTES).contains(&size) {
            destroy(buf);
            return Err(io::Error::other("malformed message size"));
        }
        let remote = buf[2];
        let id = buf[5] as i32;
        // SAFETY: plain bytes view of the message.
        let bytes =
            unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), RECV_BUFFER_BYTES) };
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
        let mut at = HEADER_BYTES;
        let mut ports = Vec::new();
        if bits & MSGH_BITS_COMPLEX != 0 {
            if size < HEADER_BYTES + BODY_BYTES {
                destroy(buf);
                return Err(io::Error::other("malformed complex message"));
            }
            let n = word(at) as usize;
            at += BODY_BYTES;
            if n > MAX_PORTS || at + n * PORT_DESCRIPTOR_BYTES > size {
                destroy(buf);
                return Err(io::Error::other("malformed descriptors"));
            }
            for _ in 0..n {
                let ty = word(at + 8) >> 24;
                if ty != MSG_PORT_DESCRIPTOR {
                    destroy(buf);
                    return Err(io::Error::other("unexpected descriptor type"));
                }
                // SAFETY: the kernel moved this right into our space for this message.
                ports.push(unsafe { Port::from_send(word(at)) });
                at += PORT_DESCRIPTOR_BYTES;
            }
        }
        let reply = (remote != MACH_PORT_NULL).then(|| {
            // SAFETY: the reply right the sender attached is now ours.
            unsafe { Port::from_send(remote) }
        });
        Ok(Self {
            id,
            ports,
            reply,
            data: bytes[at..size].to_vec(),
        })
    }

    /// Lays out the message for `mach_msg`. `ports` pairs each port with its disposition.
    fn build(
        &self,
        dest: mach_port_t,
        dest_disp: u8,
        reply: Option<(mach_port_t, u8)>,
        ports: &[(mach_port_t, u8)],
    ) -> Vec<u32> {
        let mut words = vec![0u32; HEADER_BYTES / 4];
        let (reply_name, reply_disp) = reply.unwrap_or((MACH_PORT_NULL, 0));
        let mut bits = dest_disp as u32 | ((reply_disp as u32) << 8);
        if !ports.is_empty() {
            bits |= MSGH_BITS_COMPLEX;
            words.push(ports.len() as u32);
            for &(p, disp) in ports {
                // `mach_msg_port_descriptor_t`: name, pad1, then pad2:16 | disposition:8 | type:8.
                words.extend_from_slice(&[
                    p,
                    0,
                    ((disp as u32) << 16) | (MSG_PORT_DESCRIPTOR << 24),
                ]);
            }
        }
        let data_at = words.len();
        words.resize(data_at + self.data.len().div_ceil(4), 0);
        // SAFETY: the destination words were just sized for the data.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.data.as_ptr(),
                words[data_at..].as_mut_ptr().cast::<u8>(),
                self.data.len(),
            )
        };
        words[0] = bits;
        words[1] = (words.len() * 4) as u32;
        words[2] = dest;
        words[3] = reply_name;
        words[5] = self.id as u32;
        words
    }
}

/// Sends `msg` to `dest` (a send right we keep), moving or copying `ports` as their dispositions
/// say. Moved rights are consumed by the kernel even on failure, except for a few send errors we
/// treat as fatal for the connection anyway.
pub fn send(dest: &Port, msg: &Message, ports: &[(mach_port_t, u8)]) -> io::Result<()> {
    let mut words = msg.build(dest.name(), MSG_TYPE_COPY_SEND, None, ports);
    let size = (words.len() * 4) as u32;
    // SAFETY: a well-formed message of `size` bytes; send only.
    check("mach_msg(send)", unsafe {
        mach_msg(
            words.as_mut_ptr(),
            SEND_MSG,
            size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        )
    })
}

/// Replies to a request through the send-once right it carried.
pub fn reply(to: Port, msg: &Message, ports: &[(mach_port_t, u8)]) -> io::Result<()> {
    let name = to.into_name();
    let mut words = msg.build(name, MSG_TYPE_MOVE_SEND_ONCE, None, ports);
    let size = (words.len() * 4) as u32;
    // SAFETY: as in `send`; the send-once right is consumed.
    check("mach_msg(reply)", unsafe {
        mach_msg(
            words.as_mut_ptr(),
            SEND_MSG,
            size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        )
    })
}

/// Sends a request and waits for its reply on `reply_port`, which must have no other traffic.
pub fn call(
    dest: &Port,
    reply_port: &ReceivePort,
    msg: &Message,
    ports: &[(mach_port_t, u8)],
    timeout_ms: u32,
) -> io::Result<Message> {
    let mut words = msg.build(
        dest.name(),
        MSG_TYPE_COPY_SEND,
        Some((reply_port.name(), MSG_TYPE_MAKE_SEND_ONCE)),
        ports,
    );
    let size = (words.len() * 4) as u32;
    // SAFETY: a well-formed message; send only. The reply is received separately so its buffer
    // can be sized for any reply.
    check("mach_msg(call)", unsafe {
        mach_msg(
            words.as_mut_ptr(),
            SEND_MSG,
            size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        )
    })?;
    match reply_port.receive(Some(timeout_ms))? {
        Some(m) if m.id == msg.id + REPLY_ID_OFFSET => Ok(m),
        Some(m) => Err(io::Error::other(format!(
            "reply id {:#x} to request {:#x}",
            m.id, msg.id
        ))),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "GPU host did not reply",
        )),
    }
}

/// A reply's id is its request's plus this, the MIG convention.
pub const REPLY_ID_OFFSET: i32 = 100;

/// Registers `ports` for children spawned from now on to find with [`lookup_registered`].
/// Passing none clears the registration.
pub fn register_for_children(ports: &[mach_port_t]) -> io::Result<()> {
    // SAFETY: `ports` is a valid array of `ports.len()` names (or empty).
    check("mach_ports_register", unsafe {
        mach_ports_register(task(), ports.as_ptr(), ports.len() as u32)
    })
}

/// The first port the parent registered before spawning us.
pub fn lookup_registered() -> io::Result<Port> {
    let mut ports: *mut mach_port_t = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-pointers to locals.
    check("mach_ports_lookup", unsafe {
        mach_ports_lookup(task(), &mut ports, &mut count)
    })?;
    if ports.is_null() || count == 0 {
        return Err(io::Error::other("no registered ports"));
    }
    // SAFETY: the kernel returned `count` names in a vm allocation we now own.
    let names = unsafe { std::slice::from_raw_parts(ports, count as usize) }.to_vec();
    // SAFETY: the array was vm-allocated into our task for us.
    unsafe {
        mach_vm_deallocate(
            task(),
            ports as u64,
            (count as usize * size_of::<mach_port_t>()) as u64,
        )
    };
    let mut it = names.into_iter();
    let first = it.next().unwrap_or(MACH_PORT_NULL);
    for other in it.filter(|&n| n != MACH_PORT_NULL) {
        // SAFETY: an extra registered right we do not use.
        unsafe { mach_port_deallocate(task(), other) };
    }
    if first == MACH_PORT_NULL {
        return Err(io::Error::other("registered port is null"));
    }
    // SAFETY: the lookup gave us a send right.
    Ok(unsafe { Port::from_send(first) })
}

/// A Mach semaphore. The creating task destroys it; others hold a send right to it.
#[derive(Debug)]
pub struct Semaphore {
    port: mach_port_t,
    owned: bool,
}

// SAFETY: semaphore operations are thread-safe kernel calls on a port name.
unsafe impl Send for Semaphore {}
unsafe impl Sync for Semaphore {}

impl Semaphore {
    pub fn new() -> io::Result<Self> {
        let mut port = MACH_PORT_NULL;
        // SAFETY: plain creation; SYNC_POLICY_FIFO = 0.
        check("semaphore_create", unsafe {
            semaphore_create(task(), &mut port, 0, 0)
        })?;
        Ok(Self { port, owned: true })
    }

    /// Wraps a semaphore send right received from another task.
    pub fn from_port(port: Port) -> Self {
        Self {
            port: port.into_name(),
            owned: false,
        }
    }

    pub fn name(&self) -> mach_port_t {
        self.port
    }

    pub fn signal(&self) {
        // SAFETY: a semaphore we hold a right to.
        unsafe { semaphore_signal(self.port) };
    }

    pub fn wait(&self) {
        // SAFETY: as above.
        unsafe { semaphore_wait(self.port) };
    }

    /// Waits at most `ms`; returns whether it was signalled.
    pub fn wait_ms(&self, ms: u32) -> bool {
        let t = MachTimespec {
            tv_sec: ms / 1000,
            tv_nsec: ((ms % 1000) * 1_000_000) as i32,
        };
        // SAFETY: as above.
        unsafe { semaphore_timedwait(self.port, t) == 0 }
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        // SAFETY: we created it, or hold a send right to it.
        unsafe {
            if self.owned {
                semaphore_destroy(task(), self.port);
            } else {
                mach_port_deallocate(task(), self.port);
            }
        }
    }
}

/// A named memory entry for `size` bytes at `addr` in this task, readable and writable.
pub fn make_memory_entry(addr: u64, size: u64) -> io::Result<(Port, u64)> {
    let mut len = size;
    let mut entry = MACH_PORT_NULL;
    // SAFETY: out-pointers to locals; the range is ours.
    check("mach_make_memory_entry_64", unsafe {
        mach_make_memory_entry_64(
            task(),
            &mut len,
            addr,
            VM_PROT_READ | VM_PROT_WRITE,
            &mut entry,
            MACH_PORT_NULL,
        )
    })?;
    // SAFETY: the entry's send right is ours.
    Ok((unsafe { Port::from_send(entry) }, len))
}

/// Zeroed memory of this task's own, page-rounded, freed on drop. A memory entry made over it
/// keeps its pages after the drop.
#[derive(Debug)]
pub struct Anonymous {
    addr: u64,
    size: u64,
}

impl Anonymous {
    pub fn new(len: u64) -> io::Result<Self> {
        // SAFETY: `vm_page_size` is set by the kernel before any code runs.
        let page = unsafe { libc::vm_page_size } as u64;
        let size = len.max(1).next_multiple_of(page);
        let mut addr = 0u64;
        // SAFETY: out-pointer to a local; the kernel picks the address.
        check("mach_vm_allocate", unsafe {
            mach_vm_allocate(task(), &mut addr, size, VM_FLAGS_ANYWHERE)
        })?;
        Ok(Self { addr, size })
    }

    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }
}

impl Drop for Anonymous {
    fn drop(&mut self) {
        // SAFETY: we allocated exactly this range.
        unsafe { mach_vm_deallocate(task(), self.addr, self.size) };
    }
}

/// A mapping of a memory entry into this task, unmapped on drop.
#[derive(Debug)]
pub struct Mapping {
    addr: u64,
    size: u64,
}

// SAFETY: a plain range of our address space.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn map(entry: &Port, size: u64) -> io::Result<Self> {
        let mut addr = 0u64;
        // SAFETY: out-pointer to a local; the kernel picks the address.
        check("mach_vm_map", unsafe {
            mach_vm_map(
                task(),
                &mut addr,
                size,
                0,
                VM_FLAGS_ANYWHERE,
                entry.name(),
                0,
                0,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_INHERIT_NONE,
            )
        })?;
        Ok(Self { addr, size })
    }

    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: we mapped exactly this range.
        unsafe { mach_vm_deallocate(task(), self.addr, self.size) };
    }
}
