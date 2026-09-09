//! The key=value field file the two `verify.sh` scripts derive from a stream's own parameter
//! sets with `ffmpeg -bsf:v trace_headers`.
//!
//! The serializer never sees the original bytes, only the parsed semantics — which is exactly
//! the position the backend is in, and the whole reason these oracles mean anything.

use std::collections::HashMap;
use std::path::Path;

pub struct Fields(HashMap<String, i64>);

impl Fields {
    pub fn load(path: &Path) -> Fields {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("{}: {e}", path.display());
            std::process::exit(1)
        });
        let mut map = HashMap::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            // A repeated key keeps its first value, matching the awk in verify.sh, which
            // already drops duplicates within a section.
            if let Ok(v) = v.trim().parse::<i64>() {
                map.entry(k.trim().to_string()).or_insert(v);
            }
        }
        Fields(map)
    }

    /// The value, or the default the backend would have used.
    ///
    /// A missing key is normal: `trace_headers` prints a field only when the stream codes it,
    /// and an absent field means the syntax default applies.
    pub fn get(&self, key: &str, default: i64) -> i64 {
        self.0.get(key).copied().unwrap_or(default)
    }

    pub fn flag(&self, key: &str, default: bool) -> bool {
        self.get(key, default as i64) != 0
    }

    pub fn u8(&self, key: &str, default: i64) -> u8 {
        self.get(key, default) as u8
    }

    pub fn i8(&self, key: &str, default: i64) -> i8 {
        self.get(key, default) as i8
    }

    pub fn u32(&self, key: &str, default: i64) -> u32 {
        self.get(key, default) as u32
    }
}

/// Write NAL payloads as an Annex-B prefix: a 4-byte start code before each.
pub fn write_annexb(path: &Path, nals: &[&[u8]]) {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    if let Err(e) = std::fs::write(path, &out) {
        eprintln!("{}: {e}", path.display());
        std::process::exit(1);
    }
}

/// `<fields> <width> <height> <out.bin>`, the CLI both `verify.sh` scripts call.
pub fn args(usage: &str) -> (Fields, u32, u32, std::path::PathBuf) {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 5 {
        eprintln!("usage: {usage} <fields> <width> <height> <out.bin>");
        std::process::exit(2);
    }
    let fields = Fields::load(Path::new(&argv[1]));
    let w: u32 = argv[2].parse().unwrap_or(0);
    let h: u32 = argv[3].parse().unwrap_or(0);
    (fields, w, h, std::path::PathBuf::from(&argv[4]))
}
