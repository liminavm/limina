// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The stock-tier vTPM (docs/design/vtpm.md, P2): a stock Fedora guest with no limina components
//! binds the TIS device with its in-tree `tpm_tis` driver, every consumer P0 measured
//! (spikes/vtpm-p0/consumers) works against janus, and what they sealed survives a guest reboot,
//! which starts a new worker that restores the TPM from its state file.
//!
//! Oracles, per consumer: a secret sealed by the TPM decrypts back to itself, a LUKS volume
//! enrolled to it attaches, a key that lives in it signs. After the reboot the same secrets and
//! volume open again, which a new TPM could not do: its seeds would differ.
//!
//! The image is the stock test image with the consumer packages installed
//! (`scripts/provision/make-tpm-test-image.sh`); the test adds its user to `tss` in its own
//! clone, as a user of a stock image would.
//!
//! Measured boot (07): on both boots, the firmware's event log must replay to the PCRs the TPM
//! reports, every PCR the log covers, which needs our firmware's TPM2 support. That makes the
//! PCR-7 enrolment above a real measured-boot policy, which the reboot then has to satisfy.
//!
//! Suspend (P4): a guest suspended with the suspend bracket and restored by a new worker finds
//! its TPM running as it left it: an extended PCR, a secret sealed to it, a loaded key and a
//! saved session, which a TPM rebuilt from its state file could not have.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

/// A consumer step can generate RSA keys, which the debug worker's engine takes seconds over.
const STEP: Duration = Duration::from_secs(300);

fn run(guest: &Guest, what: &str, script: &str) -> String {
    let script = format!("set -euo pipefail\n{script}");
    guest
        .ssh_exec_timeout(
            &format!("bash -c '{}'", script.replace('\'', r"'\''")),
            STEP,
        )
        .unwrap_or_else(|e| panic!("{what}: {e:#}"))
}

fn boot_id(guest: &Guest) -> String {
    run(guest, "boot id", "cat /proc/sys/kernel/random/boot_id")
        .trim()
        .to_string()
}

