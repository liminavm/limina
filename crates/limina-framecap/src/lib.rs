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
//! # The capture directory
//!
//! - `s<slot>-<seq>.png` — one per captured frame, `seq` zero-padded to eight digits. RGB, 8 bit:
//!   the scanout's alpha is "don't care", so it is dropped rather than written as noise.
//! - `frames.jsonl` — one [`Record`] per line, in the order the frames went on glass, then one
//!   [`Summary`] line when the capture stops.
//!
//! # What a record's numbers mean
//!
//! - `seq` counts the frames THIS capture saw a window put on glass for that slot, from 1. It is
//!   the capture's own count, so it never resets, and it is what names the file.
//! - `flip` is the worker's count of `frame` lines for that slot — the guest's page flips as the
//!   supervisor received them. It resets when a fresh worker is swapped in (a guest reboot or a
//!   resume), which bumps `epoch`. A gap in `flip` between two presented frames is guest flips the
//!   window never showed: replaced by a newer one before the window applied it, or before its
//!   copy finished. Each is written as a `not_presented` record, so a harness can count them.
//! - `iosurface` is the id the worker presented (the guest's surface); `shown_iosurface` is the
//!   surface actually on the layer, which differs when the window shows a private copy.
//! - `t_monotonic_raw_ns` is `CLOCK_MONOTONIC_RAW` (= `mach_continuous_time` in ns; it keeps
//!   counting across sleep) and `t_realtime_ns` is `CLOCK_REALTIME`, both read on the main thread
//!   as the frame went on glass. The guest's RTC is anchored to the host's realtime clock, so the
//!   second is the one to correlate with guest logs.

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

/// One line of `frames.jsonl`: a frame that went on glass (captured or not), or a guest flip
/// that never did.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The guest display (pool slot) the frame belongs to.
    pub slot: usize,
    /// This capture's count of presents on `slot`, from 1. `None` for a flip never presented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The worker's flip count for `slot` within `epoch`.
    pub flip: u64,
    /// Which worker the flip came from; bumped by every reboot or resume.
    pub epoch: u64,
    /// The IOSurface id the worker presented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iosurface: Option<u32>,
    /// The IOSurface id on the layer: a private copy's when the window shows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_iosurface: Option<u32>,
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

impl Record {
    /// A guest flip on `slot` that never reached glass.
    pub fn not_presented(slot: usize, epoch: u64, flip: u64) -> Record {
        Record {
            slot,
            flip,
            epoch,
            dropped: true,
            reason: Some(Reason::NotPresented),
            ..Record::default()
        }
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Always `true`: what tells this line from a [`Record`].
    pub summary: bool,
    /// Frames a window put on glass while capturing.
    pub presented: u64,
    /// Of those, frames written to an image.
    pub captured: u64,
    /// Of those, frames with no image, by any reason but `not_presented`.
    pub dropped: u64,
    /// Guest flips that never reached glass.
    pub not_presented: u64,
    /// Every `dropped: true` record, by reason (`not_presented` included).
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
}

impl Summary {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("a summary always serializes")
    }

    /// One human line, for a log or a terminal.
    pub fn describe(&self) -> String {
        format!(
            "{} presented, {} captured, {} dropped, {} flips never presented, over {:.1} s \
             ({:.1} captured fps, {:.1} MB); main thread {} us p50 / {} us max per frame",
            self.presented,
            self.captured,
            self.dropped,
            self.not_presented,
            self.duration_s,
            self.captured_fps,
            self.bytes as f64 / 1e6,
            self.present_hook_us_p50,
            self.present_hook_us_max,
        )
    }
}

/// One parsed line of `frames.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Line {
    Summary(Summary),
    Frame(Record),
}

pub fn parse_line(line: &str) -> Result<Line, String> {
    serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))
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

impl Sequencer {
    /// A frame went on glass on `slot` as flip `flip` of worker `epoch`. Returns its `seq` and
    /// the flips of that worker since the previous present that were never shown.
    ///
    /// The first present of a capture has no predecessor, so nothing before it counts as missed.
    /// A new epoch's flips count from 1, so all of them before this one were missed. The same
    /// flip again (a re-show after a modeset, with no new frame) misses nothing.
    pub fn present(&mut self, slot: usize, epoch: u64, flip: u64) -> (u64, std::ops::Range<u64>) {
        let s = self.slots.entry(slot).or_default();
        s.seq += 1;
        let missed = match s.last {
            None => flip..flip,
            Some((e, f)) if e == epoch => (f + 1).min(flip)..flip,
            Some(_) => 1.min(flip)..flip,
        };
        s.last = Some((epoch, flip));
        (s.seq, missed)
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
            Some(reason) => *self.by_reason.entry(reason).or_default() += 1,
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
            iosurface: Some(40),
            shown_iosurface: Some(41),
            width: Some(2),
            height: Some(2),
            t_monotonic_raw_ns: Some(5),
            t_realtime_ns: Some(6),
            file: Some(file_name(slot, seq)),
            ..Record::default()
        }
    }

    #[test]
    fn the_sequence_counts_presents_and_names_the_flips_never_shown() {
        let mut s = Sequencer::default();
        // The first present of a capture misses nothing, whatever flip it is.
        assert_eq!(s.present(0, 1, 40), (1, 40..40));
        // Consecutive flips miss nothing.
        assert_eq!(s.present(0, 1, 41), (2, 41..41));
        assert!(s.present(0, 1, 42).1.is_empty());
        // Flips 43 and 44 were replaced before the window applied them.
        assert_eq!(s.present(0, 1, 45), (4, 43..45));
        // A re-show of the same flip (a modeset with no new frame) misses nothing.
        assert!(s.present(0, 1, 45).1.is_empty());
        // Another slot has its own count.
        assert_eq!(s.present(1, 1, 7), (1, 7..7));
        // A fresh worker counts flips from 1: its first three never reached glass.
        assert_eq!(s.present(0, 2, 4), (6, 1..4));
        // ... and a fresh worker whose first flip is shown misses nothing.
        assert!(s.present(1, 2, 1).1.is_empty());
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

        let never = Record::not_presented(1, 2, 17);
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
        t.count(&Record::not_presented(0, 1, 4), 0);
        t.count(&Record::not_presented(0, 1, 5), 0);
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
