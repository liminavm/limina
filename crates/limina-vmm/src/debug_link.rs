// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The worker's end of the debug socket: the supervisor changes this process's log filter here.
//!
//! The supervisor owns the user-facing socket (`limina debug`, the Debug menu) and forwards what
//! is meant for the worker over a link (`--debug-control-fd`), in the same line protocol
//! (`limina_debug::wire`). Only `log` and `status` mean anything here; the worker's diagnostic
//! levers live inside libkrun and are not switchable yet.
//!
//! A replacement worker (a guest reboot, a resume) starts from the filter the supervisor last
//! set, through its `RUST_LOG` — not by being told again here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use limina_debug::wire::{self, Request};
use limina_launch::connect::ListenAt;

pub(crate) fn install(at: ListenAt) -> Result<()> {
    let place = at.to_string();
    let listener = at
        .listen()
        .with_context(|| format!("listening for debug control at {place}"))?;
    std::thread::Builder::new()
        .name("debug-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => serve(s),
                    Err(e) => log::error!("debug-control: accept failed: {e}"),
                }
            }
        })
        .context("spawning the debug-control listener thread")?;
    Ok(())
}

fn serve(stream: UnixStream) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let answer = answer(&line);
        if out.write_all(answer.as_bytes()).is_err() {
            break;
        }
    }
}

/// Everything written back for one request line, terminator included.
fn answer(line: &str) -> String {
    match Request::parse(line) {
        Ok(Request::Status) => format!(
            "log worker {}\n{}\n",
            limina_debug::logger::filter_spec(),
            wire::OK
        ),
        Ok(Request::Log { spec, .. }) => match limina_debug::logger::set_filter(&spec) {
            Ok(()) => {
                // At warn, so the change is on record under the filter it just replaced as well
                // as the new one: a log that suddenly gets quieter says why.
                log::warn!(
                    "debug-control: log filter is now {}",
                    limina_debug::logger::filter_spec()
                );
                format!("{}\n", wire::OK)
            }
            Err(e) => format!("{}\n", wire::err_line(&e)),
        },
        Ok(Request::Lever { name, .. }) => format!(
            "{}\n",
            wire::err_line(&format!("the worker has no lever named {name}"))
        ),
        Ok(Request::Capture(_)) => format!(
            "{}\n",
            wire::err_line("frame capture is the supervisor's: it captures what its windows show")
        ),
        Err(e) => format!("{}\n", wire::err_line(&e)),
    }
}
