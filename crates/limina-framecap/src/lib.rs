// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Frame-sequence capture: what is written for every frame a window puts on glass.
//!
//! The supervisor's window grabs each presented scanout (`crates/limina/src/window/
//! frame_capture.rs`); this crate is the part that needs no AppKit: the encoder, the record
//! written for each frame, the sequencing that turns the worker's flip counter into "which flips
//! never reached glass", and the reorder buffer that keeps `frames.jsonl` in present order while
//! frames finish encoding out of order on several threads.
//!
//! The capture directory (`s<slot>-<seq>.png` images and `frames.jsonl`), and what every field
//! of a [`Record`], a [`Summary`] and a [`Still`] means, are defined in `docs/graphics.md` §8
//! ("The record format"); the field docs here are reminders, the document is the definition.
//! Images are RGB, 8 bit: the scanout's alpha is "don't care", so it is dropped rather than
//! written as noise.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Byte order of the source pixels, as an IOSurface's four-character pixel format names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// `'BGRA'`: blue in the first byte. Every scanout the worker makes today.
    Bgra,
    /// `'RGBA'`.
    Rgba,
}

/// Encode a `w` x `h` frame of 4-byte pixels with row stride `bpr` as an 8-bit RGB PNG.
///
/// Fast compression with the Sub filter: measured on a busy 1920x1080 frame (the WebGL aquarium),
/// 12-15 ms and 4.1 MB, against 257 ms and 2.9 MB at the default level and 6.4 ms but 8.3 MB for
/// the raw bytes. Four encoder threads keep up with 60 Hz at 2560x1440 with room to spare.
pub fn encode_png_rgb(
    pixels: &[u8],
    w: usize,
    h: usize,
    bpr: usize,
    order: PixelOrder,
) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 || bpr < w * 4 || pixels.len() < (h - 1) * bpr + w * 4 {
        return Err(format!(
            "a {w}x{h} frame with stride {bpr} needs more than the {} bytes given",
            pixels.len()
        ));
    }
    let (r, b) = match order {
        PixelOrder::Bgra => (2, 0),
        PixelOrder::Rgba => (0, 2),
    };
    let mut rgb = vec![0u8; w * h * 3];
    for (y, out) in rgb.chunks_exact_mut(w * 3).enumerate() {
        let row = &pixels[y * bpr..y * bpr + w * 4];
        for (s, d) in row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<3>().0)
        {
            d[0] = s[r];
            d[1] = s[1];
            d[2] = s[b];
        }
    }
    let mut png = Vec::with_capacity(w * h);
    {
        let mut enc = png::Encoder::new(&mut png, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.set_filter(png::FilterType::Sub);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(&rgb))
            .map_err(|e| e.to_string())?;
    }
    Ok(png)
}

/// The file a captured frame is written to, relative to the capture directory.
pub fn file_name(slot: usize, seq: u64) -> String {
    format!("s{slot}-{seq:08}.png")
}

/// The sidecar's name inside the capture directory.
pub const SIDECAR: &str = "frames.jsonl";

/// Why a frame has no image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A guest flip the window never put on glass: a newer frame replaced it before the window
    /// applied it (or before its copy finished). Not a capture failure — nothing was shown.
    NotPresented,
    /// Shown, but every capture buffer was still waiting for an encoder.
    QueueFull,
    /// Shown, but the next frame went up before the GPU copy of this one had finished, so the
    /// guest may have been handed this buffer back while it was being read. Discarded rather
    /// than kept with pixels that might belong to a later frame.
    Overtaken,
    /// Shown, but the GPU copy of it could not be made or failed.
    CopyFailed,
    /// Shown, but the capture had already written its byte budget.
    DiskBudget,
    /// Shown and copied, but encoding or writing the file failed.
    WriteError,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotPresented => "not_presented",
            Reason::QueueFull => "queue_full",
            Reason::Overtaken => "overtaken",
            Reason::CopyFailed => "copy_failed",
            Reason::DiskBudget => "disk_budget",
            Reason::WriteError => "write_error",
        }
    }
}

