// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The video mode's movies: one HEVC QuickTime movie per display and size, each frame at its
//! real present time, so playback shows the guest's cadence as a human saw it.
//!
//! [`Movies`] lives on the capture's one video thread, which takes the copied frames in present
//! order: a writer input takes its frames from one thread with increasing times, so video does
//! not fan out over the PNG encoders. A frame is copied out of the capture buffer into a buffer
//! of the writer's own pool before it is appended — the writer keeps what it is given for as
//! long as it likes, and the capture buffer goes back to the capture's pool at once.
//!
//! Every AVFoundation call that can raise an Objective-C exception runs inside
//! [`objc2::exception::catch`]: one unwinding into Rust would abort the supervisor, VM and all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use block2::RcBlock;
use limina_framecap::{PixelOrder, Reason};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeQuickTimeMovie, AVMediaTypeVideo, AVVideoAllowFrameReorderingKey, AVVideoCodecKey,
    AVVideoCodecTypeHEVC, AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey,
    AVVideoHeightKey, AVVideoQualityKey, AVVideoWidthKey,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::CMTime;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

/// The movies' time scale: microseconds. A frame's time in the movie, and the `pts_ns` its
/// record carries, are both whole microseconds, so the two agree exactly.
const TIMESCALE: i32 = 1_000_000;

/// VideoToolbox's quality knob, 0 to 1. Screen content (text, thin lines) smears visibly well
/// above the default; this keeps a desktop's text readable at a few MB a minute.
const QUALITY: f64 = 0.75;

/// How long finishing one movie may take before it is left as it is.
const FINISH: Duration = Duration::from_secs(10);

/// One frame's pixels as the capture buffer holds them: `h` rows of `bpr` bytes, 4 per pixel.
pub(super) struct Frame<'a> {
    pub(super) pixels: &'a [u8],
    pub(super) w: usize,
    pub(super) h: usize,
    pub(super) bpr: usize,
    pub(super) order: PixelOrder,
}

/// Where a frame went: its movie and its time in it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Placed {
    pub(super) file: String,
    pub(super) pts_ns: u64,
}

/// The movie being written for one display.
struct Movie {
    file: String,
    path: PathBuf,
    geom: (usize, usize),
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    /// The present time of the movie's first frame, which is its time 0.
    first_ns: u64,
    /// The last time given, in [`TIMESCALE`] units: the next must be later.
    last_us: Option<i64>,
}

/// Every display's movie, and what the finished ones came to.
pub(super) struct Movies {
    dir: PathBuf,
    open: HashMap<usize, Movie>,
    /// Per display, the next movie's number.
    next: HashMap<usize, u32>,
    /// Bytes of the movies finished so far.
    finished: u64,
}

/// Run `f`, turning an Objective-C exception into an error naming it. Whatever `f` touched is
/// not used again after one: the movie it was writing is finished or dropped.
fn guarded<R>(what: &str, f: impl FnOnce() -> R) -> Result<R, String> {
    objc2::exception::catch(std::panic::AssertUnwindSafe(f)).map_err(|e| match e {
        Some(e) => format!("{what}: {e:?}"),
        None => format!("{what}: an exception"),
    })
}

/// A CoreVideo key as the `NSString` it is toll-free bridged to.
fn ns_key(k: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged: the same object.
    unsafe { &*(k as *const CFString as *const NSString) }
}