/// Seals what has to come back after the reboot, then runs every consumer once.
fn consumers(guest: &Guest) {
    let dmesg = run(guest, "dmesg", "sudo dmesg");
    assert!(
        dmesg.contains("tpm_tis") && dmesg.contains("2.0 TPM"),
        "the guest did not bind the TPM:\n{dmesg}"
    );
    // A new ssh login per step, so the group applies from the next one.
    run(guest, "tss group", "sudo usermod -aG tss claude");

    // 01: systemd's SRK, made persistent.
    run(
        guest,
        "systemd-tpm2-setup",
        "sudo /usr/lib/systemd/systemd-tpm2-setup",
    );
    let handles = run(
        guest,
        "persistent handles",
        "sudo tpm2_getcap handles-persistent",
    );
    assert!(handles.contains("0x81000001"), "no SRK:\n{handles}");

    // 02: systemd-creds, sealed with the TPM, with a PCR binding, with the host key, as a user.
    let creds = run(
        guest,
        "systemd-creds",
        r#"cd "$(mktemp -d)"
echo -n s3cret-tpm2 | sudo systemd-creds encrypt --with-key=tpm2 --name=t1 - t1.cred
sudo systemd-creds decrypt --name=t1 t1.cred -; echo
echo -n s3cret-pcr7 | sudo systemd-creds encrypt --with-key=tpm2 --tpm2-pcrs=7 --name=t2 - t2.cred
sudo systemd-creds decrypt --name=t2 t2.cred -; echo
echo -n s3cret-host | sudo systemd-creds encrypt --with-key=host+tpm2 --name=t3 - t3.cred
sudo systemd-creds decrypt --name=t3 t3.cred -; echo
echo -n s3cret-user | systemd-creds --user encrypt --name=t4 - t4.cred
systemd-creds --user decrypt --name=t4 t4.cred -; echo"#,
    );
    assert_eq!(
        creds.lines().collect::<Vec<_>>(),
        ["s3cret-tpm2", "s3cret-pcr7", "s3cret-host", "s3cret-user"]
    );

    // 03: a LUKS2 volume enrolled to PCR 7, then re-enrolled with a PIN; kept for the reboot.
    run(
        guest,
        "systemd-cryptenroll",
        r#"img=/var/tmp/l2-tpm-luks.img
sudo rm -f $img; sudo truncate -s 64M $img
echo -n passphrase | sudo cryptsetup luksFormat --type luks2 --batch-mode --pbkdf pbkdf2 --pbkdf-force-iterations 1000 $img -
sudo PASSWORD=passphrase systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 $img
sudo /usr/lib/systemd/systemd-cryptsetup attach l2tpm $img - tpm2-device=auto,headless=1
test -b /dev/mapper/l2tpm
sudo /usr/lib/systemd/systemd-cryptsetup detach l2tpm
sudo PASSWORD=passphrase NEWPIN=4321 systemd-cryptenroll --wipe-slot=tpm2 --tpm2-device=auto --tpm2-pcrs=7 --tpm2-with-pin=yes $img
sudo PIN=4321 /usr/lib/systemd/systemd-cryptsetup attach l2tpm $img - tpm2-device=auto,headless=1
test -b /dev/mapper/l2tpm
sudo /usr/lib/systemd/systemd-cryptsetup detach l2tpm"#,
    );

    // 04: clevis's tpm2 pin, unbound and bound to PCR 7.
    let clevis = run(
        guest,
        "clevis",
        r#"echo -n clevis-plain | clevis encrypt tpm2 "{}" | clevis decrypt; echo
echo -n clevis-pcr7 | clevis encrypt tpm2 "{\"pcr_ids\":\"7\"}" | clevis decrypt; echo"#,
    );
    assert_eq!(
        clevis.lines().collect::<Vec<_>>(),
        ["clevis-plain", "clevis-pcr7"]
    );

    // 05: ssh-tpm-agent, an ECDSA and an RSA key in the TPM, signing through the agent; the
    // signatures must verify against the keys' public halves.
    run(
        guest,
        "ssh-tpm-agent",
        r#"d=$(mktemp -d); cd $d
ssh-tpm-keygen -t ecdsa -N "" -f $d/id_ecdsa
ssh-tpm-keygen -t rsa -N "" -f $d/id_rsa
ssh-tpm-agent -l $d/agent.sock > $d/agent.log 2>&1 &
apid=$!
trap "kill $apid" EXIT
for i in $(seq 100); do [ -S $d/agent.sock ] && break; sleep 0.1; done
export SSH_AUTH_SOCK=$d/agent.sock SSH_TPM_AUTH_SOCK=$d/agent.sock
ssh-tpm-add $d/id_ecdsa.tpm
ssh-tpm-add $d/id_rsa.tpm
echo data > data
for k in ecdsa rsa; do
  ssh-keygen -Y sign -f $d/id_$k.pub -n file data
  echo "l2 $(cat $d/id_$k.pub)" > signers
  ssh-keygen -Y verify -f signers -I l2 -n file -s data.sig < data
  rm data.sig
done"#,
    );

    // 06: tpm2-pkcs11, a token with an ECC and an RSA key; ECDSA through the module (RSA
    // signing through it fails on swtpm too, P0 RESULTS.md).
    run(
        guest,
        "tpm2-pkcs11",
        r#"export TPM2_PKCS11_STORE=$(mktemp -d)
tpm2_ptool init --path=$TPM2_PKCS11_STORE
tpm2_ptool addtoken --pid=1 --label=l2 --sopin=sopin --userpin=userpin --path=$TPM2_PKCS11_STORE
tpm2_ptool addkey --algorithm=ecc256 --label=l2 --key-label=ec --userpin=userpin --path=$TPM2_PKCS11_STORE
tpm2_ptool addkey --algorithm=rsa2048 --label=l2 --key-label=rsa --userpin=userpin --path=$TPM2_PKCS11_STORE
M=/usr/lib64/pkcs11/libtpm2_pkcs11.so
echo data > $TPM2_PKCS11_STORE/data
pkcs11-tool --module $M --token-label l2 -l -p userpin --sign -m ECDSA-SHA256 --label ec -i $TPM2_PKCS11_STORE/data -o $TPM2_PKCS11_STORE/sig
test -s $TPM2_PKCS11_STORE/sig"#,
    );

    // 08: the tpm2 OpenSSL provider, a TPM-held EC and RSA key signing, each verified.
    run(
        guest,
        "tpm2-openssl",
        r#"cd "$(mktemp -d)"
P="-provider tpm2 -provider default"
echo data > data
for alg in "EC -pkeyopt group:P-256" "RSA -pkeyopt bits:2048"; do
  openssl genpkey $P -propquery "?provider=tpm2" -algorithm $alg -out key.pem
  openssl pkeyutl $P -sign -inkey key.pem -in data -rawin -digest sha256 -out sig
  openssl pkey $P -in key.pem -pubout -out pub.pem
  openssl pkeyutl -verify -pubin -inkey pub.pem -in data -rawin -digest sha256 -sigfile sig
done"#,
    );

    // For after the reboot.
    run(
        guest,
        "sealing for the reboot",
        r#"echo -n across-a-reboot | sudo systemd-creds encrypt --with-key=tpm2 --name=r1 - /var/tmp/l2-tpm-r1.cred
echo -n clevis-reboot | clevis encrypt tpm2 "{}" > /var/tmp/l2-tpm-r2.jwe"#,
    );
}

