# ssh-staging-race — what a "ready" guest still refuses

Chasing `venus_clear_rect`'s single failure in the 3-wide nextest suite of 2026-09-23.
The test is a venus guard, but it never reached venus: it died at its **first
`ssh_exec`**, immediately after `wait_for_ssh_banner` had already returned a real banner.

```
    guest SSH up: SSH-2.0-OpenSSH_10.2
    thread '…' panicked at crates/limina-test/tests/venus_clear_rect.rs:77:10:
    staging vkfdcycle.py in the guest: ssh `cat > /tmp/vkfdcycle.py <<'…'
    …
    VKFDCYCLE_PY_EOF` failed (exit status: 255):

    note: run with `RUST_BACKTRACE=1` …
```

Exit 255, and **nothing after the colon** — an empty stderr where the reason should be.

## 1. What an empty stderr means (measured, not guessed)

The harness ran ssh with `-o LogLevel=ERROR`. Against synthetic listeners on this host
(OpenSSH 10.x, 2026-09-23 — `sshprobe.py` shapes: refuse / accept-then-close /
banner-then-close):

| what the peer did | exit | stderr at `LogLevel=ERROR` | at `INFO` |
| --- | --- | --- | --- |
| nothing listening | 255 | `ssh: connect to host … Connection refused` | same |
| accept, then close | 255 | *(empty)* | `Connection closed by 127.0.0.1 port N` |
| banner, then close | 255 | *(empty)* | `Connection reset by 127.0.0.1 port N` |
| bad key | 255 | `Permission denied (publickey,…)` | same |

So `LogLevel=ERROR` prints every connection-level failure **except** the one where the
connection is accepted and then dies mid-handshake. That is exactly the failure we hit,
and it is why the suite log carries a bare 255 with no reason.

**Therefore:** the forward *was* listening (not "Connection refused"), the key *was*
accepted-or-never-reached (not "Permission denied"), and something hung up between the
TCP accept and the end of the key exchange.

This is not a one-off blind spot. `suite3.log` (the serial fallback run earlier the same
day) contains 47 ssh-255 failures: 36 `Permission denied` (the synoik host-key gap, since
fixed), 1 `Connection refused`, and **10 with an empty stderr** — ten failures that
recorded nothing about themselves.

Fix applied: `crates/limina-test/src/lib.rs` now runs ssh and scp at `LogLevel=INFO`.
stderr is read only when the command fails, so the noisier level costs nothing on the
happy path — it only adds ssh's known-hosts warning to messages that were already errors.

## 2. The theory that died first: two VMs, one forwarded port

`Guest::boot` pre-allocates the SSH-forward port by binding `127.0.0.1:0`, reading the
port, and **closing the listener** before passing it to the supervisor as `--ssh-port`.
The supervisor re-checks with its own bind probe (`allocate_ssh_port`) and hands it to
gvproxy. Both checks are TOCTOU, so two concurrent tests can in principle agree on the
same port — and the harness's own comment records what that costs: a test that ssh'es
into a *bystander* VM with identical credentials, where every check "works" against the
wrong guest.

That would explain a banner arriving suspiciously early. It does not survive contact with
gvproxy (`gvbind.py`): with the port already held,

```
level=error msg="gvproxy exiting: cannot add network services: listen tcp 127.0.0.1:57771: bind: address already in use"
```

gvproxy **exits** rather than running on with a forward it never got. A VM that loses the
race has no gateway at all, so it cannot reach a bystander's guest — it gets no banner.
Our failing run had a banner, so its gvproxy owned its port. Cross-wiring is ruled out.

## 3. What is left: the banner is a weaker oracle than the harness assumes

`Guest::wait_for_ssh_banner` — and `scripts/wait-guest-ssh.sh`, which uses the identical
rule — declares the guest ready when a read of the forward starts with `SSH-`. Every
networked test issues its first real `ssh` immediately afterwards. A banner proves gvproxy
dialed the guest and *something* answered; it does not prove a session can be established.

