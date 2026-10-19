//! The two external programs rawdev runs, and the optional config file.
//!
//! - `dcraw_emu` (from LibRaw) decodes the RAW.
//! - `exiftool` reads ISO / embedded previews and copies EXIF + GPS into the outputs.
//!
//! Both are found once at start-up and remembered. Lookup order, first hit wins:
//!
//! 1. the command-line flag (`--exiftool PATH`, `--dcraw-emu PATH`);
//! 2. the environment (`RAWDEV_EXIFTOOL`, `RAWDEV_DCRAW_EMU`);
//! 3. the config file (`exiftool = "..."`, `dcraw_emu = "..."`);
//! 4. `PATH`, then a few well-known locations (Arch keeps exiftool in `/usr/bin/vendor_perl`).
//!
//! The config file is `$RAWDEV_CONFIG`, else `$XDG_CONFIG_HOME/rawdev/config.toml`, else
//! `~/.config/rawdev/config.toml`. It is optional; a missing file is the same as an empty one.
//!
//! ```toml
//! exiftool = "/opt/exiftool/exiftool"
//! dcraw_emu = "/usr/local/bin/dcraw_emu"
//! protected = ["~/Photos/archive", "/mnt/nas/originals"]   # rawdev never writes inside these
//! ```

use crate::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub exiftool: Option<PathBuf>,
    pub dcraw_emu: Option<PathBuf>,
    /// folders rawdev must never write inside (outputs, manifests, ingest copies)
    #[serde(default)]
    pub protected: Vec<PathBuf>,
}

/// Where the config file would be (whether or not it exists).
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RAWDEV_CONFIG") {
        return Some(PathBuf::from(p));
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        return Some(PathBuf::from(x).join("rawdev/config.toml"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/rawdev/config.toml"))
}

/// `~/x` -> `$HOME/x` (config paths only; the shell already does this for arguments).
pub fn expand_home(p: &Path) -> PathBuf {
    if let (Ok(rest), Some(home)) = (p.strip_prefix("~"), std::env::var_os("HOME")) {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

/// Read the config file. A missing file is an empty config; a broken one is an error.
pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)?;
    let mut c: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    c.protected = c.protected.iter().map(|p| expand_home(p)).collect();
    c.exiftool = c.exiftool.map(|p| expand_home(&p));
    c.dcraw_emu = c.dcraw_emu.map(|p| expand_home(&p));
    Ok(c)
}

/// One resolved external program.
#[derive(Clone, Debug)]
pub struct Tool {
    pub path: PathBuf,
    /// first line of its version output, e.g. "13.55" or "LibRaw 0.22.2"
    pub version: String,
}

static EXIFTOOL: OnceLock<PathBuf> = OnceLock::new();
static DCRAW_EMU: OnceLock<PathBuf> = OnceLock::new();

/// The exiftool to run (falls back to `exiftool` on PATH if `init` was never called, e.g. in tests).
pub fn exiftool() -> &'static Path {
    EXIFTOOL.get_or_init(|| find("exiftool", EXIFTOOL_FALLBACKS).unwrap_or("exiftool".into()))
}

/// The dcraw_emu to run.
pub fn dcraw_emu() -> &'static Path {
    DCRAW_EMU.get_or_init(|| find("dcraw_emu", DCRAW_FALLBACKS).unwrap_or("dcraw_emu".into()))
}

const EXIFTOOL_FALLBACKS: &[&str] = &[
    "/usr/bin/vendor_perl/exiftool",
    "/usr/bin/exiftool",
    "/usr/local/bin/exiftool",
    "/opt/homebrew/bin/exiftool",
];
const DCRAW_FALLBACKS: &[&str] = &[
    "/usr/bin/dcraw_emu",
    "/usr/local/bin/dcraw_emu",
    "/usr/lib/libraw/dcraw_emu",
    "/opt/homebrew/bin/dcraw_emu",
];

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `name` on PATH, else the first existing fallback.
fn find(name: &str, fallbacks: &[&str]) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(name);
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    fallbacks
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable(p))
}

/// A missing external program (exit code 3).
#[derive(Debug)]
pub struct MissingTool(pub String);

impl std::fmt::Display for MissingTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for MissingTool {}

fn pick(
    flag: Option<&Path>,
    env: &str,
    config: Option<&Path>,
    name: &str,
    fallbacks: &[&str],
    hint: &str,
) -> Result<PathBuf> {
    let chosen = flag
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(env).map(PathBuf::from))
        .or_else(|| config.map(Path::to_path_buf));
    // a bare name ("exiftool") means "look it up on PATH"
    let chosen = chosen.map(|p| {
        if p.components().count() == 1 {
            find(&p.to_string_lossy(), &[]).unwrap_or(p)
        } else {
            p
        }
    });
    match chosen {
        Some(p) if is_executable(&p) => Ok(p),
        Some(p) => Err(Box::new(MissingTool(format!(
            "{name}: {} is not an executable file",
            p.display()
        )))),
        None => find(name, fallbacks)
            .ok_or_else(|| Box::new(MissingTool(format!("{name} not found on PATH. {hint}"))) as _),
    }
}

/// Find both programs, check they run, remember them. Errors are [`MissingTool`] (exit code 3).
pub fn init(
    exiftool_flag: Option<&Path>,
    dcraw_flag: Option<&Path>,
    config: &Config,
) -> Result<(Tool, Tool)> {
    let ex = pick(
        exiftool_flag,
        "RAWDEV_EXIFTOOL",
        config.exiftool.as_deref(),
        "exiftool",
        EXIFTOOL_FALLBACKS,
        "Install it (Arch: perl-image-exiftool, Debian/Ubuntu: libimage-exiftool-perl, \
         Fedora: perl-Image-ExifTool) or set RAWDEV_EXIFTOOL",
    )?;
    let dc = pick(
        dcraw_flag,
        "RAWDEV_DCRAW_EMU",
        config.dcraw_emu.as_deref(),
        "dcraw_emu",
        DCRAW_FALLBACKS,
        "Install LibRaw's tools (Arch: libraw, Debian/Ubuntu: libraw-bin, Fedora: LibRaw-samples) \
         or set RAWDEV_DCRAW_EMU",
    )?;
    let ex_ver = Command::new(&ex)
        .arg("-ver")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .ok_or_else(|| MissingTool(format!("exiftool at {} does not run", ex.display())))?;
    // dcraw_emu has no version flag; check it runs (no arguments prints the usage), then ask
    // pkg-config for the LibRaw release, else name the shared library it links
    Command::new(&dc)
        .output()
        .map_err(|e| MissingTool(format!("dcraw_emu at {} does not run: {e}", dc.display())))?;
    let dc_ver = libraw_version(&dc);
    EXIFTOOL.set(ex.clone()).ok();
    DCRAW_EMU.set(dc.clone()).ok();
    Ok((
        Tool {
            path: ex,
            version: ex_ver,
        },
        Tool {
            path: dc,
            version: dc_ver,
        },
    ))
}

/// "LibRaw 0.22.2" (pkg-config), else "libraw.so.25" (from ldd), else "unknown".
fn libraw_version(dcraw: &Path) -> String {
    let run = |c: &mut Command| {
        c.output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(v) =
        run(Command::new("pkg-config").args(["--modversion", "libraw"])).filter(|v| !v.is_empty())
    {
        return format!("LibRaw {v}");
    }
    run(Command::new("ldd").arg(dcraw))
        .and_then(|t| {
            t.split_whitespace()
                .find(|w| w.starts_with("libraw.so") || w.starts_with("libraw_r.so"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}