fn key(k: Option<&'static NSString>, name: &str) -> Result<&'static NSString, String> {
    k.ok_or_else(|| format!("AVFoundation has no {name}"))
}

impl Movies {
    pub(super) fn new(dir: &Path) -> Self {
        Movies {
            dir: dir.to_path_buf(),
            open: HashMap::new(),
            next: HashMap::new(),
            finished: 0,
        }
    }

    /// Bytes on disk now: the finished movies, and the open ones as far as written. Only what
    /// the disk budget is checked against; the summary counts finished movies alone.
    pub(super) fn bytes_now(&self) -> u64 {
        self.finished
            + self
                .open
                .values()
                .filter_map(|m| std::fs::metadata(&m.path).ok())
                .map(|md| md.len())
                .sum::<u64>()
    }

    /// Append `frame`, presented on `slot` at `t_ns` (`CLOCK_MONOTONIC_RAW`), to the slot's
    /// movie: a new one when the slot has none or the frame's size differs from it.
    pub(super) fn append(
        &mut self,
        slot: usize,
        t_ns: u64,
        frame: &Frame,
    ) -> Result<Placed, Reason> {
        if frame.order != PixelOrder::Bgra {
            // Every surface the worker makes is BGRA; a movie of swapped channels would lie.
            return Err(Reason::CopyFailed);
        }
        let geom = (frame.w, frame.h);
        if self.open.get(&slot).is_some_and(|m| m.geom != geom) {
            let movie = self.open.remove(&slot).expect("just seen");
            self.finished += finish(movie);
        }
        if !self.open.contains_key(&slot) {
            let n = self.next.entry(slot).or_default();
            let file = limina_framecap::video_file_name(slot, *n);
            *n += 1;
            let movie = Movie::start(&self.dir, file, geom, t_ns).map_err(|e| {
                log::warn!("frame capture: {e}");
                Reason::WriteError
            })?;
            self.open.insert(slot, movie);
        }
        let movie = self.open.get_mut(&slot).expect("just opened");
        match movie.append(t_ns, frame) {
            Ok(placed) => Ok(placed),
            Err(Reason::QueueFull) => Err(Reason::QueueFull),
            Err(reason) => {
                // The writer has failed: what it wrote is kept, and the next frame starts a
                // new movie.
                let movie = self.open.remove(&slot).expect("just used");
                self.finished += finish(movie);
                Err(reason)
            }
        }
    }

    /// Finish every movie; the bytes of all the movies written.
    pub(super) fn finish_all(&mut self) -> u64 {
        for (_, movie) in self.open.drain() {
            self.finished += finish(movie);
        }
        self.finished
    }
}

impl Movie {
    fn start(
        dir: &Path,
        file: String,
        geom: (usize, usize),
        first_ns: u64,
    ) -> Result<Movie, String> {
        let path = dir.join(&file);
        let (w, h) = (geom.0 as i32, geom.1 as i32);
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        // SAFETY: the statics are AVFoundation's own constants.
        let (file_type, video) = unsafe { (AVFileTypeQuickTimeMovie, AVMediaTypeVideo) };
        let file_type = key(file_type, "AVFileTypeQuickTimeMovie")?;
        let video = key(video, "AVMediaTypeVideo")?;
        // SAFETY: as above.
        let (codec_key, hevc, width_key, height_key, props_key, quality_key, reorder_key, fps_key) = unsafe {
            (
                key(AVVideoCodecKey, "AVVideoCodecKey")?,
                key(AVVideoCodecTypeHEVC, "AVVideoCodecTypeHEVC")?,
                key(AVVideoWidthKey, "AVVideoWidthKey")?,
                key(AVVideoHeightKey, "AVVideoHeightKey")?,
                key(
                    AVVideoCompressionPropertiesKey,
                    "AVVideoCompressionPropertiesKey",
                )?,
                key(AVVideoQualityKey, "AVVideoQualityKey")?,
                key(
                    AVVideoAllowFrameReorderingKey,
                    "AVVideoAllowFrameReorderingKey",
                )?,
                key(
                    AVVideoExpectedSourceFrameRateKey,
                    "AVVideoExpectedSourceFrameRateKey",
                )?,
            )
        };
        let quality = NSNumber::new_f64(QUALITY);
        // No B-frames: frames go out in the order they come, and a capture stopped mid-stream
        // loses nothing it has appended to reordering.
        let no = NSNumber::new_bool(false);
        let fps = NSNumber::new_i32(60);
        let props: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[quality_key, reorder_key, fps_key],
            &[quality.as_ref(), no.as_ref(), fps.as_ref()],
        );
        let (wn, hn) = (NSNumber::new_i32(w), NSNumber::new_i32(h));
        let settings: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[codec_key, width_key, height_key, props_key],
            &[hevc.as_ref(), wn.as_ref(), hn.as_ref(), props.as_ref()],
        );
        let bgra = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
        // SAFETY: CoreVideo's own constants.
        let (fmt_key, cvw_key, cvh_key) = unsafe {
            (
                ns_key(kCVPixelBufferPixelFormatTypeKey),
                ns_key(kCVPixelBufferWidthKey),
                ns_key(kCVPixelBufferHeightKey),
            )
        };
        let attrs: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[fmt_key, cvw_key, cvh_key],
            &[bgra.as_ref(), wn.as_ref(), hn.as_ref()],
        );
        let what = format!("starting {}", path.display());
        guarded(&what, || -> Result<Movie, String> {
            // SAFETY: a file URL and a type AVFoundation declares; the dictionaries hold the
            // value types each key documents.
            unsafe {
                let writer = AVAssetWriter::initWithURL_fileType_error(
                    AVAssetWriter::alloc(),
                    &url,
                    file_type,
                )
                .map_err(|e| format!("{e:?}"))?;
                let input = AVAssetWriterInput::initWithMediaType_outputSettings(
                    AVAssetWriterInput::alloc(),
                    video,
                    Some(&settings),
                );
                input.setExpectsMediaDataInRealTime(true);
                input.setMediaTimeScale(TIMESCALE);
                let adaptor =
                    AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(
                        AVAssetWriterInputPixelBufferAdaptor::alloc(),
                        &input,
                        Some(&attrs),
                    );
                if !writer.canAddInput(&input) {
                    return Err("the writer refuses an HEVC input".into());
                }
                writer.addInput(&input);
                if !writer.startWriting() {
                    return Err(format!("{:?}", writer.error()));
                }
                writer.startSessionAtSourceTime(CMTime::new(0, TIMESCALE));
                Ok(Movie {
                    file,
                    path: path.clone(),
                    geom,
                    writer,
                    input,
                    adaptor,
                    first_ns,
                    last_us: None,
                })
            }
        })
        .and_then(|r| r)
        .map_err(|e| format!("{what}: {e}"))
    }

    fn append(&mut self, t_ns: u64, frame: &Frame) -> Result<Placed, Reason> {
        // SAFETY: plain property reads on our own input and writer.
        let ready = unsafe { self.input.isReadyForMoreMediaData() };
        if !ready {
            // Appending now would raise; the encoder is behind, as the PNG path's queue can be.
            return Err(Reason::QueueFull);
        }
        // SAFETY: as above; the pool exists once writing has started.
        let Some(pool) = (unsafe { self.adaptor.pixelBufferPool() }) else {
            return Err(Reason::WriteError);
        };
        let buffer = pool_buffer(&pool).ok_or(Reason::WriteError)?;
        copy_into(&buffer, frame).ok_or(Reason::CopyFailed)?;
        // Strictly later than the last, or the writer refuses it.
        let mut us = (t_ns.saturating_sub(self.first_ns) / 1000) as i64;
        if let Some(last) = self.last_us
            && us <= last
        {
            us = last + 1;
        }
        let (adaptor, buf) = (&self.adaptor, &buffer);
        let appended = guarded("appending a frame", || {
            // SAFETY: a ready input, a buffer of the size the input was made for, and a
            // numeric time later than the last.
            unsafe {
                adaptor.appendPixelBuffer_withPresentationTime(buf, CMTime::new(us, TIMESCALE))
            }
        });
        match appended {
            Ok(true) => {
                self.last_us = Some(us);
                Ok(Placed {
                    file: self.file.clone(),
                    pts_ns: us as u64 * 1000,
                })
            }
            Ok(false) => {
                // SAFETY: a property read.
                let e = unsafe { self.writer.error() };
                log::warn!("frame capture: {}: append failed: {e:?}", self.file);
                Err(Reason::WriteError)
            }
            Err(e) => {
                log::warn!("frame capture: {}: {e}", self.file);
                Err(Reason::WriteError)
            }
        }
    }
}

