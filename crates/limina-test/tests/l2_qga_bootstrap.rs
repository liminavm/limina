// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **bootstrapping the enhanced tier into a guest over the stock agent's port**.
//!
//! The two-tier guarantee says a fresh install *starts* stock and the enhanced components
//! are delivered into it from there — so the delivery mechanism has to run before any of
//! them exist, and must not depend on the things it installs. Until now that meant SSH: a
//! network, a listening `sshd`, an account and a key. The stock `qemu-guest-agent` needs
//! none of those. It is already installed on every Fedora desktop variant, it is root, and
//! `guest-file-write` + `guest-exec` are all a bootstrap actually needs.
//!
//! This test delivers `limina-agent` — the enhanced tier's own front door — through that
//! port and nothing else, then waits for it to call home on the control plane.
//!
//! # What it does not cover
//!
//! A confined agent. `guest-exec` runs its children in `qemu-ga`'s own SELinux domain, and
//! Fedora's default `virt_qemu_ga_t` cannot write a system path or start a unit — so this
//! test unconfines the domain in setup, and Enforcing Fedora guests keep SSH as their
//! delivery path. See `crates/limina/src/qga/bootstrap.rs` §Where it works.
//!
//! # Why it removes the agent first
//!
//! The suite's image is the *enhanced* one: `limina-agent` is already installed and already
//! connected. A test that merely deployed over it and watched for a reconnect would go green
//! with the delivery completely broken — the unit would restart the binary that was already
//! there. That is the half-installed-fix trap, and the only structural cure is to make the
//! old artifact **gone**: the unit is disabled, the binary and the unit file are deleted, and
//! the peer is polled until it disappears. After that, a `limina-agent` peer can only exist
//! if the bytes we pushed became a running program.
//!
//! The hash check on top of that is what separates *delivered* from *delivered intact*: a
//! truncated 470 KiB binary that still happens to execute is not a passing bootstrap.
//!
//! # The grace window, and the guard on it
//!
//! The supervisor does not deploy into a guest that already runs `limina-agent` — a bootstrap
//! kit is for a guest that has none. It therefore waits `LIMINA_QGA_DEPLOY_AFTER` seconds
//! after the port answers and looks for an enhanced peer before doing anything. This test's
//! whole setup has to finish inside that window, so if it overruns, the deploy correctly
//! skips and every assertion below would fail for a reason that has nothing to do with the
//! code under test. Oracle 2 names that case explicitly instead.
//!
//! Oracles, in order:
//! 1. The agent really is gone (binary absent, peer gone from the control plane).
//! 2. The supervisor deployed rather than skipping, and its `install.sh` exited 0.
//! 3. The delivered binary is byte-identical to the host's, by sha256.
//! 4. A `limina-agent` peer connected **after** the teardown — the enhanced tier is back,
//!    and it can only have come from what we pushed.
//! 5. A kit that carries `authorized_keys.<user>` gets those keys installed through
//!    `guest-ssh-add-authorized-keys` — the other half of a bootstrap, since a guest with
//!    no key is a guest nobody can reach.

use limina_test::{repo_root, Guest, GuestConfig};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the supervisor waits for an existing `limina-agent` before deciding the guest
/// needs a bootstrap. Long enough here that boot, SSH and the teardown below fit inside it.
const GRACE_SECS: &str = "150";

/// Where the kit lands in the guest. `/var/tmp` survives a `systemd-tmpfiles` sweep mid-run,
/// which `/tmp` does not.
const STAGE: &str = "/var/tmp/limina-bootstrap";

/// Long enough for the whole chain — grace, transfer, `install.sh`, the unit starting, the
/// agent's vsock connect — plus room for a loaded host.
const DEPLOY_WITHIN: Duration = Duration::from_secs(240);

/// The guest account the harness logs in as, and so the one whose `authorized_keys` the kit
/// targets.
const GUEST_USER: &str = "claude";