/// Why a frame went on glass when it is not a new guest flip: the host path that presented it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The worker (re)configured the scanout — a mode set, or the display re-enabled after the
    /// guest blanked it — and announced fresh buffers; the window put up the first of them,
    /// which no guest flip has drawn into yet. `flip` is the last guest flip, not this frame's.
    ScanoutConfigured,
    /// The window put a guest flip already on glass up again (a display moving to another
    /// window, or a window that re-applied its slot with no new frame).
    Reshow,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::ScanoutConfigured => "scanout_configured",
            Cause::Reshow => "reshow",
        }
    }
}

/// The record format this crate writes: the `format` field of every line. Bumped by any change
/// a reader of an older format would misread; a field added that older readers can ignore does
/// not bump it.
pub const FORMAT: u32 = 1;

/// One line of `frames.jsonl`: a frame that went on glass (captured or not), or a guest flip
/// that never did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// [`FORMAT`].
    pub format: u32,
    /// The guest display (pool slot) the frame belongs to.
    pub slot: usize,
    /// This capture's count of presents on `slot`, from 1. `None` for a flip never presented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The worker's flip count for `slot` within `epoch`. For a `not_presented` run, its last;
    /// for a present that is not a guest flip, the last guest flip the worker had sent.
    pub flip: u64,
    /// Whether the record stands for a guest flip: a new one on glass, or a `not_presented` run.
    /// `false` for a frame the host put on glass with no new guest flip; `cause` says which path.
    pub guest_flip: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// A `not_presented` run's first and last flip (inclusive), and how many flips it covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_from: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_to: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Which worker the flip came from; bumped by every reboot or resume.
    pub epoch: u64,
    /// The IOSurface id the worker named for this frame (the guest's scanout buffer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presented_iosurface: Option<u32>,
    /// The IOSurface id the window put on its layer, which is what the image is read from: the
    /// window's private copy when it shows one, else the same as `presented_iosurface`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_iosurface: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// `CLOCK_MONOTONIC_RAW` (= `mach_continuous_time`) in ns, as the frame went on glass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_monotonic_raw_ns: Option<u64>,
    /// `CLOCK_REALTIME` in ns since the Unix epoch, as the frame went on glass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_realtime_ns: Option<u64>,
    /// The image, relative to the capture directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dropped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

impl Default for Record {
    fn default() -> Self {
        Record {
            format: FORMAT,
            slot: 0,
            seq: None,
            flip: 0,
            guest_flip: true,
            cause: None,
            flip_from: None,
            flip_to: None,
            count: None,
            epoch: 0,
            presented_iosurface: None,
            layer_iosurface: None,
            width: None,
            height: None,
            t_monotonic_raw_ns: None,
            t_realtime_ns: None,
            file: None,
            dropped: false,
            reason: None,
        }
    }
}

impl Record {
    /// The guest flips `flips` on `slot` that never reached glass, as one record. `None` when
    /// the range is empty.
    pub fn not_presented(slot: usize, epoch: u64, flips: std::ops::Range<u64>) -> Option<Record> {
        if flips.is_empty() {
            return None;
        }
        Some(Record {
            slot,
            flip: flips.end - 1,
            flip_from: Some(flips.start),
            flip_to: Some(flips.end - 1),
            count: Some(flips.end - flips.start),
            epoch,
            dropped: true,
            reason: Some(Reason::NotPresented),
            ..Record::default()
        })
    }

    /// Mark a presented frame as having no image, for `reason`.
    pub fn drop_for(mut self, reason: Reason) -> Record {
        self.file = None;
        self.dropped = true;
        self.reason = Some(reason);
        self
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("a record always serializes")
    }
}

