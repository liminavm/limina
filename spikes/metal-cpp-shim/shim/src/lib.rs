//! limina-metal (spike): the slice of the Objective-C runtime that Apple's metal-cpp headers
//! reach, and Metal's compute classes behind it, over limina's Metal-level transport. A program
//! built against metal-cpp, unmodified, runs on the host's GPU from a Linux guest. See RESULTS.md.

// The unsafe functions here are the Objective-C runtime's and Metal's C entry points: their
// contract is the runtime's, not one this crate states.
#![allow(clippy::missing_safety_doc)]

mod blocks;
mod foundation;
mod metal;
pub mod runtime;

/// Every class this library defines, superclasses first.
fn defs() -> Vec<runtime::Def> {
    let mut d = runtime::root_defs();
    d.extend(foundation::defs());
    d.extend(metal::defs());
    d
}
