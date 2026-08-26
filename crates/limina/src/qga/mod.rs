// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host end of the stock **`qemu-guest-agent`** port, `org.qemu.guest_agent.0`.
//!
//! Same shape as [`crate::vdagent`], and for the same reason: the agent is already
//! installed in a stock guest and only waits for the port. Fedora's
//! `guest-desktop-agents` comps group makes `qemu-guest-agent` **mandatory** in every
//! desktop variant (Ubuntu desktop ships it too), and its unit is
//! `BindsTo=dev-virtio\x2dports-org.qemu.guest_agent.0.device`, started by
//! `99-qemu-guest-agent.rules` matching `ATTR{name}=="org.qemu.guest_agent.0"`. Expose the
//! port and a guest with **nothing of ours installed** gains the features below; expose
//! nothing and the agent stays dormant, which is what it did here until now.
//!
//! Verified against upstream source (2026-08-26), not memory — `qemu/qga/qapi-schema.json`
//! and `qemu/qga/main.c`, plus Fedora's `qemu-guest-agent.service` / `qemu-ga.sysconfig`:
//!
//! - **Line-delimited JSON, request/response, one client, no unsolicited messages.** There
//!   is nothing to receive that we did not ask for, so unlike the vdagent broker this has
//!   no reader thread — one mutex serializes the single outstanding call.
//! - **`guest-sync-delimited` is the only resync.** Its reply is prefixed with a `0xFF`
//!   sentinel byte (`main.c:672-675`), so a client can discard a stale or half-read stream
//!   deterministically. We send a leading `0xFF` of our own with it: the agent feeds bytes
//!   straight to a JSON parser (`main.c:728`), and that invalid byte is what kicks the
//!   parser out of a partial message left by a previous client.
//! - **Some commands answer nothing on success** (`guest-shutdown`, `guest-suspend-*` carry
//!   `'success-response': false`), which is why [`client::Qga::fire`] exists next to
//!   [`client::Qga::call`].
//! - **The guest's own config is wide open.** Fedora ships `/etc/sysconfig/qemu-ga` with the
//!   `--block-rpcs` line commented out, so every RPC — including `guest-exec` and the
//!   `guest-file-*` family, as root — is reachable. Only the supervisor process holds the
//!   host end of the port.
//!
//! What limina uses it for today is the **guest clock** ([`policy`]): a stock guest that
//! ignores the RTC, or that stays "running" across a host nap, has no other correction path
//! — PL031 injection needs the guest kernel to consult it, and our own `TimeSync` needs
//! `limina-agent`. The enhanced tier keeps winning: the fallback only runs when no
//! `timesync`-capable peer took the message (`crate::control`).

pub mod client;
pub mod codec;
pub mod policy;
