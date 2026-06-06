# M3 gvproxy NAT spike — RESULTS

**Date:** 2026-06-06 · **Host:** M1 Max, macOS 26.5 · gvproxy v0.8.8 (`/opt/homebrew/bin`)
**Verdict: PASS — user-mode NAT works end-to-end with no libkrun patches.**

## What was tested

Drove `limina-vmm` **directly** (the supervisor doesn't spawn gvproxy yet) with the new
`--net-gvproxy <socket>` flag, against a cow-cloned stock `Fedora-Workstation-43.raw`,
headless (no display), silent EFI firmware. gvproxy launched from the script with
`-debug -listen-vfkit unixgram://<abs-socket>`. Oracle is **host-side gvproxy `-debug`
logs** (stock Fedora is silent on serial after GRUB — no `console=` — so the guest console
can't be the oracle). See `run.sh`; artifacts under `out/` (gitignored).

## Evidence (read the packets, then the conclusion)

- **Handshake.** Worker `unixgram` backend opened the socket and exchanged frames both ways
  (`Written/Read eth frame to/from proxy`, 89 frames guest→proxy). The `VFKT` magic
  (`UnixgramPath(path, true)`) was accepted — gvproxy never errored on the client.
- **DHCP — full Discover→Offer→Request→Ack.** gvproxy leased the guest
  `YourClientIP=192.168.127.3`, `SubnetMask 255.255.255.0`, `Router 192.168.127.1`,
  `DNS 192.168.127.1`, `MTU 1500`, `LeaseTime 3600`. `ClientHWAddr=02:67:6b:76:6d:01` — our
  fixed `NET_GUEST_MAC`. (Fedora's NetworkManager did the DHCP; `KRUN_DHCP`/`dhcp_client` is
  irrelevant on the stock-distro path, as expected.)
- **DNS resolution.** Outbound A queries to `192.168.127.1:53` returned real answers
  (`QR=true … ResponseCode=No Error … ANCount=9` / `ANCount=4`).
- **Outbound NAT to the real internet.** Guest `192.168.127.3` sent TCP to external hosts —
  e.g. `DstIP=140.211.169.196 DstPort=80(http)` (Fedora connectivity check / mirror),
  plus other real mirror IPs (`143.107.229.210`, `168.181.126.28`, …). UDP/NTP too.
- **No errors.** No worker FATAL/HANG-UP; no gvproxy errors. (`dropping spoofing packets
  from the gateway about IP 192.168.127.3` is benign gvproxy ARP debug chatter, not a drop
  of guest traffic.)

## Findings that shape the productization

1. **No libkrun patch needed for NAT.** The stock `unixgram.rs` + `UnixgramPath(_, true)`
   speaks gvproxy's vfkit dialect directly. (The roadmap's optional `worker.rs`
   reconnect-on-HANG_UP patch is still worth doing so a gvproxy restart doesn't need a VM
   restart — deferred until we have supervision.)
2. **gvproxy socket path must be ABSOLUTE.** `gvproxy -listen-vfkit unixgram://<path>` parses
   `unixgram://host/path`; a relative path's first component is mistaken for the URL host
   (`bind: no such file or directory`). Always pass `unixgram:///abs/path`.
3. **gvproxy defaults are sufficient.** `-listen-vfkit` alone (no JSON config, no `-listen`
   control endpoint) gives the standard krunkit network: `192.168.127.0/24`, gateway/DNS
   `.1`, dynamic DHCP. No extra flags required for basic NAT.
4. **Ordering.** The backend connects at guest **device activation** (net `worker.rs:64`),
   not at `build_resources` — so the supervisor only needs gvproxy listening before the
   guest activates the NIC (seconds of slack after kernel boot). Waiting for the socket file
   to exist before spawning the worker is enough.

## Next (productization — task #17)

Supervisor (`limina`) spawns + supervises gvproxy (`-listen-vfkit unixgram://<abs>`), waits
for the socket, passes `--net-gvproxy` to the worker, restarts gvproxy on crash, cleans up
the socket on teardown. Then an L2 networking test using the same host-side oracle.
