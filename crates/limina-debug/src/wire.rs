// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The debug socket's line protocol.
//!
//! One request per line; the answer is any number of report lines, then exactly one `ok` or
//! `err <why>` line that ends it:
//!
//! ```text
//! > status
//! < log supervisor warn,limina::window=debug
//! < log worker warn
//! < lever edge-trace on LIMINA_EDGE_TRACE grab/edge/click decisions, every pointer event
//! < ok
//! > log all info
//! < ok
//! > lever edge-trace off
//! < ok
//! > lever nope on
//! < err no lever named nope
//! ```
//!
//! The supervisor speaks the same protocol to its worker, where only `log` means anything (the
//! scope is then ignored: the worker has one filter).

/// Which process a `log` request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Supervisor,
    Worker,
    All,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Supervisor => "supervisor",
            Scope::Worker => "worker",
            Scope::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "supervisor" => Some(Scope::Supervisor),
            "worker" => Some(Scope::Worker),
            "all" => Some(Scope::All),
            _ => None,
        }
    }

    pub fn includes_supervisor(self) -> bool {
        matches!(self, Scope::Supervisor | Scope::All)
    }

    pub fn includes_worker(self) -> bool {
        matches!(self, Scope::Worker | Scope::All)
    }
}

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Report the filters and every lever.
    Status,
    /// Replace a filter (`RUST_LOG` syntax, or `default`).
    Log { scope: Scope, spec: String },
    /// Turn a lever on or off.
    Lever { name: String, on: bool },
}

impl Request {
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("status") => Ok(Request::Status),
            Some("log") => {
                let scope = words.next().ok_or("log needs a scope and a filter")?;
                let scope = Scope::parse(scope)
                    .ok_or_else(|| format!("unknown scope {scope} (supervisor, worker, all)"))?;
                // A filter never contains whitespace; anything after it is a mistake to report,
                // not to drop.
                let spec = words.next().ok_or("log needs a filter")?;
                if let Some(extra) = words.next() {
                    return Err(format!("unexpected {extra:?} after the filter"));
                }
                Ok(Request::Log {
                    scope,
                    spec: spec.to_string(),
                })
            }
            Some("lever") => {
                let name = words.next().ok_or("lever needs a name and on|off")?;
                let on = match words.next() {
                    Some("on") => true,
                    Some("off") => false,
                    Some(other) => {
                        return Err(format!("lever state must be on or off, not {other}"));
                    }
                    None => return Err("lever needs on or off".into()),
                };
                Ok(Request::Lever {
                    name: name.to_string(),
                    on,
                })
            }
            Some(other) => Err(format!("unknown request {other}")),
            None => Err("empty request".into()),
        }
    }

    pub fn to_line(&self) -> String {
        match self {
            Request::Status => "status".into(),
            Request::Log { scope, spec } => format!("log {} {spec}", scope.as_str()),
            Request::Lever { name, on } => {
                format!("lever {name} {}", if *on { "on" } else { "off" })
            }
        }
    }
}

/// The line that ends a successful answer.
pub const OK: &str = "ok";

/// The line that ends a failed one.
pub fn err_line(why: &str) -> String {
    // One line, always: a newline inside the reason would end the answer early.
    format!("err {}", why.replace('\n', " "))
}

/// Read one answer: the report lines, or the reason it failed.
pub fn read_answer(reader: &mut impl std::io::BufRead) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("the connection closed before the answer ended".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("reading the answer: {e}")),
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line == OK {
            return Ok(lines);
        }
        if let Some(why) = line.strip_prefix("err ") {
            return Err(why.to_string());
        }
        lines.push(line.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::Status,
            Request::Log {
                scope: Scope::All,
                spec: "warn,limina::window=debug".into(),
            },
            Request::Log {
                scope: Scope::Worker,
                spec: "default".into(),
            },
            Request::Lever {
                name: "edge-trace".into(),
                on: true,
            },
            Request::Lever {
                name: "input-trace".into(),
                on: false,
            },
        ] {
            assert_eq!(Request::parse(&r.to_line()), Ok(r));
        }
    }

    #[test]
    fn malformed_requests_say_what_is_wrong() {
        assert!(Request::parse("").is_err());
        assert!(Request::parse("log").is_err());
        assert!(Request::parse("log everyone info").is_err());
        assert!(Request::parse("log all").is_err());
        assert!(Request::parse("log all info debug").is_err());
        assert!(Request::parse("lever edge-trace").is_err());
        assert!(Request::parse("lever edge-trace maybe").is_err());
        assert!(Request::parse("reboot").is_err());
    }

    #[test]
    fn an_answer_ends_at_ok_or_err() {
        let mut ok = "log supervisor warn\nlever a on\nok\nleftover\n".as_bytes();
        assert_eq!(
            read_answer(&mut ok),
            Ok(vec![
                "log supervisor warn".to_string(),
                "lever a on".to_string()
            ])
        );
        let mut err = "err no lever named nope\n".as_bytes();
        assert_eq!(
            read_answer(&mut err),
            Err("no lever named nope".to_string())
        );
        let mut cut = "log supervisor warn\n".as_bytes();
        assert!(read_answer(&mut cut).is_err());
    }

    #[test]
    fn an_error_is_always_one_line() {
        assert_eq!(err_line("a\nb"), "err a b");
    }
}