/// A buffer from the writer's pool. Ours until appended.
fn pool_buffer(pool: &CVPixelBufferPool) -> Option<CFRetained<CVPixelBuffer>> {
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: a valid pool and somewhere to put the buffer, which follows the create rule.
    unsafe {
        if CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut out))
            != kCVReturnSuccess
        {
            return None;
        }
        NonNull::new(out).map(|p| CFRetained::from_raw(p))
    }
}

/// Copy `frame`'s rows into `buffer`, which must be its size.
fn copy_into(buffer: &CVPixelBuffer, frame: &Frame) -> Option<()> {
    let flags = CVPixelBufferLockFlags::empty();
    if CVPixelBufferGetWidth(buffer) != frame.w || CVPixelBufferGetHeight(buffer) != frame.h {
        return None;
    }
    // SAFETY: a locked buffer's base address is valid for its rows until the unlock; each row
    // copied is `w * 4` bytes, inside both the source row and the destination row.
    unsafe {
        if CVPixelBufferLockBaseAddress(buffer, flags) != kCVReturnSuccess {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(buffer) as *mut u8;
        let dst_bpr = CVPixelBufferGetBytesPerRow(buffer);
        let row = frame.w * 4;
        let fits = !base.is_null()
            && dst_bpr >= row
            && frame.bpr >= row
            && frame.pixels.len() >= (frame.h - 1) * frame.bpr + row;
        if fits {
            for y in 0..frame.h {
                std::ptr::copy_nonoverlapping(
                    frame.pixels.as_ptr().add(y * frame.bpr),
                    base.add(y * dst_bpr),
                    row,
                );
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, flags);
        fits.then_some(())
    }
}

/// Finish `movie` and wait for it, at most [`FINISH`]; the bytes it came to.
fn finish(movie: Movie) -> u64 {
    let (tx, rx) = sync_channel::<()>(1);
    let done = RcBlock::new(move || {
        let _ = tx.try_send(());
    });
    let (input, writer) = (&movie.input, &movie.writer);
    let started = guarded("finishing a movie", || {
        // SAFETY: nothing else appends to this input, and the handler only signals a channel.
        unsafe {
            input.markAsFinished();
            writer.finishWritingWithCompletionHandler(&done);
        }
    });
    let deadline = Instant::now() + FINISH;
    let outcome = match started {
        Err(e) => Err(e),
        Ok(()) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Err(_) => Err(format!("not finished after {FINISH:?}")),
            // SAFETY: a property read once the writer is done.
            Ok(()) => match unsafe { movie.writer.status() } {
                AVAssetWriterStatus::Completed => Ok(()),
                status => Err(format!("{status:?}: {:?}", unsafe { movie.writer.error() })),
            },
        },
    };
    if let Err(e) = outcome {
        log::warn!("frame capture: {}: {e}", movie.file);
    }
    std::fs::metadata(&movie.path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` x `h` BGRA frame of one colour.
    fn solid(w: usize, h: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat(w * h)
    }

    fn frame(pixels: &[u8], w: usize, h: usize) -> Frame<'_> {
        Frame {
            pixels,
            w,
            h,
            bpr: w * 4,
            order: PixelOrder::Bgra,
        }
    }

    /// Appending as fast as the test can risks `queue_full`; give the encoder its turn.
    fn append_ready(m: &mut Movies, slot: usize, t_ns: u64, f: &Frame) -> Placed {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match m.append(slot, t_ns, f) {
                Ok(p) => return p,
                Err(Reason::QueueFull) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(r) => panic!("append: {r:?}"),
            }
        }
    }

    #[test]
    fn frames_land_at_their_present_times_and_a_new_size_starts_a_new_movie() {
        let dir = std::env::temp_dir().join(format!("limina-video-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut movies = Movies::new(&dir);
        let small = solid(320, 200, [0, 0, 255, 255]);
        let t0 = 5_000_000_000;
        // A writer that cannot start (no hardware encoder on a CI runner's VM) skips the test:
        // the record, wire and CLI tests are what runs everywhere.
        let first = match movies.append(0, t0, &frame(&small, 320, 200)) {
            Ok(p) => p,
            Err(r) => {
                eprintln!("skipped: no movie writer here ({r:?})");
                return;
            }
        };
        assert_eq!(
            first,
            Placed {
                file: "s0-000.mov".into(),
                pts_ns: 0
            }
        );
        // 16.6667 ms later, kept to the microsecond.
        let p = append_ready(&mut movies, 0, t0 + 16_666_700, &frame(&small, 320, 200));
        assert_eq!(p.pts_ns, 16_666_000);
        // The same microsecond again still moves forward.
        let p = append_ready(&mut movies, 0, t0 + 16_666_900, &frame(&small, 320, 200));
        assert_eq!(p.pts_ns, 16_667_000);
        // Another display has its own movie and its own time 0.
        let p = append_ready(&mut movies, 1, t0 + 40_000_000, &frame(&small, 320, 200));
        assert_eq!((p.file.as_str(), p.pts_ns), ("s1-000.mov", 0));
        // A resize finishes the movie and starts the next.
        let big = solid(640, 400, [0, 255, 0, 255]);
        let p = append_ready(&mut movies, 0, t0 + 50_000_000, &frame(&big, 640, 400));
        assert_eq!((p.file.as_str(), p.pts_ns), ("s0-001.mov", 0));
        let bytes = movies.finish_all();
        let sizes: Vec<u64> = ["s0-000.mov", "s0-001.mov", "s1-000.mov"]
            .iter()
            .map(|f| std::fs::metadata(dir.join(f)).map(|m| m.len()).unwrap_or(0))
            .collect();
        assert!(
            sizes.iter().all(|&n| n > 0),
            "every movie written: {sizes:?}"
        );
        assert_eq!(bytes, sizes.iter().sum::<u64>());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_swapped_channel_frame_is_refused_not_written() {
        let dir = std::env::temp_dir().join(format!("limina-video-rgba-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut movies = Movies::new(&dir);
        let px = solid(16, 16, [1, 2, 3, 4]);
        let f = Frame {
            order: PixelOrder::Rgba,
            ..frame(&px, 16, 16)
        };
        assert_eq!(movies.append(0, 1, &f), Err(Reason::CopyFailed));
        assert_eq!(movies.finish_all(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
