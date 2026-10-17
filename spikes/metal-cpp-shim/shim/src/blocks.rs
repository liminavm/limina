//! The blocks runtime clang's `-fblocks` code calls, after the layout and rules of Apple's
//! libclosure (`Block_private.h`, `runtime.cpp`). metal-cpp turns a `std::function` completion
//! handler into a block that captures a `__block` variable, so copying one needs `__block` storage
//! as well as blocks.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

const REFCOUNT_MASK: i32 = 0xfffe;
const NEEDS_FREE: i32 = 1 << 24;
const HAS_COPY_DISPOSE: i32 = 1 << 25;
const IS_GLOBAL: i32 = 1 << 28;
const BYREF_LAYOUT_EXTENDED: i32 = 1 << 28;

const FIELD_IS_OBJECT: i32 = 3;
const FIELD_IS_BLOCK: i32 = 7;
const FIELD_IS_BYREF: i32 = 8;
const FIELD_IS_WEAK: i32 = 16;
const BYREF_CALLER: i32 = 128;

/// The isa words clang and this runtime put in blocks; only their addresses matter.
#[unsafe(no_mangle)]
pub static _NSConcreteStackBlock: [usize; 32] = [0; 32];
#[unsafe(no_mangle)]
pub static _NSConcreteGlobalBlock: [usize; 32] = [0; 32];
#[unsafe(no_mangle)]
pub static _NSConcreteMallocBlock: [usize; 32] = [0; 32];

#[repr(C)]
pub struct Block {
    isa: *const c_void,
    flags: AtomicI32,
    reserved: i32,
    pub invoke: *const c_void,
    descriptor: *const Descriptor,
}

#[repr(C)]
struct Descriptor {
    reserved: usize,
    size: usize,
    // Present with HAS_COPY_DISPOSE.
    copy: unsafe extern "C" fn(*mut c_void, *const c_void),
    dispose: unsafe extern "C" fn(*const c_void),
}

#[repr(C)]
struct Byref {
    isa: *const c_void,
    forwarding: *mut Byref,
    flags: AtomicI32,
    size: u32,
}

#[repr(C)]
struct ByrefHelpers {
    keep: unsafe extern "C" fn(*mut Byref, *mut Byref),
    destroy: unsafe extern "C" fn(*mut Byref),
}

/// Adds one reference (2 in the count's units) unless the count is saturated.
fn incr(flags: &AtomicI32) {
    let _ = flags.fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| {
        (f & REFCOUNT_MASK != REFCOUNT_MASK).then_some(f + 2)
    });
}