Reproduction by repetition is expensive: `repro.sh`, running the real test 3 copies at a
time (the suite's own width), went **120 boots with no failure**, against a suite rate of
roughly one in fifty networked boots.

So `window-probe.py` measures the window instead of waiting to land in it. Per boot it
records `T_banner` (first successful banner read) and `T_session` (first successful
`ssh true`), with optional co-resident guests for load. Every boot yields a number.
The harness polls the banner every 500 ms and ssh's immediately after, so it fires its
first command somewhere in `[T_banner, T_banner + 0.5s]`: a window near zero is safe, and
a window past half a second is a failure it cannot avoid.

### The reproduction

`repro.sh` did eventually land in it, on its 23rd iteration of 40 — 67 boots in — and with
ssh no longer muzzled, the guest said exactly what was wrong:

```
VKFDCYCLE_PY_EOF` failed (exit status: 255):
Warning: Permanently added '[127.0.0.1]:49628' (ED25519) to the list of known hosts.
"System is booting up. Unprivileged users are not permitted to log in yet.
 Please come back later. For technical details, see pam_nologin(8)."
Connection closed by 127.0.0.1 port 49628
```

**`pam_nologin`.** systemd creates `/run/nologin` early in boot and
`systemd-user-sessions.service` removes it when the system is ready for logins. Nothing
orders sshd after that service, so sshd listens — and answers banners — while every
unprivileged login is still refused at PAM's account stage.

### The window

`window-probe.py`, 8 boots with 2 co-resident guests, `T_banner` = first successful banner
read, `T_session` = first successful `ssh true`:

| boot | T_banner | T_session | window |
| ---: | ---: | ---: | ---: |
| 0 | 3.21 s | 3.21 s | 0.00 s |
| 1 | 3.21 s | 4.35 s | **1.14 s** |
| 2 | 3.22 s | 4.26 s | **1.05 s** |
| 3 | 3.21 s | 4.21 s | **1.00 s** |
| 4 | 4.81 s | 4.81 s | 0.00 s |
| 5 | 3.21 s | 3.21 s | 0.00 s |
| 6 | 4.82 s | 4.82 s | 0.00 s |
| 7 | 3.21 s | 3.21 s | 0.00 s |

So the gap is real on **three boots in eight**, about a second wide. It is a race inside
the guest's own boot — whether `systemd-user-sessions` gets there before sshd does — not
something the host causes, which is why host load only changes the odds.

### Why a one-second hole only costs a test every ~120 boots

Because the harness is slower to notice the banner than it looks. `wait_for_ssh` polls
every 500 ms, but each poll opens a connection with a 2-second read timeout, and before
sshd is reachable gvproxy accepts and then goes quiet — so an early poll *blocks for two
seconds* before failing. The effective cadence is ~2.5 s, not 0.5 s, and the first
banner-positive poll usually lands well past the far edge of a 1 s window. Occasionally
the phase lines up and it lands inside. One in a hundred-odd boots, which is exactly the
rate the suite shows.

## 4. The fix

The oracle was wrong, so the oracle changed — in both waiters, because
`scripts/wait-guest-ssh.sh` had the identical rule and CLAUDE.md named it THE way to wait
for a networked boot:

- `Guest::wait_for_ssh_banner` → **`Guest::wait_for_ssh`** (80 call sites across 60 test files), now two-stage:
  the banner, and then `ssh … true` until a session actually establishes. It still returns
  the banner, so it still reads as the end-to-end NAT proof. Costs ~1 s on the boots that
  need it and nothing on the rest.
- `scripts/wait-guest-ssh.sh` gained the same second stage (`WAIT_SSH_USER=` opts out, for
  a guest this host has no credentials on).
- The harness's ssh and scp now share one option list at `LogLevel=INFO`.

RED first: `a_banner_alone_is_not_readiness` drives the wait against a fake sshd that
greets and hangs up — the guest's exact state while `/run/nologin` exists. Against the
pre-fix code it fails with `a guest that only answers banners must NOT count as ready:
"SSH-2.0-OpenSSH_10.2"`; against the fix it passes. It needs no VM, so it runs in plain
`cargo test`. `scripts/tests/wait-guest-ssh-stages.sh` does the same for the shell waiter,
fakes and all, and the pre-commit hook now runs everything in `scripts/tests/`.

Those tests, not a boot count, are what says this is fixed: 36 boots of the real test
passed afterwards, but at a base rate near one in a hundred-odd that shows only that
nothing regressed.

## What this does NOT explain

`l2_qga_fstrim`, the other failure in the same suite. It fails solo too, its ssh works
fine, and its gap is a host-allocation measurement — a separate chase
(`spikes/qga-fstrim/`).