/// The last line of `frames.jsonl`, written when the capture stops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// [`FORMAT`].
    pub format: u32,
    /// Always `true`: what tells this line from a [`Record`].
    pub summary: bool,
    /// Frames a window put on glass while capturing.
    pub presented: u64,
    /// Of those, frames written to an image.
    pub captured: u64,
    /// Of those, frames with no image, by any reason but `not_presented`.
    pub dropped: u64,
    /// Guest flips that never reached glass (the sum of the `not_presented` records' counts).
    pub not_presented: u64,
    /// Every frame without an image, by reason (`not_presented` counted in flips).
    pub by_reason: BTreeMap<String, u64>,
    /// Wall time from start to stop.
    pub duration_s: f64,
    /// `captured / duration_s`.
    pub captured_fps: f64,
    /// Bytes of image written.
    pub bytes: u64,
    /// What the capture cost the main thread per presented frame, in microseconds.
    pub present_hook_us_p50: u64,
    pub present_hook_us_p99: u64,
    pub present_hook_us_max: u64,
    /// Present only when the stop gave up waiting: how many frames were still in flight. Records
    /// for them never reach the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<u64>,
}

impl Summary {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("a summary always serializes")
    }

    /// One human line, for a log or a terminal.
    pub fn describe(&self) -> String {
        let incomplete = match self.incomplete {
            Some(n) => format!("; INCOMPLETE: {n} frames still in flight at the stop"),
            None => String::new(),
        };
        format!(
            "{} presented, {} captured, {} dropped, {} flips never presented, over {:.1} s \
             ({:.1} captured fps, {:.1} MB); main thread {} us p50 / {} us max per frame{}",
            self.presented,
            self.captured,
            self.dropped,
            self.not_presented,
            self.duration_s,
            self.captured_fps,
            self.bytes as f64 / 1e6,
            self.present_hook_us_p50,
            self.present_hook_us_max,
            incomplete,
        )
    }
}

/// One of the VM's windows, as the host shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    /// The guest display (pool slot) the window shows.
    pub slot: usize,
    /// Some part of the window is on screen: AppKit's occlusion state. `false` covers every way
    /// out of sight — wholly covered, minimized, hidden with the app, on another Space.
    pub visible: bool,
    /// Minimized to the Dock.
    pub minimized: bool,
}

/// `NSProcessInfo.thermalState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
    Unknown,
}

impl Thermal {
    pub fn as_str(self) -> &'static str {
        match self {
            Thermal::Nominal => "nominal",
            Thermal::Fair => "fair",
            Thermal::Serious => "serious",
            Thermal::Critical => "critical",
            Thermal::Unknown => "unknown",
        }
    }
}

/// What the host was doing to the VM's windows and its supervisor process, as the supervisor
/// observes it. Only what is read from the system; nothing inferred. `docs/graphics.md` §8
/// ("Host state") says which of these measurably slow presents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostState {
    /// Every window showing a guest display, by slot.
    pub windows: Vec<WindowState>,
    /// The supervisor is the active (frontmost) app.
    pub app_active: bool,
    /// The app is hidden (Cmd-H).
    pub app_hidden: bool,
    /// The supervisor's main thread is at the background band (effective priority 4 or less):
    /// what App Nap, and the Game Mode clamp, do to it. The main thread is the one every frame
    /// goes on glass from.
    pub throttled: bool,
    /// The main thread's effective scheduling priority when sampled (`pth_curpri`). Moves
    /// constantly (31, boosted to 37-47 while the app is in use); only the band is a state.
    pub main_thread_priority: i32,
    /// Online displays, and how many of them are asleep.
    pub displays: u32,
    pub displays_asleep: u32,
    /// The login session's screen lock, from its session dictionary. `None` when unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_locked: Option<bool>,
    /// The session owns the console (`false` after fast user switching away from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_console: Option<bool>,
    pub thermal_state: Thermal,
    pub low_power_mode: bool,
    /// The supervisor holds its anti-throttle activity (the `no-throttle` lever).
    pub no_throttle: bool,
}

impl HostState {
    /// Any of the windows is on screen.
    pub fn visible(&self) -> bool {
        self.windows.iter().any(|w| w.visible)
    }

    /// The same state, ignoring the sampled priority (which jitters within a band).
    pub fn same_as(&self, other: &HostState) -> bool {
        HostState {
            main_thread_priority: other.main_thread_priority,
            ..self.clone()
        } == *other
    }
}

