// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A `RUST_LOG` filter that can be replaced while the process runs.
//!
//! `env_logger` reads its filter once, at init. So the filter lives here instead: the installed
//! logger is an `env_logger::Logger` that passes everything, behind a [`env_filter::Filter`] that
//! [`set_filter`] swaps.
//!
//! Swapping the filter alone would not raise anything. The `log` macros compare against a global
//! ceiling (`log::max_level`) before they reach any logger, and init leaves that at the starting
//! filter's level — `warn` by default — so an `info!` would be discarded before the new filter
//! saw it. Every change sets the ceiling to the new filter's own level.

use std::sync::{OnceLock, RwLock};

use log::{LevelFilter, Log, Metadata, Record};

/// What `RUST_LOG` means when it is unset or empty, in both host processes.
pub const DEFAULT_SPEC: &str = "warn";

/// The installed filter and the text it was parsed from.
struct Current {
    spec: String,
    filter: env_filter::Filter,
}

static CURRENT: RwLock<Option<Current>> = RwLock::new(None);
static STARTUP: OnceLock<String> = OnceLock::new();

/// Install the logger, starting from `RUST_LOG` (or [`DEFAULT_SPEC`]).
///
/// `configure` sets everything but the filter on the `env_logger` builder — format, timestamps,
/// target — exactly as the caller set it before. Its filter is left wide open on purpose: this
/// one does the filtering, and a module directive left on the inner logger would cap that module
/// below whatever [`set_filter`] later asks for.
pub fn init(configure: impl FnOnce(&mut env_logger::Builder)) {
    let spec = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SPEC.to_string());
    // Lenient, as `env_logger` was: a bad directive in the environment is reported on stderr and
    // skipped, never fatal at startup.
    let filter = env_filter::Builder::new().parse(&spec).build();
    let _ = STARTUP.set(spec.clone());
    let level = filter.filter();
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Current { spec, filter });

    let mut builder = env_logger::Builder::new();
    builder.filter_level(LevelFilter::Trace);
    if let Ok(style) = std::env::var("RUST_LOG_STYLE") {
        builder.parse_write_style(&style);
    }
    configure(&mut builder);
    let inner = builder.build();
    if log::set_boxed_logger(Box::new(Reloadable { inner })).is_ok() {
        log::set_max_level(level);
    }
}

/// Replace the filter. `spec` is `RUST_LOG` syntax; `default` goes back to the one the process
/// started with. A spec that does not parse changes nothing.
pub fn set_filter(spec: &str) -> Result<(), String> {
    let spec = spec.trim();
    let spec = if spec == "default" {
        STARTUP.get().map_or(DEFAULT_SPEC, String::as_str)
    } else {
        spec
    };
    let filter = parse(spec)?;
    let level = filter.filter();
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Current {
        spec: spec.to_string(),
        filter,
    });
    log::set_max_level(level);
    Ok(())
}

/// The filter in force, as text.
pub fn filter_spec() -> String {
    CURRENT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or_else(|| DEFAULT_SPEC.to_string(), |c| c.spec.clone())
}

/// The filter the process started with.
pub fn startup_spec() -> String {
    STARTUP
        .get()
        .cloned()
        .unwrap_or_else(|| DEFAULT_SPEC.to_string())
}

/// Parse `RUST_LOG` syntax strictly: a runtime change that does not parse is refused rather than
/// half-applied.
pub fn parse(spec: &str) -> Result<env_filter::Filter, String> {
    if spec.is_empty() {
        return Err("empty filter (use `default` to go back to the starting one)".into());
    }
    env_filter::Builder::new()
        .try_parse(spec)
        .map(|b| b.build())
        .map_err(|e| format!("bad filter {spec:?}: {e}"))
}

struct Reloadable {
    inner: env_logger::Logger,
}

impl Log for Reloadable {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        CURRENT
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|c| c.filter.enabled(metadata))
    }

    fn log(&self, record: &Record<'_>) {
        let pass = CURRENT
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|c| c.filter.matches(record));
        if pass {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(f: &env_filter::Filter, target: &str, level: log::Level) -> bool {
        f.enabled(&Metadata::builder().target(target).level(level).build())
    }

    #[test]
    fn a_module_directive_raises_only_that_module() {
        let f = parse("warn,limina::window=debug").unwrap();
        assert!(enabled(
            &f,
            "limina::window::capture_tap",
            log::Level::Debug
        ));
        assert!(!enabled(&f, "limina::session", log::Level::Info));
        assert!(!enabled(&f, "krun_devices::virtio", log::Level::Info));
        // The ceiling the macros check has to admit the raised module.
        assert_eq!(f.filter(), LevelFilter::Debug);
    }

    #[test]
    fn a_bad_filter_is_refused() {
        assert!(parse("limina=loud").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn set_filter_rejects_garbage_and_keeps_the_old_filter() {
        // `set_filter` is process-global; this test owns it.
        set_filter("info").unwrap();
        assert!(set_filter("limina=loud").is_err());
        assert_eq!(filter_spec(), "info");
        set_filter("debug").unwrap();
        assert_eq!(filter_spec(), "debug");
        assert_eq!(log::max_level(), LevelFilter::Debug);
        set_filter("default").unwrap();
        assert_eq!(filter_spec(), startup_spec());
    }
}
