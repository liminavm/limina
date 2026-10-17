//! The slice of the Objective-C runtime that metal-cpp reaches: interned selectors, classes looked up
//! by name, `objc_msgSend`, reference counting and autorelease pools.
//!
//! Every object and every class starts with a [`Header`], whose `isa` names the class whose methods
//! answer it; a class's `isa` is its metaclass, which holds the class methods. A message that no
//! class in the chain implements aborts, naming the class and the selector.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

pub type Sel = *const c_char;
pub type Imp = *const c_void;

/// The head of every object and class.
#[repr(C)]
pub struct Header {
    pub isa: *const Class,
    pub refs: AtomicUsize,
}

/// A class or a metaclass. Built once, then never changed.
#[repr(C)]
pub struct Class {
    head: Header,
    name: String,
    meta: bool,
    superclass: *const Class,
    methods: HashMap<usize, Imp>,
}

// SAFETY: a class is immutable once built, and its raw pointers name other immutable classes and
// functions.
unsafe impl Sync for Class {}
unsafe impl Send for Class {}

/// An object of some class holding a `T`.
#[repr(C)]
pub struct Obj<T> {
    pub head: Header,
    pub v: T,
}

/// A class as the code below defines it: its methods by selector name.
pub struct Def {
    pub name: &'static str,
    pub superclass: Option<&'static str>,
    pub methods: Vec<(&'static str, Imp)>,
    pub class_methods: Vec<(&'static str, Imp)>,
}

/// `f` as an [`Imp`].
#[macro_export]
macro_rules! imp {
    ($f:expr) => {
        $f as *const () as $crate::runtime::Imp
    };
}

/// A never-freed class object's reference count.
const IMMORTAL: usize = usize::MAX / 2;

fn selectors() -> &'static Mutex<HashMap<String, &'static CStr>> {
    static S: OnceLock<Mutex<HashMap<String, &'static CStr>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The one selector for `name`.
pub fn sel(name: &str) -> Sel {
    let mut s = selectors().lock().unwrap();
    if let Some(c) = s.get(name) {
        return c.as_ptr();
    }
    let c: &'static CStr = Box::leak(CString::new(name).unwrap().into_boxed_c_str());
    s.insert(name.to_owned(), c);
    c.as_ptr()
}

struct Registry {
    classes: HashMap<String, &'static Class>,
}

fn build_class(def: &Def, superclass: Option<&'static Class>) -> &'static Class {
    let table = |m: &[(&'static str, Imp)]| m.iter().map(|&(n, f)| (sel(n) as usize, f)).collect();
    let meta: &'static Class = Box::leak(Box::new(Class {
        head: Header {
            isa: std::ptr::null(),
            refs: AtomicUsize::new(IMMORTAL),
        },
        name: def.name.to_owned(),
        meta: true,
        // The root metaclass's superclass is the root class, so classes answer NSObject's methods.
        superclass: superclass.map_or(std::ptr::null(), |s| s.head.isa),
        methods: table(&def.class_methods),
    }));
    let class: &'static Class = Box::leak(Box::new(Class {
        head: Header {
            isa: meta,
            refs: AtomicUsize::new(IMMORTAL),
        },
        name: def.name.to_owned(),
        meta: false,
        superclass: superclass.map_or(std::ptr::null(), |s| s as *const Class),
        methods: table(&def.methods),
    }));
    if superclass.is_none() {
        // SAFETY: `meta` was leaked just above and nothing else has seen it yet.
        unsafe { (*(meta as *const Class as *mut Class)).superclass = class };
    }
    class
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| {
        let mut classes: HashMap<String, &'static Class> = HashMap::new();
        // Superclasses come first in `defs`.
        for def in crate::defs() {
            let superclass = def.superclass.map(|s| classes[s]);
            classes.insert(def.name.to_owned(), build_class(&def, superclass));
        }
        Registry { classes }
    })
}

/// Class `name`, which this library defines.
pub fn class(name: &str) -> *const Class {
    registry().classes[name]
}

