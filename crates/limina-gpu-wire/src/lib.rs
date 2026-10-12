//! The guest↔replayer protocol of limina's Metal-level GPU transport.
//!
//! The design is in `spikes/metal-heap-addressing/SKETCH.md`; this crate holds the parts both ends
//! must agree on byte for byte:
//!
//! - [`ring`]: a single-producer, single-consumer byte ring that lives in arena memory. The guest
//!   writes commands and advances `tail`; the replayer copies each command out before it looks at
//!   it, executes it and advances `head`. A doorbell is needed only when the replayer has parked,
//!   which the ring's control block says.
//! - [`cmd`]: the commands. They follow Metal's object model (placed resources, libraries,
//!   pipelines, encoders, commits) and carry Metal's own enum values, so the replayer translates
//!   nothing. Objects are named by handles the guest assigns; nothing a command creates is ever
//!   sent back.
//!
//! Buffers have no objects at all: a buffer is a range of an arena, named by [`cmd::BufRef`], and
//! its GPU address is the arena's base plus the offset.
#![no_std]

pub mod cmd;
pub mod ring;

#[cfg(test)]
mod c_header;

/// Protocol version. Both ends refuse a ring whose control block names another one.
pub const VERSION: u32 = 3;
