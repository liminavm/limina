# vmnet-necp-probe — which of our binaries can dial a vmnet guest's address

A host process that connects to a guest's vmnet address is subject to Local Network privacy (NECP);
our ad-hoc binaries are dropped with `EHOSTUNREACH` (`docs/design/multi-vm-networking.md` §3.3).
This probe asks whether a binary signed with a real identity gets through, from a shell and as a
launchd job (how limina's worker is started), before and after the user answers the Local Network
prompt. It decides whether a port forward on vmnet can simply dial the guest.

Run: boot a guest on `--net-vmnet shared`, then `run.sh <guest-ip> [port]`. It builds `probe.c`,
signs one copy ad-hoc, one with the Apple Development identity, and one with that identity and the
hardened runtime (`-o runtime`), and dials with each. Signing with the identity needs the login
keychain unlocked in the shell that signs: from an agent's shell it failed with
`errSecInternalComponent` until the user unlocked it.

## Measured 2026-10-19, macOS 27.0.1, guest sshd on 192.168.65.19:22

| Signature | Prompt unanswered | After "Allow" |
|---|---|---|
| ad-hoc | `EHOSTUNREACH`, shell and launchd | — (not allowed) |
| Apple Development | `EHOSTUNREACH`, shell and launchd | `EHOSTUNREACH`; see below |
| Apple Development + hardened runtime | `EHOSTUNREACH`, shell and launchd | **connects**, shell and launchd |

- **A real identity gets through once the user allows it**, and the grant applies to the binary
  run as a launchd job as well as from a shell. Until the prompt is answered, every signature is
  dropped alike.
- **Ad-hoc binaries raise the prompt too.** The first prompt, "Allow "probe-identity" to find
  devices on local networks?", came while that copy was still ad-hoc (its identity signing had
  failed). It was answered only after the copy had been re-signed with the identity, and the
  re-signed copy stayed dropped and raised no new prompt: a grant belongs to the code identity
  that raised the prompt, so it did not carry over. Whether a plain Apple Development signature
  without the hardened runtime passes is therefore not settled by this run.
- **What it means for limina:** a forward that dials the guest needs the dialing binary signed
  with a stable identity and a user's click before the first connection. Ad-hoc dev and test
  builds change identity on every build, so each would need a click of its own, and a headless
  run cannot give one.