/// A throwaway key, only ever added to the guest's `authorized_keys` so oracle 5 can see it
/// arrive. It authorizes nothing: no private half exists.
const TEST_PUBKEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBogusKeyForTheQgaBootstrapL2TestOnly limina-l2";

/// Run a guest command over SSH, tolerating a non-zero exit and a transient failure.
///
/// SSH is the **verifier** here, never the delivery path: everything this test asserts about
/// was put there by the guest agent port.
fn ssh_soft(guest: &Guest, cmd: &str) -> String {
    let wrapped = format!("{{ {cmd} ; }} 2>&1 || true");
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec_timeout(&wrapped, Duration::from_secs(120)) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// How many times a `limina-agent` peer has completed the control-plane handshake.
fn agent_connections(guest: &Guest) -> usize {
    count(guest, "control: guest agent connected: limina-agent/")
}

/// …and how many of those have gone away again. Equal counts mean no `limina-agent` peer is
/// connected right now, which is the state oracle 1 has to reach before anything else can be
/// attributed to the bootstrap.
///
/// The trailing `/` in both needles separates the system agent from `limina-agent-session`,
/// the per-user helper that shares its prefix and keeps its own connection. Counting that
/// one as the agent is what made an earlier run see a reconnect that never happened.
fn agent_disconnections(guest: &Guest) -> usize {
    count(guest, "control: guest agent disconnected: limina-agent/")
}

fn count(guest: &Guest, needle: &str) -> usize {
    guest
        .supervisor_log()
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

/// The supervisor's `qga:` and peer lines, for a failure message. Both, because most ways
/// this test fails are about the order the two interleaved in.
fn qga_lines(guest: &Guest) -> String {
    let log = guest.supervisor_log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("qga:") || l.contains("control: guest agent"))
        .collect();
    if lines.is_empty() {
        "(the supervisor logged nothing about the guest agent)".to_string()
    } else {
        lines.join("\n")
    }
}

/// Assemble the bootstrap kit the supervisor will push: the agent, its unit, and the script
/// that installs them. Deliberately the same three pieces `scripts/install-guest-agent.sh`
/// copies over SSH — the point is that the transport changed, not the payload.
fn build_kit() -> Result<PathBuf, String> {
    let root = repo_root();
    let agent = root.join("guest/target/aarch64-unknown-linux-musl/release/limina-agent");
    if !agent.exists() {
        return Err(format!(
            "no guest agent built at {} — run `cd guest && cargo build --release -p limina-agent`",
            agent.display()
        ));
    }
    let unit = root.join("guest/limina-agent/limina-agent.service");
    if !unit.exists() {
        return Err(format!("the agent's unit is missing at {}", unit.display()));
    }

    let kit = root.join(format!("target/qga-kit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&kit);
    std::fs::create_dir_all(&kit).map_err(|e| format!("creating {}: {e}", kit.display()))?;
    for src in [&agent, &unit] {
        let dst = kit.join(src.file_name().unwrap());
        std::fs::copy(src, &dst).map_err(|e| format!("copying {}: {e}", src.display()))?;
    }
    // `restorecon` is not optional politeness: a file the guest agent creates does not carry
    // `bin_t`, and an SELinux-Enforcing guest refuses to execute it from a system path.
    std::fs::write(
        kit.join("install.sh"),
        "#!/bin/sh\n\
         set -eu\n\
         d=$(dirname \"$0\")\n\
         install -m0755 \"$d/limina-agent\" /usr/local/bin/limina-agent\n\
         install -m0644 \"$d/limina-agent.service\" /etc/systemd/system/limina-agent.service\n\
         restorecon -F /usr/local/bin/limina-agent /etc/systemd/system/limina-agent.service || true\n\
         systemctl daemon-reload\n\
         systemctl enable --now limina-agent\n",
    )
    .map_err(|e| format!("writing install.sh: {e}"))?;

    // A kit may also carry keys for an account. The agent's own verb places them, so the
    // file lands with the ownership, mode and SELinux label sshd insists on — none of which
    // a raw `guest-file-write` into `~/.ssh` would get right.
    std::fs::write(
        kit.join(format!("authorized_keys.{GUEST_USER}")),
        format!("{TEST_PUBKEY}\n"),
    )
    .map_err(|e| format!("writing the kit's authorized_keys: {e}"))?;
    Ok(kit)
}

/// sha256 of a host file, as the guest's `sha256sum` would print it.
fn host_sha256(path: &Path) -> String {
    let out = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("hashing {}: {e}", path.display()));
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_bootstrap_kit_installs_the_enhanced_agent_through_the_guest_agent_port() {
    let name = "a_bootstrap_kit_installs_the_enhanced_agent_through_the_guest_agent_port";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let kit = match build_kit() {
        Ok(kit) => kit,
        Err(e) => {
            eprintln!("SKIPPED {name}: {e}");
            return;
        }
    };
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg
            // gdm dies without a display device and systemd restarts it forever; that noise
            // competes with the transfer this test is timing.
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_QGA_DEPLOY", &kit.display().to_string())
            .with_env("LIMINA_QGA_DEPLOY_AFTER", GRACE_SECS)
            .with_env("LIMINA_QGA_TRACE", "1")
            .with_env("RUST_LOG", "limina=info,limina::qga=debug"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            let _ = std::fs::remove_dir_all(&kit);
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    let rpm = ssh_soft(&guest, "rpm -q qemu-guest-agent");
    if !rpm.contains("qemu-guest-agent-") {
        eprintln!(
            "SKIPPED {name}: this image has no qemu-guest-agent ({rpm}) — an image property, \
             not a failure."
        );
        guest.shutdown(Duration::from_secs(60)).ok();
        let _ = std::fs::remove_dir_all(&kit);
        return;
    }

    // Unconfine the agent, which is the guest this test is about.
    //
    // `guest-exec` runs its children as `virt_qemu_ga_t`. On a stock Enforcing Fedora that
    // domain may not write `bin_t`, may not reach systemd's D-Bus, and may not touch
    // `user_home_t` (`guest-ssh-add-authorized-keys` has its own boolean) — so a bootstrap
    // through the port cannot install anything there, and SSH is that guest's delivery path.
    // The kit mechanism is scoped to an unconfined agent, and these two lines are what put
    // the guest in that scope; without them the test would measure Fedora's policy instead.
    let policy = ssh_soft(
        &guest,
        "sudo setsebool virt_qemu_ga_manage_ssh=on 2>&1; \
         sudo semanage permissive -a virt_qemu_ga_t 2>&1",
    );
    eprintln!("guest policy: unconfined virt_qemu_ga_t {policy}");

    // --- Oracle 1: take the enhanced tier away ---
    //
    // Not just stopped: *deleted*. A stopped unit restarting the binary that was already
    // there is exactly the false green this whole file is shaped to avoid.
    let torn = ssh_soft(
        &guest,
        "sudo systemctl disable --now limina-agent; \
         sudo rm -f /usr/local/bin/limina-agent /etc/systemd/system/limina-agent.service; \
         sudo systemctl daemon-reload; \
         ls /usr/local/bin/limina-agent 2>&1",
    );
    assert!(
        torn.contains("No such file"),
        "could not remove the installed agent, so a reconnect below would prove nothing: {torn}"
    );

    // Wait for the control plane to agree the peer is gone, and only THEN take the baseline.
    // Sampling it before the teardown made this test pass its wait loop on the dying agent's
    // own last reconnect — the count has to be frozen at a moment when nothing can add to it
    // but the bootstrap.
    let gone = Instant::now() + Duration::from_secs(60);
    while agent_disconnections(&guest) < agent_connections(&guest) && Instant::now() < gone {
        std::thread::sleep(Duration::from_secs(2));
    }
    assert_eq!(
        agent_disconnections(&guest),
        agent_connections(&guest),
        "a limina-agent peer is still connected after the binary and unit were deleted, so \
         the reconnect oracle below cannot distinguish it from a bootstrapped one"
    );
    let before = agent_connections(&guest);
    eprintln!("enhanced agent removed and its peer gone; baseline {before} connection(s)");

    // --- Oracle 2: the supervisor deployed, and the kit's installer succeeded ---
    //
    // Waited on the supervisor's own account of what it did, not on the peer count: the
    // count is oracle 4's business, and using it here made a stray reconnect end the wait
    // before the deploy had had its grace.
    let deadline = Instant::now() + DEPLOY_WITHIN;
    let done = |g: &Guest| {
        let log = g.supervisor_log();
        log.contains("qga: bootstrap kit installed")
            || log.contains("qga: the guest already runs limina-agent")
            || log.contains("qga: the bootstrap kit did not install")
    };
    while !done(&guest) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(5));
    }
    let log = guest.supervisor_log();
    assert!(
        !log.contains("qga: the guest already runs limina-agent"),
        "the supervisor skipped the bootstrap because a peer was still connected when the \
         {GRACE_SECS}s grace expired — setup overran the window, so nothing below can be \
         measured. Raise GRACE_SECS.\n{}",
        qga_lines(&guest)
    );
    assert!(
        log.contains("qga: bootstrap kit installed"),
        "the supervisor never finished deploying the kit within {DEPLOY_WITHIN:?}. Either the \
         deploy thread is not wired (crates/limina/src/qga/bootstrap.rs), the agent blocks \
         guest-file-write / guest-exec, or install.sh failed.\n{}",
        qga_lines(&guest)
    );

    // --- Oracle 3: the bytes arrived intact ---
    let want = host_sha256(&kit.join("limina-agent"));
    let got = ssh_soft(&guest, "sha256sum /usr/local/bin/limina-agent");
    assert!(
        got.starts_with(&want),
        "the delivered agent does not match the host's ({want}); the guest says: {got}. A \
         binary that executes is not the same as a binary that arrived whole — check the \
         chunking in qga::client::write_file"
    );
    eprintln!("delivered agent matches the host's ({want})");

    // --- Oracle 4: the enhanced tier came back, from what we pushed ---
    //
    // `install.sh` ends at `systemctl enable --now`, which returns when the unit is started,
    // not when the agent inside it has finished its vsock handshake.
    let up = Instant::now() + Duration::from_secs(60);
    while agent_connections(&guest) == before && Instant::now() < up {
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(
        agent_connections(&guest) > before,
        "the agent was installed but never reached the control plane (still {before} \
         connection(s)). It is running from a binary this test delivered, so this is the \
         agent's own startup, not the transfer.\n{}",
        qga_lines(&guest)
    );
    eprintln!(
        "limina-agent reconnected: {} total",
        agent_connections(&guest)
    );

    // --- Oracle 5: the other provisioning verb ---
    //
    // Cheap to check here and worth having: key injection is what makes a guest reachable in
    // the first place, on an image that never had a key.
    let keys = ssh_soft(&guest, "cat ~/.ssh/authorized_keys");
    assert!(
        keys.contains("limina-l2"),
        "guest-ssh-add-authorized-keys did not reach authorized_keys (policy said: \
         {policy}); it holds:\n{keys}\n{}",
        qga_lines(&guest)
    );

    // The staging dir is the supervisor's, not the guest's to keep.
    let leftover = ssh_soft(&guest, &format!("ls {STAGE} 2>&1"));
    assert!(
        leftover.contains("No such file"),
        "the bootstrap left its staging dir behind at {STAGE}: {leftover}"
    );

    guest
        .shutdown(Duration::from_secs(90))
        .expect("the guest did not power off");
    let _ = std::fs::remove_dir_all(&kit);
}
