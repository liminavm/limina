//! The Foundation classes MLX's Metal backend touches: strings, file URLs and errors.

use std::ffi::{CStr, CString, c_char};

use crate::imp;
use crate::runtime::{Class, Def, Obj, Sel, alloc, dealloc, new};

pub type NSString = Obj<CString>;

/// A new autoreleased-by-the-caller string (one reference).
pub fn string(s: &str) -> *mut NSString {
    new("NSString", CString::new(s.replace('\0', "")).unwrap())
}

/// The text of `s`, or "" for nil.
pub fn text(s: *const NSString) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: a live NSString.
    unsafe { (*s).v.to_string_lossy().into_owned() }
}

unsafe extern "C" fn string_with_cstring(
    _: *const Class,
    _: Sel,
    c: *const c_char,
    _enc: usize,
) -> *mut NSString {
    // SAFETY: the caller passes a C string. Every encoding MLX passes (ASCII, UTF-8) reads as UTF-8.
    let s = unsafe { CStr::from_ptr(c) }.to_owned();
    let o = new("NSString", s);
    // Not a new/alloc/copy method: the caller does not own the result.
    unsafe { crate::runtime::send0(o.cast(), "autorelease") };
    o
}

unsafe extern "C" fn utf8(this: *const NSString, _: Sel) -> *const c_char {
    // SAFETY: a live NSString; the pointer lives as long as it.
    unsafe { (*this).v.as_ptr() }
}

unsafe extern "C" fn cstring_using(this: *const NSString, s: Sel, _enc: usize) -> *const c_char {
    // SAFETY: as in utf8.
    unsafe { utf8(this, s) }
}

unsafe extern "C" fn length(this: *const NSString, _: Sel) -> usize {
    // SAFETY: a live NSString.
    unsafe { (*this).v.as_bytes().len() }
}

#[derive(Default)]
pub struct Url {
    pub path: String,
}

unsafe extern "C" fn file_url_with_path(
    _: *const Class,
    _: Sel,
    path: *const NSString,
) -> *mut Obj<Url> {
    let o = new("NSURL", Url { path: text(path) });
    // SAFETY: just made.
    unsafe { crate::runtime::send0(o.cast(), "autorelease") };
    o
}

unsafe extern "C" fn url_path(this: *const Obj<Url>, _: Sel) -> *mut NSString {
    // SAFETY: a live NSURL.
    let o = string(unsafe { &(*this).v.path });
    unsafe { crate::runtime::send0(o.cast(), "autorelease") };
    o
}

#[derive(Default)]
pub struct Error {
    pub description: String,
    pub code: isize,
}

/// Stores a new autoreleased NSError in `*out`, when the caller asked for one.
pub fn fail(out: *mut *mut Obj<Error>, description: String) {
    eprintln!("limina-metal: {description}");
    if out.is_null() {
        return;
    }
    let e = new(
        "NSError",
        Error {
            description,
            code: 1,
        },
    );
    // SAFETY: just made; `out` is the caller's error slot.
    unsafe {
        crate::runtime::send0(e.cast(), "autorelease");
        *out = e;
    }
}

unsafe extern "C" fn localized_description(this: *const Obj<Error>, _: Sel) -> *mut NSString {
    // SAFETY: a live NSError.
    let o = string(unsafe { &(*this).v.description });
    unsafe { crate::runtime::send0(o.cast(), "autorelease") };
    o
}

unsafe extern "C" fn error_code(this: *const Obj<Error>, _: Sel) -> isize {
    // SAFETY: a live NSError.
    unsafe { (*this).v.code }
}

pub fn defs() -> Vec<Def> {
    vec![
        Def {
            name: "NSString",
            superclass: Some("NSObject"),
            methods: vec![
                ("UTF8String", imp!(utf8)),
                ("cStringUsingEncoding:", imp!(cstring_using)),
                ("length", imp!(length)),
                ("dealloc", imp!(dealloc::<CString>)),
            ],
            class_methods: vec![
                ("alloc", imp!(alloc::<CString>)),
                ("stringWithCString:encoding:", imp!(string_with_cstring)),
            ],
        },
        Def {
            name: "NSURL",
            superclass: Some("NSObject"),
            methods: vec![("path", imp!(url_path)), ("dealloc", imp!(dealloc::<Url>))],
            class_methods: vec![("fileURLWithPath:", imp!(file_url_with_path))],
        },
        Def {
            name: "NSError",
            superclass: Some("NSObject"),
            methods: vec![
                ("localizedDescription", imp!(localized_description)),
                ("code", imp!(error_code)),
                ("dealloc", imp!(dealloc::<Error>)),
            ],
            class_methods: vec![],
        },
    ]
}