/// What the first boot sealed opens on the second, and its keys are still in the TPM.
/// The firmware's event log replays to what the TPM holds: for every PCR the log extends, the
/// value computed from the log's SHA-256 digests is the one the TPM reports, and the log covers
/// at least the firmware's PCRs 0-7.
fn event_log_replays(guest: &Guest) {
    let out = run(
        guest,
        "event log replay",
        r#"sudo tpm2_eventlog /sys/kernel/security/tpm0/binary_bios_measurements > /tmp/l2-el.yaml
sudo tpm2_pcrread sha256 > /tmp/l2-pcrs.yaml
python3 - <<"PY"
import re

def bank(text):
    text = text[text.index("sha256:") + len("sha256:"):]
    values = {}
    for line in text.splitlines()[1:]:
        m = re.match(r"\s+(\d+)\s*:\s*0x([0-9A-Fa-f]+)\s*$", line)
        if not m:
            break
        values[int(m.group(1))] = m.group(2).lower()
    return values

log = open("/tmp/l2-el.yaml").read()
log = bank(log[log.index("\npcrs:"):])
tpm = bank(open("/tmp/l2-pcrs.yaml").read())
missing = sorted(set(range(8)) - set(log))
assert not missing, f"the event log covers no events for PCRs {missing}"
wrong = [i for i in sorted(log) if tpm.get(i) != log[i]]
assert not wrong, "PCRs that do not replay: " + ", ".join(
    f"{i}: log {log[i]} tpm {tpm.get(i)}" for i in wrong)
print("replayed", " ".join(map(str, sorted(log))))
PY"#,
    );
    assert!(out.contains("replayed"), "{out}");
    eprintln!("event log: {}", out.trim());
}

fn after_the_reboot(guest: &Guest) {
    let handles = run(
        guest,
        "persistent handles",
        "sudo tpm2_getcap handles-persistent",
    );
    assert!(
        handles.contains("0x81000001"),
        "the SRK did not survive:\n{handles}"
    );
    let opened = run(
        guest,
        "unsealing after the reboot",
        r#"sudo systemd-creds decrypt --name=r1 /var/tmp/l2-tpm-r1.cred -; echo
clevis decrypt < /var/tmp/l2-tpm-r2.jwe; echo"#,
    );
    assert_eq!(
        opened.lines().collect::<Vec<_>>(),
        ["across-a-reboot", "clevis-reboot"]
    );
    run(
        guest,
        "LUKS after the reboot",
        r#"sudo PIN=4321 /usr/lib/systemd/systemd-cryptsetup attach l2tpm /var/tmp/l2-tpm-luks.img - tpm2-device=auto,headless=1
test -b /dev/mapper/l2tpm
sudo /usr/lib/systemd/systemd-cryptsetup detach l2tpm"#,
    );
}