/// Why a [`HostStateRecord`] was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStateCause {
    /// The state when the capture started (or, for a capture started before the window had
    /// sampled anything, the first state sampled).
    CaptureStart,
    /// The state changed.
    Change,
}

/// A line of `frames.jsonl` that is not a frame: the host state at capture start and at every
/// change after it. It holds from its place in the file until the next one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostStateRecord {
    /// [`FORMAT`].
    pub format: u32,
    /// Always `true`: what tells this line from a [`Record`] or a [`Summary`].
    pub host_state: bool,
    pub cause: HostStateCause,
    /// When the state was sampled, on a frame record's clocks.
    pub t_monotonic_raw_ns: u64,
    pub t_realtime_ns: u64,
    #[serde(flatten)]
    pub state: HostState,
}

impl HostStateRecord {
    pub fn new(
        cause: HostStateCause,
        t_monotonic_raw_ns: u64,
        t_realtime_ns: u64,
        state: HostState,
    ) -> Self {
        HostStateRecord {
            format: FORMAT,
            host_state: true,
            cause,
            t_monotonic_raw_ns,
            t_realtime_ns,
            state,
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("a host state always serializes")
    }
}

/// One parsed line of `frames.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Line {
    Summary(Summary),
    HostState(HostStateRecord),
    Frame(Record),
}

/// Parse one line of `frames.jsonl`. A line of another [`FORMAT`] is refused rather than read
/// with this format's meanings.
pub fn parse_line(line: &str) -> Result<Line, String> {
    check_format(line)?;
    serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))
}

fn check_format(line: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Format {
        format: Option<u32>,
    }
    let f: Format = serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))?;
    match f.format {
        Some(FORMAT) => Ok(()),
        Some(n) => Err(format!(
            "format {n}; this reader knows format {FORMAT}: {line}"
        )),
        None => Err(format!("no format field: {line}")),
    }
}

/// What `limina debug <vm> capture still <png> [slot]` prints: the frame on glass on one display
/// when the still was read, in the same vocabulary as a [`Record`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Still {
    /// [`FORMAT`].
    pub format: u32,
    /// Always `true`: what tells this line from a [`Record`] or a [`Summary`].
    pub still: bool,
    pub slot: usize,
    /// The frame on glass, as its record would tag it.
    pub flip: u64,
    pub epoch: u64,
    pub guest_flip: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    pub presented_iosurface: u32,
    pub layer_iosurface: u32,
    pub width: u32,
    pub height: u32,
    /// When that frame went on glass, as a record's `t_*` (so possibly long ago).
    pub t_monotonic_raw_ns: u64,
    pub t_realtime_ns: u64,
    /// When the still was read, on the same two clocks.
    pub taken_t_monotonic_raw_ns: u64,
    pub taken_t_realtime_ns: u64,
    /// The image written, as given (absolute).
    pub file: String,
}

impl Still {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("a still always serializes")
    }

    pub fn parse(line: &str) -> Result<Still, String> {
        check_format(line)?;
        serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))
    }
}

/// Turns each present into its `seq`, and the worker's flip counter into the flips that never
/// reached glass.
#[derive(Default)]
pub struct Sequencer {
    slots: HashMap<usize, SlotSeq>,
}

#[derive(Default)]
struct SlotSeq {
    seq: u64,
    last: Option<(u64, u64)>,
}

/// What [`Sequencer::present`] made of one present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presented {
    /// The present's `seq`.
    pub seq: u64,
    /// The flips of that worker since the previous guest flip on glass that never reached it.
    pub missed: std::ops::Range<u64>,
    /// `None` when the present is a new guest flip; otherwise the host path that presented it.
    pub cause: Option<Cause>,
}

