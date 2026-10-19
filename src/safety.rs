//! The write guards. Everything here runs *before* rawdev creates a folder or writes a byte.
//!
//! rawdev refuses to write:
//!
//! - into the folder that holds the input RAWs (outputs never sit next to originals);
//! - anywhere inside a shoot's `dev/` folder: `<shoot>/dev` for `--shoot`, and for any input that lives
//!   in a `<shoot>/raw/` folder (that folder belongs to whatever develop pipeline you already run);
//! - anywhere inside a folder listed under `protected` in the config file.
//!
//! Paths are compared after resolving symlinks, so a symlink can't be used to sneak past a guard.

use crate::Result;
use std::path::{Path, PathBuf};

/// A safety refusal (exit code 2).
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refusal {}

fn refuse<T>(msg: String) -> Result<T> {
    Err(Box::new(Refusal(msg)))
}

/// The RAW extensions rawdev picks up from a folder (any LibRaw format works when named directly
/// with one of these extensions).
pub const RAW_EXT: &[&str] = &[
    "3fr", "ari", "arw", "bay", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq", "k25",
    "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "raf", "raw", "rw2", "rwl",
    "sr2", "srf", "srw", "x3f",
];

pub fn is_raw(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RAW_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// The RAW files directly inside `dir`, sorted by name.
pub fn raws_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_raw(p))
        .collect();
    v.sort();
    Ok(v)
}

/// The real (symlink-resolved) form of a path that may not exist yet: resolve the deepest part that does
/// exist, then add the rest back on. Paths with `..` in the missing part are rejected to keep this honest.
pub fn resolve(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("bad path {}", path.display()))?
            .to_owned();
        rest.push(name);
        if !existing.pop() || existing.as_os_str().is_empty() {
            existing = PathBuf::from(".");
        }
    }
    let mut real = existing.canonicalize()?;
    for name in rest.iter().rev() {
        if name == ".." {
            return refuse(format!(
                "please give the path without '..': {}",
                path.display()
            ));
        }
        real.push(name);
    }
    Ok(real)
}

/// Refuse if `target` is inside one of the configured protected folders.
pub fn check_protected(target: &Path, protected: &[PathBuf]) -> Result<()> {
    let t = resolve(target)?;
    for p in protected {
        let pr = resolve(p)?;
        if t.starts_with(&pr) {
            return refuse(format!(
                "refusing to write into {}: it is inside the protected folder {} (config file)",
                t.display(),
                pr.display()
            ));
        }
    }
    Ok(())
}

/// All the output-folder guards for a develop run.
pub fn check_out(
    out: &Path,
    inputs: &[PathBuf],
    shoot: Option<&Path>,
    protected: &[PathBuf],
) -> Result<()> {
    let out_real = resolve(out)?;
    check_protected(out, protected)?;
    let mut dev_dirs: Vec<PathBuf> = shoot.map(|s| s.join("dev")).into_iter().collect();
    for i in inputs {
        let Some(dir) = i.parent() else { continue };
        let dir_real = resolve(if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        })?;
        if out_real == dir_real {
            return refuse(format!(
                "refusing to write into {}: that is where the RAWs are. Pick another --out \
                 (a subfolder is fine)",
                dir_real.display()
            ));
        }
        if dir_real.file_name().is_some_and(|n| n == "raw")
            && let Some(s) = dir_real.parent()
        {
            dev_dirs.push(s.join("dev"));
        }
    }
    for d in dev_dirs {
        let d_real = resolve(&d)?;
        if out_real.starts_with(&d_real) {
            return refuse(format!(
                "refusing to write into {}: a shoot's dev/ folder belongs to your existing develop \
                 pipeline, not to rawdev",
                d_real.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rawdev-safety-{name}-{}", std::process::id()));
        std::fs::create_dir_all(d.join("shoot/raw")).unwrap();
        std::fs::create_dir_all(d.join("shoot/dev")).unwrap();
        std::fs::write(d.join("shoot/raw/A.CR3"), b"x").unwrap();
        d
    }

    #[test]
    fn guards() {
        let d = tmp("guards");
        let raw = vec![d.join("shoot/raw/A.CR3")];
        // next to the RAWs
        assert!(check_out(&d.join("shoot/raw"), &raw, None, &[]).is_err());
        // a shoot's dev/, found from the input path alone, even through a symlink
        assert!(check_out(&d.join("shoot/dev/x"), &raw, None, &[]).is_err());
        std::os::unix::fs::symlink(d.join("shoot/dev"), d.join("sneaky")).unwrap();
        assert!(check_out(&d.join("sneaky/out"), &raw, None, &[]).is_err());
        // protected
        assert!(check_out(&d.join("shoot/out"), &raw, None, &[d.join("shoot")]).is_err());
        // fine
        assert!(check_out(&d.join("shoot/out"), &raw, None, &[]).is_ok());
        assert!(check_out(&d.join("shoot/raw/developed"), &raw, None, &[]).is_ok());
        // '..' in the part that doesn't exist yet
        assert!(resolve(&d.join("nope/../x")).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