/// A class this library does not define: every message to it aborts with its name.
fn stub(name: &str) -> *const Class {
    static STUBS: OnceLock<Mutex<HashMap<String, &'static Class>>> = OnceLock::new();
    let mut stubs = STUBS.get_or_init(Default::default).lock().unwrap();
    if let Some(c) = stubs.get(name) {
        return *c;
    }
    let c = build_class(
        &Def {
            name: Box::leak(name.to_owned().into_boxed_str()),
            superclass: None,
            methods: vec![],
            class_methods: vec![],
        },
        None,
    );
    stubs.insert(name.to_owned(), c);
    c
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sel_registerName(name: *const c_char) -> Sel {
    // SAFETY: the caller passes a C string.
    sel(&unsafe { CStr::from_ptr(name) }.to_string_lossy())
}

#[unsafe(no_mangle)]
pub extern "C" fn sel_getName(sel: Sel) -> *const c_char {
    sel
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn objc_lookUpClass(name: *const c_char) -> *const Class {
    // SAFETY: the caller passes a C string.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    registry()
        .classes
        .get(name.as_ref())
        .map_or_else(|| stub(&name), |c| *c as *const Class)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn objc_getClass(name: *const c_char) -> *const Class {
    // SAFETY: as for objc_lookUpClass.
    unsafe { objc_lookUpClass(name) }
}

/// No protocol objects: metal-cpp looks every protocol up at load, and MLX checks none.
#[unsafe(no_mangle)]
pub extern "C" fn objc_getProtocol(_: *const c_char) -> *const c_void {
    std::ptr::null()
}

/// The method for `sel` in `recv`'s class chain.
unsafe fn find(recv: *const Header, sel: Sel) -> Option<Imp> {
    // SAFETY: every object and class starts with a Header.
    let mut c = unsafe { (*recv).isa };
    while !c.is_null() {
        // SAFETY: classes are leaked and immutable.
        let class = unsafe { &*c };
        if let Some(&imp) = class.methods.get(&(sel as usize)) {
            return Some(imp);
        }
        c = class.superclass;
    }
    None
}

/// The method `objc_msgSend` jumps to for `sel` sent to `recv`, which is not null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn limina_objc_lookup(recv: *const Header, sel: Sel) -> Imp {
    // SAFETY: the caller passes a live object or class.
    if let Some(imp) = unsafe { find(recv, sel) } {
        return imp;
    }
    // SAFETY: as above; `sel` is an interned C string.
    let class = unsafe { &*(*recv).isa };
    let name = unsafe { CStr::from_ptr(sel) }.to_string_lossy();
    eprintln!(
        "limina-metal: {}[{} {name}] is not implemented",
        if class.meta { "+" } else { "-" },
        class.name
    );
    std::process::abort();
}

// objc_msgSend(receiver, selector, ...) with the method's own signature: the arguments stay where
// the caller put them (x0-x7, q0-q7, the stack, x8 for a returned struct), and the method found
// for them is jumped to. A message to nil returns zero in x0, x1 and d0-d3; a struct returned
// through x8 is left as it was.
#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    ".globl objc_msgSend",
    ".type objc_msgSend, %function",
    "objc_msgSend:",
    "cbz x0, 2f",
    "stp x29, x30, [sp, #-224]!",
    "mov x29, sp",
    "stp x0, x1, [sp, #16]",
    "stp x2, x3, [sp, #32]",
    "stp x4, x5, [sp, #48]",
    "stp x6, x7, [sp, #64]",
    "str x8, [sp, #80]",
    "stp q0, q1, [sp, #96]",
    "stp q2, q3, [sp, #128]",
    "stp q4, q5, [sp, #160]",
    "stp q6, q7, [sp, #192]",
    "bl {lookup}",
    "mov x16, x0",
    "ldp q6, q7, [sp, #192]",
    "ldp q4, q5, [sp, #160]",
    "ldp q2, q3, [sp, #128]",
    "ldp q0, q1, [sp, #96]",
    "ldr x8, [sp, #80]",
    "ldp x6, x7, [sp, #64]",
    "ldp x4, x5, [sp, #48]",
    "ldp x2, x3, [sp, #32]",
    "ldp x0, x1, [sp, #16]",
    "ldp x29, x30, [sp], #224",
    "br x16",
    "2:",
    "mov x1, #0",
    "movi d0, #0",
    "movi d1, #0",
    "movi d2, #0",
    "movi d3, #0",
    "ret",
    ".size objc_msgSend, .-objc_msgSend",
    lookup = sym limina_objc_lookup,
);

/// Sends `name` with no arguments to `obj`.
pub unsafe fn send0(obj: *mut Header, name: &str) -> *mut c_void {
    // SAFETY: the caller passes a live object; every zero-argument method takes (self, _cmd).
    unsafe {
        let s = sel(name);
        let f: extern "C" fn(*mut Header, Sel) -> *mut c_void =
            std::mem::transmute(limina_objc_lookup(obj, s));
        f(obj, s)
    }
}

/// A new object of class `name` holding `v`, with one reference.
pub fn new<T>(name: &str, v: T) -> *mut Obj<T> {
    new_of(class(name), v)
}

pub fn new_of<T>(class: *const Class, v: T) -> *mut Obj<T> {
    Box::into_raw(Box::new(Obj {
        head: Header {
            isa: class,
            refs: AtomicUsize::new(1),
        },
        v,
    }))
}

/// `+alloc` for a class whose objects hold a `T`.
pub extern "C" fn alloc<T: Default>(class: *const Class, _: Sel) -> *mut Obj<T> {
    new_of(class, T::default())
}

/// `-dealloc` for a class whose objects hold a `T`.
pub unsafe extern "C" fn dealloc<T>(this: *mut Obj<T>, _: Sel) {
    // SAFETY: made by `new_of`, and this was its last reference.
    drop(unsafe { Box::from_raw(this) });
}

extern "C" fn init(this: *mut Header, _: Sel) -> *mut Header {
    this
}

unsafe extern "C" fn retain(this: *mut Header, _: Sel) -> *mut Header {
    // SAFETY: a live object.
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) };
    this
}