impl Sequencer {
    /// A frame went on glass on `slot`, tagged with flip `flip` of worker `epoch`. `cause` is the
    /// window's own word for a present that is not a guest flip (`None` when it says it is one).
    ///
    /// The first present of a capture has no predecessor, so nothing before it counts as missed.
    /// A new epoch's flips count from 1, so all of them before this one were missed. A present
    /// that is not a guest flip misses nothing and leaves the bookkeeping where the last guest
    /// flip put it, so a flip it stood in for still counts as missed when the next one lands.
    pub fn present(
        &mut self,
        slot: usize,
        epoch: u64,
        flip: u64,
        cause: Option<Cause>,
    ) -> Presented {
        let s = self.slots.entry(slot).or_default();
        s.seq += 1;
        // The window's word first; a flip already put on glass is a re-show whatever it says.
        let cause = cause.or((s.last == Some((epoch, flip))).then_some(Cause::Reshow));
        if cause.is_some() {
            return Presented {
                seq: s.seq,
                missed: flip..flip,
                cause,
            };
        }
        let missed = match s.last {
            None => flip..flip,
            Some((e, f)) if e == epoch => (f + 1).min(flip)..flip,
            Some(_) => 1.min(flip)..flip,
        };
        s.last = Some((epoch, flip));
        Presented {
            seq: s.seq,
            missed,
            cause,
        }
    }
}

/// Keeps lines in the order they were numbered, whatever order they finish in.
///
/// Every number handed out must eventually be [`Reorder::put`], or every line after it waits;
/// the supervisor guarantees that by deciding each frame's fate (captured or dropped) on every
/// path that took a number.
#[derive(Default)]
pub struct Reorder {
    next: u64,
    pending: BTreeMap<u64, String>,
}

impl Reorder {
    /// File line `n`; returns the lines now ready, in order.
    pub fn put(&mut self, n: u64, line: String) -> Vec<String> {
        self.pending.insert(n, line);
        let mut ready = Vec::new();
        while let Some(line) = self.pending.remove(&self.next) {
            ready.push(line);
            self.next += 1;
        }
        ready
    }

    /// Lines still waiting on an earlier one.
    pub fn waiting(&self) -> usize {
        self.pending.len()
    }
}

/// The running totals a [`Summary`] is made from.
#[derive(Default)]
pub struct Tally {
    presented: u64,
    captured: u64,
    bytes: u64,
    by_reason: BTreeMap<Reason, u64>,
    hook_us: Vec<u32>,
}

/// How many main-thread cost samples are kept: an hour at 60 Hz, then the rest are not sampled.
const HOOK_SAMPLES: usize = 216_000;

impl Tally {
    /// Count one finished record.
    pub fn count(&mut self, r: &Record, bytes: u64) {
        if r.seq.is_some() {
            self.presented += 1;
        }
        match r.reason {
            Some(reason) => *self.by_reason.entry(reason).or_default() += r.count.unwrap_or(1),
            None => {
                self.captured += 1;
                self.bytes += bytes;
            }
        }
    }

    /// Record what one present cost the main thread.
    pub fn hook_cost(&mut self, us: u32) {
        if self.hook_us.len() < HOOK_SAMPLES {
            self.hook_us.push(us);
        }
    }

