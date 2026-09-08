// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Did the renderer serve this boot, or refuse it?
//!
//! virglrs has one fatal outcome and it is quiet. A context that raises a host GL error is
//! **poisoned**: every later submit from it is refused, the guest's client keeps running, nothing
//! crashes, and the only trace is a line on the worker's stderr. When that context belongs to the
//! compositor, the desktop stops being painted — and every oracle a test normally carries still
//! passes, because the processes are alive, the windows exist and the last good frame is still on
//! screen.
//!
//! That is not hypothetical. A vrend context died of `CreateObject: GL error 0x502` on the first
//! frame a Vulkan client produced; the line was printed into three separate runs' diagnostics,
//! nothing asserted on it, and the fault was chased through the guest's display configuration for
//! two days. 46318 submits were refused in the boot that finally named it.
//!
//! So: **no test that drives a GPU may pass while the renderer is refusing work.** Call
//! [`assert_renderer_served`] on the supervisor log at the end of any such test.
//!
//! # Why two needles and a control
//!
//! [`refusals`] reads virglrs's own marker. [`refused_submits`] counts the VMM's independent
//! account of the same event, which is written by a different repository in different words — so
//! a renderer that changes its wording degrades this to one needle instead of to zero. And
//! [`spoke`] is the positive control: a log with no refusals *and* no renderer output at all is
//! not a healthy boot, it is a dead needle, and a check that cannot fail is worse than no check.

/// The marker every virglrs refusal is printed under, whichever renderer refused (`REFUSED` in
/// that crate). A line carrying it means work was refused and the context it names is finished.
const REFUSED: &str = "[virglrs] refused:";

/// The VMM's own account of a refused command batch, written by libkrun rather than by virglrs.
const REFUSED_SUBMIT: &str = "submit_command -> Err";

/// Anything the renderer prints. The positive control for the needles above: if this is absent
/// the log is not one the other two could ever have matched.
const SPOKE: &str = "[virglrs]";

/// Every refusal line in the log, in order.
pub fn refusals(log: &str) -> Vec<&str> {
    log.lines().filter(|l| l.contains(REFUSED)).collect()
}

/// How many command batches the VMM recorded as refused.
///
/// A count rather than the lines: one poisoned context refuses every batch that follows, so this
/// runs to tens of thousands and its size is the interesting part — it says how long the guest
/// went on talking to a renderer that had stopped listening.
pub fn refused_submits(log: &str) -> usize {
    log.lines().filter(|l| l.contains(REFUSED_SUBMIT)).count()
}

/// Whether the renderer said anything at all in this log.
pub fn spoke(log: &str) -> bool {
    log.contains(SPOKE)
}

/// Panic unless the renderer served the whole boot.
///
/// `when` names the point in the test this was checked, so a failure says which leg died.
///
/// Fails on a silent log too. A GPU test whose supervisor log contains no renderer output is
/// either not capturing the worker's stderr or not running the renderer, and in both cases the
/// refusal check above is decoration — see the module docs.
#[track_caller]
pub fn assert_renderer_served(log: &str, when: &str) {
    assert!(
        spoke(log),
        "{when}: the supervisor log contains no renderer output at all, so the refusal check \
         below it cannot fail — the log is not being captured, or this boot ran no renderer"
    );
    let refused = refusals(log);
    let submits = refused_submits(log);
    assert!(
        refused.is_empty() && submits == 0,
        "{when}: the renderer REFUSED work — a poisoned context serves nothing further, so \
         whatever was drawing through it stopped.\n  {} refusal(s), {submits} refused submit(s)\
         \n{}",
        refused.len(),
        refused
            .iter()
            .take(8)
            .map(|l| format!("  {}\n", l.trim()))
            .collect::<String>(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines this reads are written by another repository, so they are carried here verbatim
    /// from a real failing boot rather than paraphrased. A needle written from memory is how the
    /// last one rotted.
    const POISONED: &str = "\
[virglrs] vrend: OpenGL ES 3.2 Mesa 26.0.0-devel (git-9edc4f6) (gles 2), 122 formats
[virglrs] refused: vrend ctx 2: CreateObject(SamplerView): GL error 0x502
[2026-09-08T00:30:36Z ERROR krun_rutabaga_gfx::virgl_renderer] virglrs: submit_cmd: the context is poisoned
[2026-09-08T00:30:36Z ERROR krun_devices::virtio::gpu::worker] [SUBMIT3D] ctx 2 submit_command -> Err(\"ErrRutabaga(ComponentError(-22))\") (cmd_size=20392)";

    const HEALTHY: &str = "\
[virglrs] vrend: OpenGL ES 3.2 Mesa 26.0.0-devel (git-9edc4f6) (gles 2), 122 formats
[virglrs] vrend: iosurface scanout: 2560x1440 B8G8R8X8_UNORM (IOSurface id 121)
[virglrs] limina GPU budget: cap 4096 MiB, refusal stops the context";

    #[test]
    fn a_poisoned_context_is_read_out_of_the_log() {
        assert_eq!(refusals(POISONED).len(), 1);
        assert!(refusals(POISONED)[0].contains("CreateObject(SamplerView)"));
        assert_eq!(refused_submits(POISONED), 1);
    }

    #[test]
    fn a_healthy_boot_shows_no_refusals_and_still_speaks() {
        assert!(refusals(HEALTHY).is_empty());
        assert_eq!(refused_submits(HEALTHY), 0);
        // The control: absence of refusals only means something because the renderer was talking.
        assert!(spoke(HEALTHY));
        assert_renderer_served(HEALTHY, "the healthy fixture");
    }

    #[test]
    #[should_panic(expected = "the renderer REFUSED work")]
    fn the_gate_fires_on_a_poisoned_boot() {
        assert_renderer_served(POISONED, "the poisoned fixture");
    }

    /// The failure this whole module exists to prevent, in its purest form: a log that cannot
    /// produce a refusal line must not read as a clean bill of health.
    #[test]
    #[should_panic(expected = "no renderer output at all")]
    fn a_silent_log_fails_rather_than_passing() {
        assert_renderer_served("booting\nguest up\npowering off", "the silent fixture");
    }
}