fn reboot(guest: &Guest) {
    let before = boot_id(guest);
    run(
        guest,
        "reboot",
        "sudo systemd-run --on-active=1 systemctl reboot",
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if let Ok(out) = guest.ssh_exec("cat /proc/sys/kernel/random/boot_id")
            && !out.trim().is_empty()
            && out.trim() != before
        {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let kept = guest.forensics("rebooting", "systemd");
    panic!("the guest did not come back from its reboot; evidence in {kept:?}");
}

#[test]
fn stock_guest_tpm_consumers_work_and_survive_a_reboot() {
    if !limina_test::require_hvf_or_skip("stock_guest_tpm_consumers_work_and_survive_a_reboot") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("limina-l2-tpm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the TPM's state");
    let state = dir.join("tpm.state");
    // With a TPM, shim's fallback resets after writing the boot entry, so the variables must
    // persist or the guest resets forever (docs/design/efi-vars.md). The first boot here is that
    // one fallback reset; the supervisor relaunches through it.
    let vars = dir.join("efi.vars");
    let cfg = match GuestConfig::tpm_fedora_from_env() {
        Ok(cfg) => cfg
            .with_supervisor_arg("--tpm-state")
            .with_supervisor_arg(state.to_str().expect("a UTF-8 temporary path"))
            .with_supervisor_arg("--efi-vars")
            .with_supervisor_arg(vars.to_str().expect("a UTF-8 temporary path")),
        Err(e) => {
            eprintln!("SKIP stock_guest_tpm_consumers_work_and_survive_a_reboot: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest did not reach sshd");
    event_log_replays(&guest);
    consumers(&guest);
    assert_owner_only(&state);

    reboot(&guest);
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest did not reach sshd after its reboot");
    event_log_replays(&guest);
    after_the_reboot(&guest);

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    std::fs::remove_dir_all(&dir).expect("removing the TPM's state");
}

/// What the guest holds in the TPM's volatile state when it suspends, for the restore to find:
/// a PCR extended past its boot value, a secret sealed to it, a key loaded at a transient handle
/// and a policy session whose context is saved. All of it goes through `/dev/tpm0`, so the
/// kernel's resource manager neither flushes the key nor virtualizes the handles.
fn before_the_suspend(guest: &Guest) -> String {
    run(
        guest,
        "volatile state",
        r#"sudo rm -rf /var/tmp/l2s; sudo mkdir -p /var/tmp/l2s; cd /var/tmp/l2s
export TPM2TOOLS_TCTI=device:/dev/tpm0
sudo -E tpm2_pcrextend 16:sha256=$(printf p4 | sha256sum | cut -d" " -f1)
sudo -E tpm2_createprimary -Q -C o -g sha256 -G ecc -c primary.ctx
sudo -E tpm2_startauthsession -Q --policy-session -S session.ctx
echo -n across-a-restore | sudo systemd-creds encrypt --with-key=tpm2 --tpm2-pcrs=16 --name=s1 - s1.cred"#,
    );
    run(guest, "PCR 16", "sudo tpm2_pcrread sha256:16")
}

/// After the restore, everything [`before_the_suspend`] left is still there and usable.
fn after_the_restore(guest: &Guest, pcr16: &str) {
    assert_eq!(
        run(guest, "PCR 16", "sudo tpm2_pcrread sha256:16"),
        pcr16,
        "PCR 16 did not survive the restore"
    );
    let loaded = run(
        guest,
        "transient handles",
        "sudo TPM2TOOLS_TCTI=device:/dev/tpm0 tpm2_getcap handles-transient",
    );
    assert!(
        loaded.contains("0x80000000"),
        "the key loaded before the suspend is gone:\n{loaded}"
    );
    // The contexts load and the session takes a policy command. Commands to /dev/tpm0 leave
    // what they load loaded, so the transient objects are flushed before systemd-creds, which
    // needs two of the TPM's three object slots.
    let out = run(
        guest,
        "volatile state after the restore",
        r#"cd /var/tmp/l2s
export TPM2TOOLS_TCTI=device:/dev/tpm0
sudo -E tpm2_readpublic -Q -c primary.ctx
sudo -E tpm2_policypcr -Q -S session.ctx -l sha256:16
sudo -E tpm2_flushcontext session.ctx
sudo -E tpm2_flushcontext -t
sudo systemd-creds decrypt --name=s1 s1.cred -; echo"#,
    );
    assert!(
        out.lines().any(|l| l == "across-a-restore"),
        "the secret sealed to PCR 16 did not unseal:\n{out}"
    );
}

/// docs/design/vtpm.md, P4: a guest suspended with a TPM resumes with the TPM it suspended with,
/// running: its PCRs, its loaded key and its saved session are where it left them. The guest
/// sends `TPM2_Shutdown(STATE)` on the way down and no Startup on the way up, as on hardware
/// whose firmware does not run on an s2idle resume.
#[test]
fn stock_guest_tpm_survives_a_suspend_and_restore() {
    if !limina_test::require_hvf_or_skip("stock_guest_tpm_survives_a_suspend_and_restore") {
        return;
    }
    // The device's trace names each command, so the suspend leg shows the guest's Shutdown.
    unsafe { std::env::set_var("RUST_LOG", "info,krun_devices::tpm=trace") };
    let dir = std::env::temp_dir().join(format!("limina-l2-tpm-snap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the TPM's state");
    let state = dir.join("tpm.state");
    let vars = dir.join("efi.vars");
    let base = match GuestConfig::tpm_fedora_from_env() {
        Ok(cfg) => cfg
            .with_supervisor_arg("--tpm-state")
            .with_supervisor_arg(state.to_str().expect("a UTF-8 temporary path"))
            .with_supervisor_arg("--efi-vars")
            .with_supervisor_arg(vars.to_str().expect("a UTF-8 temporary path"))
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIP stock_guest_tpm_survives_a_suspend_and_restore: {e:#}");
            return;
        }
    };

    let mut g1 = Guest::boot(&base.clone().with_snapshot()).expect("spawning the supervisor");
    g1.wait_for_ssh(Duration::from_secs(300))
        .expect("guest did not reach sshd");
    let booted = boot_id(&g1);
    let pcr16 = before_the_suspend(&g1);

    // The bracket's suspend-button pulse alone, as `limina suspend` does. A timer armed in the
    // guest as well would fire after the restore and put the resumed guest back to sleep.
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    let suspend_log = g1.supervisor_log();
    assert_eq!(
        outcome.code,
        Some(126),
        "the guest did not suspend: {outcome:?}\n{suspend_log}"
    );
    // The guest's own suspend sends TPM2_Shutdown (the firmware sends one too, on the fallback
    // reset at first boot): the snapshot holds a TPM that has shut down and goes on running.
    let shut_down = suspend_log
        .lines()
        .skip_while(|l| !l.contains("bracket: SIGTSTP received"))
        .any(|l| l.contains("tpm: command 0x145 -> 0x0"));
    assert!(
        shut_down,
        "the guest suspended without TPM2_Shutdown:\n{suspend_log}"
    );

    let snap = dir.join("snapshot");
    let disk = dir.join("disk.raw");
    std::fs::copy(g1.snapshot_path().expect("a snapshot path"), &snap).expect("keeping it");
    limina_test::cow_clone(&g1.scratch_dir().join("disk.raw"), &disk).expect("keeping the disk");
    drop(g1);

    let mut cfg2 = base.restore_from(&snap);
    if let limina_test::Boot::Firmware { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .expect("the worker never entered the restore path");
    g2.wait_for_ssh(Duration::from_secs(120))
        .expect("the restored guest never reached sshd");
    assert_eq!(
        boot_id(&g2),
        booted,
        "the guest rebooted instead of resuming"
    );
    after_the_restore(&g2, &pcr16);

    let outcome = g2
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    std::fs::remove_dir_all(&dir).expect("removing the TPM's state");
}

fn assert_owner_only(state: &Path) {
    let meta = std::fs::metadata(state).expect("the TPM's state file");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "{state:?} is not owner-only"
    );
}