    pub fn summary(&self, duration_s: f64) -> Summary {
        let not_presented = self
            .by_reason
            .get(&Reason::NotPresented)
            .copied()
            .unwrap_or(0);
        let dropped = self.by_reason.values().sum::<u64>() - not_presented;
        let mut us = self.hook_us.clone();
        us.sort_unstable();
        let pct = |p: f64| {
            us.get(((us.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(0) as u64
        };
        Summary {
            format: FORMAT,
            summary: true,
            presented: self.presented,
            captured: self.captured,
            dropped,
            not_presented,
            by_reason: self
                .by_reason
                .iter()
                .map(|(r, n)| (r.as_str().to_string(), *n))
                .collect(),
            duration_s,
            captured_fps: if duration_s > 0.0 {
                self.captured as f64 / duration_s
            } else {
                0.0
            },
            bytes: self.bytes,
            present_hook_us_p50: pct(0.5),
            present_hook_us_p99: pct(0.99),
            present_hook_us_max: us.last().copied().unwrap_or(0) as u64,
            incomplete: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presented(slot: usize, seq: u64, flip: u64) -> Record {
        Record {
            slot,
            seq: Some(seq),
            flip,
            epoch: 1,
            presented_iosurface: Some(40),
            layer_iosurface: Some(41),
            width: Some(2),
            height: Some(2),
            t_monotonic_raw_ns: Some(5),
            t_realtime_ns: Some(6),
            file: Some(file_name(slot, seq)),
            ..Record::default()
        }
    }

    fn flip(seq: u64, missed: std::ops::Range<u64>) -> Presented {
        Presented {
            seq,
            missed,
            cause: None,
        }
    }

    #[test]
    fn the_sequence_counts_presents_and_names_the_flips_never_shown() {
        let mut s = Sequencer::default();
        // The first present of a capture misses nothing, whatever flip it is.
        assert_eq!(s.present(0, 1, 40, None), flip(1, 40..40));
        // Consecutive flips miss nothing.
        assert_eq!(s.present(0, 1, 41, None), flip(2, 41..41));
        assert!(s.present(0, 1, 42, None).missed.is_empty());
        // Flips 43 and 44 were replaced before the window applied them.
        assert_eq!(s.present(0, 1, 45, None), flip(4, 43..45));
        // Another slot has its own count.
        assert_eq!(s.present(1, 1, 7, None), flip(1, 7..7));
        // A fresh worker counts flips from 1: its first three never reached glass.
        assert_eq!(s.present(0, 2, 4, None), flip(5, 1..4));
        // ... and a fresh worker whose first flip is shown misses nothing.
        assert!(s.present(1, 2, 1, None).missed.is_empty());
    }

    /// The trial's seq 26: the display woke, the worker announced its scanout again, and the
    /// window put the announcement's buffer up under the flip number it already had.
    #[test]
    fn a_present_that_is_no_new_guest_flip_says_what_presented_it() {
        let mut s = Sequencer::default();
        assert_eq!(s.present(0, 1, 286, None), flip(1, 286..286));
        assert_eq!(s.present(0, 1, 287, None), flip(2, 287..287));
        // The window says the scanout was (re)configured: not a guest flip, nothing missed.
        let wake = s.present(0, 1, 287, Some(Cause::ScanoutConfigured));
        assert_eq!(
            wake,
            Presented {
                seq: 3,
                missed: 287..287,
                cause: Some(Cause::ScanoutConfigured)
            }
        );
        // The guest's next flip is one again, and nothing was missed.
        assert_eq!(s.present(0, 1, 288, None), flip(4, 288..288));
        // The same flip again with no word from the window is still not a new guest flip.
        assert_eq!(
            s.present(0, 1, 288, None).cause,
            Some(Cause::Reshow),
            "a repeated flip must be marked"
        );
        // An announcement landing after a flip the window never applied: the announcement's
        // buffer goes up tagged with that flip, which therefore never reached glass.
        assert_eq!(
            s.present(0, 1, 289, Some(Cause::ScanoutConfigured)).cause,
            Some(Cause::ScanoutConfigured)
        );
        assert_eq!(s.present(0, 1, 290, None), flip(7, 289..290));
    }

    #[test]
    fn lines_come_out_in_number_order_whatever_order_they_finish() {
        let mut r = Reorder::default();
        assert!(r.put(1, "b".into()).is_empty());
        assert!(r.put(2, "c".into()).is_empty());
        assert_eq!(r.waiting(), 2);
        assert_eq!(r.put(0, "a".into()), ["a", "b", "c"]);
        assert_eq!(r.put(3, "d".into()), ["d"]);
        assert_eq!(r.waiting(), 0);
    }

    #[test]
    fn every_line_carries_the_format_and_every_record_says_whether_it_is_a_guest_flip() {
        let flipped = presented(0, 1, 9);
        let line = flipped.to_line();
        assert!(line.starts_with(r#"{"format":1,"#), "{line}");
        assert!(line.contains(r#""guest_flip":true"#), "{line}");
        assert!(!line.contains("cause"), "{line}");
        let host = Record {
            guest_flip: false,
            cause: Some(Cause::ScanoutConfigured),
            ..presented(0, 2, 9)
        };
        let line = host.to_line();
        assert!(line.contains(r#""guest_flip":false"#), "{line}");
        assert!(line.contains(r#""cause":"scanout_configured""#), "{line}");
        assert_eq!(parse_line(&line), Ok(Line::Frame(host)));
        let never = Record::not_presented(0, 1, 3..5).unwrap().to_line();
        assert!(never.contains(r#""guest_flip":true"#), "{never}");
        let summary = Tally::default().summary(1.0).to_line();
        assert!(summary.starts_with(r#"{"format":1,"#), "{summary}");
        // A line of another format, or none, is refused rather than misread.
        let other = flipped.to_line().replace(r#""format":1"#, r#""format":2"#);
        assert!(parse_line(&other).unwrap_err().contains("format 2"));
        let bare = flipped.to_line().replace(r#""format":1,"#, "");
        assert!(parse_line(&bare).is_err());
    }

    #[test]
    fn a_still_round_trips_in_the_record_vocabulary() {
        let still = Still {
            format: FORMAT,
            still: true,
            slot: 0,
            flip: 287,
            epoch: 1,
            guest_flip: false,
            cause: Some(Cause::ScanoutConfigured),
            presented_iosurface: 44,
            layer_iosurface: 170,
            width: 1280,
            height: 800,
            t_monotonic_raw_ns: 1,
            t_realtime_ns: 2,
            taken_t_monotonic_raw_ns: 3,
            taken_t_realtime_ns: 4,
            file: "/x/a.png".into(),
        };
        let line = still.to_line();
        assert!(line.starts_with(r#"{"format":1,"still":true,"#), "{line}");
        assert_eq!(Still::parse(&line), Ok(still));
        // A record is not a still.
        assert!(Still::parse(&presented(0, 1, 1).to_line()).is_err());
    }

    #[test]
    fn records_round_trip_and_omit_what_they_do_not_have() {
        let shown = presented(0, 3, 9);
        let line = shown.to_line();
        assert!(!line.contains("dropped"), "{line}");
        assert!(!line.contains("reason"), "{line}");
        assert_eq!(parse_line(&line), Ok(Line::Frame(shown.clone())));

        let gone = shown.drop_for(Reason::QueueFull);
        let line = gone.to_line();
        assert!(line.contains(r#""dropped":true"#), "{line}");
        assert!(line.contains(r#""reason":"queue_full""#), "{line}");
        assert!(!line.contains("file"), "{line}");
        assert_eq!(parse_line(&line), Ok(Line::Frame(gone)));

        assert_eq!(Record::not_presented(1, 2, 17..17), None);
        let never = Record::not_presented(1, 2, 17..20).unwrap();
        assert_eq!(
            (never.flip_from, never.flip_to, never.count, never.flip),
            (Some(17), Some(19), Some(3), 19)
        );
        let line = never.to_line();
        assert!(!line.contains("seq"), "{line}");
        assert!(line.contains(r#""reason":"not_presented""#), "{line}");
        assert_eq!(parse_line(&line), Ok(Line::Frame(never)));
    }

    #[test]
    fn the_summary_separates_capture_drops_from_flips_never_shown() {
        let mut t = Tally::default();
        t.count(&presented(0, 1, 1), 100);
        t.count(&presented(0, 2, 2), 50);
        t.count(&presented(0, 3, 3).drop_for(Reason::Overtaken), 0);
        t.count(&Record::not_presented(0, 1, 4..6).unwrap(), 0);
        for us in [10, 20, 30, 1000] {
            t.hook_cost(us);
        }
        let s = t.summary(2.0);
        assert_eq!(
            (s.presented, s.captured, s.dropped, s.not_presented),
            (3, 2, 1, 2)
        );
        assert_eq!(s.by_reason["overtaken"], 1);
        assert_eq!(s.by_reason["not_presented"], 2);
        assert_eq!(s.bytes, 150);
        assert_eq!(s.captured_fps, 1.0);
        assert_eq!(s.present_hook_us_max, 1000);
        let line = s.to_line();
        assert_eq!(parse_line(&line), Ok(Line::Summary(s)));
    }

    fn host(visible: bool, priority: i32) -> HostState {
        HostState {
            windows: vec![WindowState {
                slot: 0,
                visible,
                minimized: !visible,
            }],
            app_active: visible,
            app_hidden: false,
            throttled: priority <= 4,
            main_thread_priority: priority,
            displays: 1,
            displays_asleep: 0,
            screen_locked: Some(false),
            on_console: Some(true),
            thermal_state: Thermal::Nominal,
            low_power_mode: false,
            no_throttle: false,
        }
    }

    #[test]
    fn a_host_state_line_round_trips_and_is_told_from_frames_and_summaries() {
        let rec = HostStateRecord::new(HostStateCause::CaptureStart, 7, 8, host(true, 31));
        let line = rec.to_line();
        assert!(
            line.starts_with(r#"{"format":1,"host_state":true,"cause":"capture_start","#),
            "{line}"
        );
        // Flat: a reader filtering with jq sees the state's fields at the top level.
        assert!(line.contains(r#""app_active":true"#), "{line}");
        assert!(line.contains(r#""thermal_state":"nominal""#), "{line}");
        assert_eq!(parse_line(&line), Ok(Line::HostState(rec)));
        // A frame and a summary are still themselves.
        let frame = presented(0, 1, 1);
        assert_eq!(parse_line(&frame.to_line()), Ok(Line::Frame(frame)));
        let summary = Tally::default().summary(1.0);
        assert_eq!(parse_line(&summary.to_line()), Ok(Line::Summary(summary)));
        // An unreadable lock state is left out, not guessed.
        let mut unknown = host(false, 4);
        unknown.screen_locked = None;
        let line = HostStateRecord::new(HostStateCause::Change, 1, 2, unknown).to_line();
        assert!(!line.contains("screen_locked"), "{line}");
        assert!(line.contains(r#""throttled":true"#), "{line}");
    }

    #[test]
    fn a_priority_wobble_is_not_a_change_of_state() {
        assert!(host(true, 31).same_as(&host(true, 47)));
        assert!(!host(true, 31).same_as(&host(false, 31)));
        assert!(
            !host(true, 31).same_as(&host(true, 4)),
            "the band is a state"
        );
        assert!(host(true, 31).visible() && !host(false, 31).visible());
    }

    #[test]
    fn the_encoder_writes_rgb_from_either_byte_order_and_skips_row_padding() {
        // 2x2, stride 12: one pixel of padding per row that must not be read.
        let mut bgra = vec![0xeeu8; 24];
        for y in 0..2 {
            for x in 0..2 {
                bgra[y * 12 + x * 4..y * 12 + x * 4 + 4].copy_from_slice(&[0x10, 0x20, 0x30, 0]);
            }
        }
        for (order, want) in [
            (PixelOrder::Bgra, [0x30, 0x20, 0x10]),
            (PixelOrder::Rgba, [0x10, 0x20, 0x30]),
        ] {
            let png = encode_png_rgb(&bgra, 2, 2, 12, order).unwrap();
            let mut reader = png::Decoder::new(&png[..]).read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut buf).unwrap();
            assert_eq!((info.width, info.height), (2, 2));
            assert_eq!(info.color_type, png::ColorType::Rgb);
            for px in buf[..info.buffer_size()].as_chunks::<3>().0 {
                assert_eq!(px, &want, "{order:?}");
            }
        }
    }

    #[test]
    fn the_encoder_refuses_a_buffer_too_small_for_its_frame() {
        assert!(encode_png_rgb(&[0; 15], 2, 2, 8, PixelOrder::Bgra).is_err());
        assert!(encode_png_rgb(&[0; 64], 4, 2, 8, PixelOrder::Bgra).is_err());
        assert!(encode_png_rgb(&[], 0, 0, 0, PixelOrder::Bgra).is_err());
    }
}