/// Drops one reference; true when it was the last.
fn decr(flags: &AtomicI32) -> bool {
    let old = flags
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| {
            let n = f & REFCOUNT_MASK;
            (n != REFCOUNT_MASK && n != 0).then_some(f - 2)
        })
        .unwrap_or(0);
    old & REFCOUNT_MASK == 2
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Block_copy(arg: *const c_void) -> *mut c_void {
    if arg.is_null() {
        return std::ptr::null_mut();
    }
    let b = arg as *mut Block;
    // SAFETY: the caller passes a block, whose layout clang fixes.
    unsafe {
        let flags = (*b).flags.load(Ordering::Acquire);
        if flags & NEEDS_FREE != 0 {
            incr(&(*b).flags);
            return b.cast();
        }
        if flags & IS_GLOBAL != 0 {
            return b.cast();
        }
        let size = (*(*b).descriptor).size;
        let copy = libc::malloc(size) as *mut Block;
        std::ptr::copy_nonoverlapping(b as *const u8, copy as *mut u8, size);
        (*copy).flags.store(
            (flags & !(REFCOUNT_MASK | 1)) | NEEDS_FREE | 2,
            Ordering::Release,
        );
        (*copy).isa = _NSConcreteMallocBlock.as_ptr().cast();
        if flags & HAS_COPY_DISPOSE != 0 {
            ((*(*b).descriptor).copy)(copy.cast(), b.cast());
        }
        copy.cast()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Block_release(arg: *const c_void) {
    if arg.is_null() {
        return;
    }
    let b = arg as *mut Block;
    // SAFETY: the caller passes a block it holds a reference to.
    unsafe {
        let flags = (*b).flags.load(Ordering::Acquire);
        if flags & IS_GLOBAL != 0 || flags & NEEDS_FREE == 0 {
            return;
        }
        if decr(&(*b).flags) {
            if flags & HAS_COPY_DISPOSE != 0 {
                ((*(*b).descriptor).dispose)(b.cast());
            }
            libc::free(b.cast());
        }
    }
}

unsafe fn byref_copy(arg: *const c_void) -> *mut c_void {
    let src = arg as *mut Byref;
    // SAFETY: a `__block` variable's storage, laid out by clang.
    unsafe {
        let fwd = (*src).forwarding;
        let fflags = (*fwd).flags.load(Ordering::Acquire);
        if fflags & REFCOUNT_MASK == 0 {
            // Still on the stack: move it to the heap, and point both at the copy.
            let size = (*src).size as usize;
            let copy = libc::malloc(size) as *mut Byref;
            let sflags = (*src).flags.load(Ordering::Acquire);
            (*copy).isa = std::ptr::null();
            // 4: one reference for the stack frame, one for the caller.
            (*copy).flags = AtomicI32::new(sflags | NEEDS_FREE | 4);
            (*copy).forwarding = copy;
            (*src).forwarding = copy;
            (*copy).size = (*src).size;
            if sflags & HAS_COPY_DISPOSE != 0 {
                let s2 = src.add(1) as *mut ByrefHelpers;
                let c2 = copy.add(1) as *mut ByrefHelpers;
                (*c2).keep = (*s2).keep;
                (*c2).destroy = (*s2).destroy;
                if sflags & BYREF_LAYOUT_EXTENDED != 0 {
                    *(c2.add(1) as *mut *const c_void) = *(s2.add(1) as *const *const c_void);
                }
                ((*s2).keep)(copy, src);
            } else {
                std::ptr::copy_nonoverlapping(
                    src.add(1) as *const u8,
                    copy.add(1) as *mut u8,
                    size - size_of::<Byref>(),
                );
            }
        } else if fflags & NEEDS_FREE != 0 {
            incr(&(*fwd).flags);
        }
        (*src).forwarding.cast()
    }
}

unsafe fn byref_release(arg: *const c_void) {
    // SAFETY: as in byref_copy.
    unsafe {
        let b = (*(arg as *const Byref)).forwarding;
        let flags = (*b).flags.load(Ordering::Acquire);
        if flags & NEEDS_FREE != 0 && decr(&(*b).flags) {
            if flags & HAS_COPY_DISPOSE != 0 {
                ((*(b.add(1) as *const ByrefHelpers)).destroy)(b);
            }
            libc::free(b.cast());
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Block_object_assign(
    dest: *mut *const c_void,
    obj: *const c_void,
    flags: i32,
) {
    let kind =
        flags & (FIELD_IS_OBJECT | FIELD_IS_BLOCK | FIELD_IS_BYREF | FIELD_IS_WEAK | BYREF_CALLER);
    // SAFETY: clang's copy helpers pass a field of the copy and the value to store in it.
    unsafe {
        *dest = match kind {
            FIELD_IS_BLOCK => _Block_copy(obj),
            k if k == FIELD_IS_BYREF || k == FIELD_IS_BYREF | FIELD_IS_WEAK => byref_copy(obj),
            // Objects are not retained: the blocks this library takes capture none.
            _ => obj,
        }
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Block_object_dispose(obj: *const c_void, flags: i32) {
    let kind =
        flags & (FIELD_IS_OBJECT | FIELD_IS_BLOCK | FIELD_IS_BYREF | FIELD_IS_WEAK | BYREF_CALLER);
    // SAFETY: clang's dispose helpers pass what the matching assign stored.
    unsafe {
        match kind {
            FIELD_IS_BLOCK => _Block_release(obj),
            k if k == FIELD_IS_BYREF || k == FIELD_IS_BYREF | FIELD_IS_WEAK => byref_release(obj),
            _ => {}
        }
    }
}
