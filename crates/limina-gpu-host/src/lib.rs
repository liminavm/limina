//! The host half of limina's Metal-level GPU transport.
//!
//! - The **GPU host** ([`server`], the `limina-gpu-host` binary) is its own process, one per VM.
//!   It owns Metal. Guest-controlled input reaches Metal only here, so a crash is a lost device for
//!   the guest, not a dead VM.
//! - The **worker** drives it through [`client`]: it creates contexts and arenas on the guest
//!   kernel's behalf, maps arena pages (which the GPU host owns) into its own address space for the
//!   guest, and rings a context's doorbell when the guest says the ring's consumer parked.
//!
//! Arena pages survive a GPU host crash: the worker's mapping holds them
//! (`spikes/replayer-ipc/RESULTS.md`).
//!
//! Every thread here that calls Metal drains an autorelease pool per unit of work (a ring command,
//! a request). Metal autoreleases what it hands back, encoders included, and a thread without a
//! pool of its own keeps all of it until it exits: a ring thread lives as long as its context.
//! `heap(1)` on a test's GPU host (its pid is `GpuHost::pid`) names what piles up.

pub mod client;
mod exec;
pub mod ipc;
pub mod journal;
pub mod mach;
mod metal;
pub mod server;
