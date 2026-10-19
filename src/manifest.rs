//! The machine-readable side: `--json` event lines, the run manifest, SHA-256 hashes, UTC timestamps.
//!
//! **Event lines** (`--json`): one JSON object per line on stdout, each with `"v": 1` (the schema version)
//! and an `"event"` name: `batch_start`, `frame_planned` (dry run), `frame_done`, `frame_failed`,
//! `batch_end`, and `error` (a refusal, bad arguments or a missing tool, before or instead of a batch).
//! Human-readable text goes to stderr while `--json` is on. The fields are documented in
//! `docs/JSON.md`.
//!
//! **Manifest**: `<out>/rawdev-manifest.json` records every run that wrote into the folder: the rawdev
//! version, the tools and their versions, the look (name, origin, SHA-256 of its TOML text) and, per
//! frame, the options used and the SHA-256 of the input RAW and of every output. Each run appends one
//! entry to `runs`; the file is replaced atomically (written to a temp file, then renamed).

use crate::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA: u32 = 1;
pub const MANIFEST_NAME: &str = "rawdev-manifest.json";

/// SHA-256 of a file, as lowercase hex (streams it; never loads the whole file).
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn sha256_bytes(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Now, as `2026-10-18T19:04:05Z`.
pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    utc(secs)
}

/// Unix seconds -> RFC 3339 UTC (Howard Hinnant's days-to-civil algorithm; no date crate needed).
pub fn utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Where event lines go. With `--json` they are printed to stdout; either way nothing is printed when
/// `json` is false (the human log is separate).
pub struct Events {
    pub json: bool,
}

impl Events {
    /// Print one event line (adds `"v"` and `"event"`).
    pub fn emit(&self, event: &str, mut body: Value) {
        if !self.json {
            return;
        }
        if let Value::Object(m) = &mut body {
            let mut out = serde_json::Map::new();
            out.insert("v".into(), json!(SCHEMA));
            out.insert("event".into(), json!(event));
            out.append(m);
            let line = Value::Object(out).to_string();
            let mut so = std::io::stdout().lock();
            let _ = writeln!(so, "{line}");
            let _ = so.flush();
        }
    }
}

/// Append one run to `<out>/rawdev-manifest.json` (creating it if needed). Atomic replace.
pub fn append_run(out: &Path, run: Value) -> Result<()> {
    let path = out.join(MANIFEST_NAME);
    let mut doc = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.get("runs").is_some_and(Value::is_array) => v,
            _ => {
                return Err(format!(
                    "{} exists but is not a rawdev manifest; move it away first",
                    path.display()
                )
                .into());
            }
        }
    } else {
        json!({ "rawdev_manifest": SCHEMA, "runs": [] })
    };
    doc["runs"].as_array_mut().unwrap().push(run);
    let tmp = out.join(format!(".{MANIFEST_NAME}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_792_350_245), "2026-10-18T19:04:05Z");
    }

    #[test]
    fn sha256_known() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_appends() {
        let d = std::env::temp_dir().join(format!("rawdev-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        append_run(&d, json!({"n": 1})).unwrap();
        append_run(&d, json!({"n": 2})).unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(d.join(MANIFEST_NAME)).unwrap()).unwrap();
        assert_eq!(v["runs"].as_array().unwrap().len(), 2);
        std::fs::write(d.join(MANIFEST_NAME), "{}").unwrap();
        assert!(append_run(&d, json!({})).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