pub unsafe extern "C" fn release(this: *mut Header, _: Sel) {
    // SAFETY: a live object, which this reference kept alive.
    if unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } == 1 {
        unsafe { send0(this, "dealloc") };
    }
}

unsafe extern "C" fn responds_to_selector(this: *const Header, _: Sel, sel: Sel) -> bool {
    // SAFETY: a live object; `sel` is interned.
    unsafe { find(this, sel) }.is_some()
}

/// Labels are for Xcode's debugger and captures, which a guest has neither of.
extern "C" fn set_label(_: *mut Header, _: Sel, _: *const c_void) {}

unsafe extern "C" fn retain_count(this: *mut Header, _: Sel) -> usize {
    // SAFETY: a live object.
    unsafe { (*this).refs.load(Ordering::Relaxed) }
}

thread_local! {
    static POOLS: RefCell<Vec<Vec<*mut Header>>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" fn autorelease(this: *mut Header, _: Sel) -> *mut Header {
    let pooled = POOLS.with(|p| match p.borrow_mut().last_mut() {
        Some(pool) => {
            pool.push(this);
            true
        }
        None => false,
    });
    if !pooled {
        // As on Darwin, an object autoreleased outside any pool leaks.
        eprintln!("limina-metal: an object autoreleased with no pool in place leaks");
    }
    this
}

extern "C" fn pool_init(this: *mut Header, _: Sel) -> *mut Header {
    POOLS.with(|p| p.borrow_mut().push(Vec::new()));
    this
}

unsafe extern "C" fn pool_dealloc(this: *mut Obj<()>, s: Sel) {
    // Pools nest: this one is the innermost.
    let objects = POOLS.with(|p| p.borrow_mut().pop()).unwrap_or_default();
    for o in objects {
        // SAFETY: each was live when autoreleased, and the pool held that reference.
        unsafe { release(o, s) };
    }
    // SAFETY: as in `dealloc`.
    unsafe { dealloc(this, s) };
}

unsafe extern "C" fn pool_drain(this: *mut Header, s: Sel) {
    // SAFETY: a live pool.
    unsafe { release(this, s) };
}

pub fn root_defs() -> Vec<Def> {
    vec![
        Def {
            name: "NSObject",
            superclass: None,
            methods: vec![
                ("init", imp!(init)),
                ("retain", imp!(retain)),
                ("release", imp!(release)),
                ("autorelease", imp!(autorelease)),
                ("retainCount", imp!(retain_count)),
                ("respondsToSelector:", imp!(responds_to_selector)),
                ("setLabel:", imp!(set_label)),
                ("dealloc", imp!(dealloc::<()>)),
            ],
            class_methods: vec![
                ("alloc", imp!(alloc::<()>)),
                ("respondsToSelector:", imp!(responds_to_selector)),
            ],
        },
        Def {
            name: "NSAutoreleasePool",
            superclass: Some("NSObject"),
            methods: vec![
                ("init", imp!(pool_init)),
                ("drain", imp!(pool_drain)),
                ("dealloc", imp!(pool_dealloc)),
            ],
            class_methods: vec![("alloc", imp!(alloc::<()>))],
        },
    ]
}
