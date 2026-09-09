//! Rebuild an AV1 stream from captured picture descriptors, using the shipping Rust serializer.
//!
//! The serializer's job is to reconstruct a frame header that was destroyed at the guest's
//! decoder -> VA-API boundary, from the parsed descriptor alone. Nothing about that is checkable
//! by inspection: a header is bit-packed, so one wrong value shifts everything after it and the
//! failure surfaces as noise somewhere else entirely.
//!
//! This binary is only the serializer half. It writes the rebuilt stream to a file, and
//! `spikes/av1-obu-serializer/oracle` decodes that against the original with dav1d and compares
//! pixels — the split exists so the dav1d harness, which is the expensive and codec-agnostic
//! part, is written once and grades either implementation:
//!
//!     av1-rebuild capture/baseline rebuilt.obu
//!     AV1_ORACLE_STREAM=rebuilt.obu ./oracle capture/baseline clips/baseline.obu
//!
//! `AV1_REBUILD_CONTRACT=1` drives the serializer the way a buggy backend would — building each
//! frame without ever flushing the held one — and checks that it REFUSES rather than quietly
//! losing a picture. Worth a mode of its own because the normal path always flushes, so nothing
//! else here ever reaches that guard.

use std::path::{Path, PathBuf};
use virglrenderer::vrend::video::av1::{FrameDesc, ObuState};

fn slurp(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() != 3 {
        eprintln!("usage: av1-rebuild <capture-dir> <out.obu>");
        eprintln!();
        eprintln!("  capture-dir  frameNNNNN.desc/.tile written by a LIMINA_AV1_CAPTURE run");
        eprintln!("  out.obu      the rebuilt stream, low-overhead OBU framing");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&argv[1]);
    let out = PathBuf::from(&argv[2]);
    let contract = std::env::var_os("AV1_REBUILD_CONTRACT").is_some();

    let mut state: ObuState = ObuState::new();
    let mut stream: Vec<u8> = Vec::new();
    let mut frames = 0u32;
    let mut held_seen = false;

    for i in 0u32.. {
        let Some(blob) = slurp(&dir.join(format!("frame{i:05}.desc"))) else {
            break;
        };
        let desc = match FrameDesc::read(&blob) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("frame {i}: the serializer refused the descriptor: {e:?}");
                std::process::exit(1);
            }
        };
        let tiles = slurp(&dir.join(format!("frame{i:05}.tile"))).unwrap_or_default();

        if contract {
            // Deliberately never flush: the second frame must be refused.
            match state.build_temporal_unit(&desc, &tiles, ()) {
                Err(_) => {
                    if held_seen {
                        println!("PASS: the serializer refused to build over a held frame");
                        return;
                    }
                    println!("FAIL: refused at frame {i} with nothing held");
                    std::process::exit(1);
                }
                Ok(None) => held_seen = true,
                Ok(Some(bytes)) => {
                    if held_seen {
                        println!(
                            "FAIL: frame {i} built over a held frame instead of refusing -- \
                             the held picture is lost"
                        );
                        std::process::exit(1);
                    }
                    stream.extend_from_slice(&bytes);
                }
            }
            frames += 1;
            continue;
        }

        // Twice on purpose. A frame's tile data can arrive over several decode_bitstream calls,
        // each carrying the same descriptor, so the flush has to be idempotent within a frame --
        // a second unit here would be a second picture the guest never asked for.
        for call in 0..2 {
            if let Some((unit, ())) = state.flush_held(&desc) {
                if call == 1 {
                    eprintln!(
                        "frame {i}: flushing twice emitted a second temporal unit ({} bytes)",
                        unit.bytes.len()
                    );
                    std::process::exit(1);
                }
                stream.extend_from_slice(&unit.bytes);
            }
        }

        match state.build_temporal_unit(&desc, &tiles, ()) {
            Ok(Some(bytes)) => stream.extend_from_slice(&bytes),
            // A hidden frame is being held: nothing goes out this submission, and the unit
            // arrives on the next one.
            Ok(None) => {}
            Err(_) => {
                eprintln!("frame {i}: the serializer refused to build a temporal unit");
                std::process::exit(1);
            }
        }
        frames += 1;
    }

    if contract {
        println!("SKIP: no frame was ever held, so the guard was not reached");
        return;
    }

    // A hidden frame may still be held: it waits one submission so its refresh can be derived
    // from the next descriptor, and after the last one there is no next.
    if let Some((unit, ())) = state.flush_temporal_unit() {
        stream.extend_from_slice(&unit.bytes);
    }

    if frames == 0 {
        eprintln!("no fixtures in {}", dir.display());
        std::process::exit(1);
    }

    if let Err(e) = std::fs::write(&out, &stream) {
        eprintln!("{}: {e}", out.display());
        std::process::exit(1);
    }
    println!(
        "rebuilt {frames} frames into {} bytes -> {}",
        stream.len(),
        out.display()
    );
}
