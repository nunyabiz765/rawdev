//! Getting frames off a card, and setting rejects aside. Neither ever deletes anything.
//!
//! **Ingest** (`rawdev ingest SRC... --to DIR`): finds every RAW and camera JPEG under the sources
//! (recursively, e.g. a card's `DCIM/`), and copies them into one shoot folder
//! `DIR/<date>[-<name>]/raw/` and `.../jpg/`, the layout `rawdev night|day --shoot` reads. Each file is:
//!
//! 1. copied to `<dest>.part` while hashing the source (SHA-256), then flushed to disk (`fsync`);
//! 2. read back from disk and hashed again; the two hashes must match;
//! 3. renamed into place, with the source's modification time, and made read-only.
//!
//! A destination that already exists with the same hash is skipped ("already"); one with a different hash
//! is a conflict and is left alone (that file fails, the rest go on). `ingest-manifest.json` in the shoot
//! folder lists every file with its hash. `--unmount` unmounts the card afterwards, but only if every
//! file verified (it never touches a card with a failed copy). The card itself is only read.
//!
//! **Cull** (`rawdev cull DIR --reject LIST`): moves the named frames (the RAW and its camera JPEG, by
//! stem or number) into `DIR/_rejects/`, keeping their relative paths, and appends each move to
//! `DIR/_rejects/cull-log.jsonl`. `rawdev cull DIR --undo` moves everything in that log back.

use crate::Result;
use crate::manifest::{self, Events, sha256_file};
use crate::recipe;
use crate::safety::{self, Refusal};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct IngestOptions {
    pub sources: Vec<PathBuf>,
    pub to: PathBuf,
    pub name: Option<String>,
    /// YYYY-MM-DD; default: the earliest capture date among the files
    pub date: Option<String>,
    pub unmount: bool,
    pub dry_run: bool,
    pub protected: Vec<PathBuf>,
}

#[derive(Default)]
pub struct Summary {
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
}

fn is_jpeg(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string());
        // skip hidden files/folders (macOS ._ files, .Trashes) and anything that isn't a plain file/dir
        if name.as_deref().is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        let ft = std::fs::symlink_metadata(&p)?.file_type();
        if ft.is_dir() {
            walk(&p, out)?;
        } else if ft.is_file() && (safety::is_raw(&p) || is_jpeg(&p)) {
            out.push(p);
        }
    }
    Ok(())
}

/// Capture dates (YYYY-MM-DD) of the files, one exiftool run.
fn capture_dates(files: &[PathBuf]) -> Result<Vec<Option<String>>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new(crate::tools::exiftool())
        .args([
            "-json",
            "-d",
            "%Y-%m-%d",
            "-DateTimeOriginal",
            "-CreateDate",
        ])
        .args(files)
        .output()?;
    let rows: Vec<Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("exiftool -json output not understood: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| {
            ["DateTimeOriginal", "CreateDate"]
                .iter()
                .filter_map(|k| r.get(*k)?.as_str())
                .find(|s| s.len() == 10 && s.as_bytes()[4] == b'-')
                .map(str::to_string)
        })
        .collect())
}

fn valid_date(d: &str) -> bool {
    d.len() == 10
        && d.char_indices().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 80
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !n.starts_with('.')
}

/// Free bytes on the filesystem holding `dir` (via `df`, to stay free of libc bindings).
fn free_bytes(dir: &Path) -> Option<u64> {
    let out = Command::new("df")
        .args(["-P", "-k"])
        .arg(dir)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().nth(1)?;
    line.split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()
        .map(|k| k * 1024)
}

/// Copy `src` to `dest` with the hash/fsync/read-back verify. Returns the SHA-256.
fn verified_copy(src: &Path, dest: &Path) -> Result<String> {
    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let mut r = std::fs::File::open(src)?;
    let mut w = std::fs::File::create(&part)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        w.write_all(&buf[..n])?;
    }
    w.sync_all()?;
    drop(w);
    let want: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let got = sha256_file(&part)?;
    if got != want {
        std::fs::remove_file(&part).ok(); // our own partial copy, never a source
        return Err(format!("verify failed for {} (copy hash differs)", src.display()).into());
    }
    let f = std::fs::File::options().write(true).open(&part)?;
    if let Ok(m) = std::fs::metadata(src).and_then(|m| m.modified()) {
        f.set_modified(m).ok();
    }
    drop(f);
    std::fs::rename(&part, dest)?;
    let mut perm = std::fs::metadata(dest)?.permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(dest, perm)?;
    Ok(want)
}

/// The mount point and device holding `p` (longest matching entry in /proc/self/mounts).
fn mount_of(p: &Path) -> Option<(PathBuf, String)> {
    let real = p.canonicalize().ok()?;
    let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mut best: Option<(PathBuf, String)> = None;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (dev, mp) = (f.next()?, f.next()?);
        // /proc/mounts escapes spaces as \040
        let mp = PathBuf::from(mp.replace("\\040", " "));
        if real.starts_with(&mp)
            && best
                .as_ref()
                .is_none_or(|b| mp.as_os_str().len() > b.0.as_os_str().len())
        {
            best = Some((mp, dev.to_string()));
        }
    }
    best
}

/// Unmount the card: `udisksctl unmount` (works without root for removable media), else `umount`.
fn unmount(mp: &Path, dev: &str) -> Result<String> {
    if mp == Path::new("/") || mp == Path::new("/home") {
        return Err(format!("refusing to unmount {}", mp.display()).into());
    }
    let ok = Command::new("udisksctl")
        .args(["unmount", "-b", dev])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        return Ok(format!("udisksctl unmount -b {dev}"));
    }
    let ok = Command::new("umount")
        .arg(mp)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(format!("umount {}", mp.display()))
    } else {
        Err(format!("could not unmount {} ({dev})", mp.display()).into())
    }
}

pub fn ingest(o: &IngestOptions, ev: &Events) -> Result<Summary> {
    let say = |m: &str| {
        if ev.json {
            eprintln!("{m}")
        } else {
            println!("{m}")
        }
    };
    let mut files = Vec::new();
    for s in &o.sources {
        if !s.is_dir() {
            return Err(format!("source {} is not a folder", s.display()).into());
        }
        walk(s, &mut files)?;
    }
    if files.is_empty() {
        return Err(Box::new(Refusal(
            "no RAW or JPEG files found under the source folder(s)".into(),
        )));
    }
    if let Some(n) = &o.name
        && !valid_name(n)
    {
        return Err(Box::new(Refusal(format!(
            "--name {n:?}: use letters, digits, '-', '_' and '.'"
        ))));
    }
    let date = match &o.date {
        Some(d) if valid_date(d) => d.clone(),
        Some(d) => return Err(Box::new(Refusal(format!("--date {d}: use YYYY-MM-DD")))),
        None => capture_dates(&files)?
            .into_iter()
            .flatten()
            .min()
            .ok_or("no capture date in the files; pass --date YYYY-MM-DD")?,
    };
    let folder = match &o.name {
        Some(n) => format!("{date}-{n}"),
        None => date.clone(),
    };
    let shoot = o.to.join(&folder);
    safety::check_protected(&shoot, &o.protected)?;
    // never copy a source into itself
    let shoot_real = safety::resolve(&shoot)?;
    for s in &o.sources {
        let sr = s.canonicalize()?;
        if shoot_real.starts_with(&sr) || sr.starts_with(&shoot_real) {
            return Err(Box::new(Refusal(format!(
                "the destination {} and the source {} overlap",
                shoot_real.display(),
                sr.display()
            ))));
        }
    }

    // destinations; two source files with the same name would collide
    let mut plan: Vec<(PathBuf, PathBuf, u64)> = Vec::new();
    let mut names = std::collections::HashMap::new();
    for f in &files {
        let sub = if safety::is_raw(f) { "raw" } else { "jpg" };
        let name = f.file_name().unwrap().to_owned();
        let dest = shoot.join(sub).join(&name);
        if let Some(prev) = names.insert(dest.clone(), f.clone()) {
            return Err(Box::new(Refusal(format!(
                "two source files would land on {}: {} and {}",
                dest.display(),
                prev.display(),
                f.display()
            ))));
        }
        plan.push((f.clone(), dest, std::fs::metadata(f)?.len()));
    }
    let total: u64 = plan.iter().map(|p| p.2).sum();
    let free_at = shoot.ancestors().find(|p| p.exists());
    if let Some(dir) = free_at
        && let Some(free) = free_bytes(dir)
        && free < total + (512 << 20)
    {
        return Err(Box::new(Refusal(format!(
            "not enough space: {} MB to copy, {} MB free at {} (keeps 512 MB spare)",
            total >> 20,
            free >> 20,
            dir.display()
        ))));
    }
    let mount = if o.unmount {
        let m = mount_of(&o.sources[0]).ok_or("can't find the card's mount point for --unmount")?;
        // only removable-media mount points: never a system or home filesystem
        if !["/run/media/", "/media/", "/mnt/"]
            .iter()
            .any(|p| m.0.starts_with(p) && m.0 != Path::new(p.trim_end_matches('/')))
        {
            return Err(Box::new(Refusal(format!(
                "--unmount: {} doesn't look like a card mount (expected under /run/media, /media \
                 or /mnt); unmount it yourself",
                m.0.display()
            ))));
        }
        Some(m)
    } else {
        None
    };

    ev.emit(
        "ingest_start",
        json!({ "dry_run": o.dry_run, "sources": o.sources, "shoot": shoot, "files": plan.len(),
                "bytes": total, "unmount": mount.as_ref().map(|m| json!({"mount": m.0, "device": m.1})) }),
    );
    say(&format!(
        "rawdev ingest{}: {} files ({} MB) -> {}",
        if o.dry_run { " --dry-run" } else { "" },
        plan.len(),
        total >> 20,
        shoot.display()
    ));
    let mut sum = Summary::default();
    if o.dry_run {
        for (src, dest, bytes) in &plan {
            let status = if dest.exists() { "exists" } else { "copy" };
            ev.emit(
                "file_planned",
                json!({ "src": src, "dest": dest, "bytes": bytes, "status": status }),
            );
            say(&format!(
                "  {status:<6} {} -> {}",
                src.display(),
                dest.display()
            ));
        }
        ev.emit(
            "ingest_end",
            json!({ "dry_run": true, "files": plan.len() }),
        );
        return Ok(sum);
    }
    std::fs::create_dir_all(shoot.join("raw"))?;
    if plan.iter().any(|p| is_jpeg(&p.0)) {
        std::fs::create_dir_all(shoot.join("jpg"))?;
    }
    let started = manifest::utc_now();
    let mut records = Vec::new();
    for (src, dest, bytes) in &plan {
        let res: Result<(String, &str)> = if dest.exists() {
            match (sha256_file(src), sha256_file(dest)) {
                (Ok(a), Ok(b)) if a == b => Ok((a, "already")),
                (Ok(_), Ok(_)) => Err(format!(
                    "{} already exists with different content; left alone",
                    dest.display()
                )
                .into()),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        } else {
            verified_copy(src, dest).map(|h| (h, "copied"))
        };
        let rec = match res {
            Ok((sha, status)) => {
                if status == "already" {
                    sum.skipped += 1;
                } else {
                    sum.done += 1;
                }
                json!({ "src": src, "dest": dest, "bytes": bytes, "sha256": sha, "status": status })
            }
            Err(e) => {
                sum.failed += 1;
                say(&format!("  FAILED {}: {e}", src.display()));
                json!({ "src": src, "dest": dest, "bytes": bytes, "status": "failed",
                        "error": e.to_string() })
            }
        };
        ev.emit("file_done", rec.clone());
        records.push(rec);
    }
    let m = json!({
        "rawdev_ingest": manifest::SCHEMA,
        "rawdev_version": env!("CARGO_PKG_VERSION"),
        "started": started,
        "finished": manifest::utc_now(),
        "sources": o.sources,
        "files": records,
    });
    // one manifest per ingest run (a second card into the same shoot gets its own)
    let mut mpath = shoot.join("ingest-manifest.json");
    let mut n = 2;
    while mpath.exists() {
        mpath = shoot.join(format!("ingest-manifest-{n}.json"));
        n += 1;
    }
    std::fs::write(&mpath, serde_json::to_string_pretty(&m)? + "\n")?;
    say(&format!(
        "copied {}, already there {}, failed {}; manifest {}",
        sum.done,
        sum.skipped,
        sum.failed,
        mpath.display()
    ));
    let mut unmounted = Value::Null;
    if let Some((mp, dev)) = &mount {
        if sum.failed > 0 {
            say("not unmounting: some files failed");
        } else {
            match unmount(mp, dev) {
                Ok(how) => {
                    say(&format!("unmounted {} ({how})", mp.display()));
                    unmounted = json!(mp);
                }
                Err(e) => {
                    say(&format!("unmount failed: {e}"));
                    sum.failed += 1;
                }
            }
        }
    }
    ev.emit(
        "ingest_end",
        json!({ "dry_run": false, "copied": sum.done, "already": sum.skipped, "failed": sum.failed,
                "manifest": mpath, "shoot": shoot, "unmounted": unmounted }),
    );
    Ok(sum)
}

const REJECTS: &str = "_rejects";
const CULL_LOG: &str = "cull-log.jsonl";

/// Files in a shoot folder (or a plain folder of RAWs) that belong to a frame: its RAW(s) and camera
/// JPEG(s), at the top level and in `raw/` and `jpg/`.
fn frame_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for sub in ["", "raw", "jpg"] {
        let d = if sub.is_empty() {
            dir.to_path_buf()
        } else {
            dir.join(sub)
        };
        if !d.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_file() && (safety::is_raw(&p) || is_jpeg(&p)) {
                v.push(p);
            }
        }
    }
    v.sort();
    Ok(v)
}

fn say_to(ev: &Events, m: &str) {
    if ev.json {
        eprintln!("{m}")
    } else {
        println!("{m}")
    }
}

/// Move the named frames into `<dir>/_rejects/`. Never deletes; refuses to replace anything.
pub fn cull(
    dir: &Path,
    rejects: &[String],
    dry_run: bool,
    ev: &Events,
    protected: &[PathBuf],
) -> Result<Summary> {
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()).into());
    }
    safety::check_protected(dir, protected)?;
    let files = frame_files(dir)?;
    let stems: Vec<String> = files
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    let mut unique: Vec<String> = stems.clone();
    unique.sort();
    unique.dedup();
    let chosen = recipe::resolve("--reject", rejects, &unique)?;
    let rej = dir.join(REJECTS);
    let mut moves = Vec::new();
    for (f, s) in files.iter().zip(&stems) {
        if chosen.contains(s) {
            let rel = f.strip_prefix(dir).unwrap().to_path_buf();
            let dest = rej.join(&rel);
            if dest.exists() {
                return Err(Box::new(Refusal(format!(
                    "{} already exists; not replacing it",
                    dest.display()
                ))));
            }
            moves.push((f.clone(), dest));
        }
    }
    ev.emit(
        "cull_start",
        json!({ "dry_run": dry_run, "dir": dir, "frames": chosen.len(), "files": moves.len() }),
    );
    let mut sum = Summary::default();
    let mut log = if dry_run {
        None
    } else {
        std::fs::create_dir_all(&rej)?;
        Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(rej.join(CULL_LOG))?,
        )
    };
    for (from, to) in &moves {
        let rec = json!({ "from": from, "to": to, "at": manifest::utc_now() });
        if let Some(l) = &mut log {
            std::fs::create_dir_all(to.parent().unwrap())?;
            // a rename within one folder tree: same filesystem, atomic, nothing copied or deleted
            match std::fs::rename(from, to) {
                Ok(()) => {
                    writeln!(l, "{rec}")?;
                    l.sync_all()?;
                    sum.done += 1;
                }
                Err(e) => {
                    sum.failed += 1;
                    say_to(ev, &format!("  FAILED {}: {e}", from.display()));
                    continue;
                }
            }
        }
        ev.emit(if dry_run { "move_planned" } else { "moved" }, rec);
        say_to(
            ev,
            &format!(
                "  {} {} -> {}",
                if dry_run { "would move" } else { "moved" },
                from.display(),
                to.display()
            ),
        );
    }
    ev.emit(
        "cull_end",
        json!({ "dry_run": dry_run, "moved": sum.done, "failed": sum.failed,
                "log": (!dry_run).then(|| rej.join(CULL_LOG)) }),
    );
    Ok(sum)
}

/// Move everything recorded in `<dir>/_rejects/cull-log.jsonl` back where it came from.
pub fn cull_undo(dir: &Path, dry_run: bool, ev: &Events, protected: &[PathBuf]) -> Result<Summary> {
    safety::check_protected(dir, protected)?;
    let logp = dir.join(REJECTS).join(CULL_LOG);
    let text = std::fs::read_to_string(&logp)
        .map_err(|e| format!("no cull log at {}: {e}", logp.display()))?;
    let mut sum = Summary::default();
    let rej_real = dir.join(REJECTS).canonicalize()?;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line)?;
        let (Some(from), Some(to)) = (v["from"].as_str(), v["to"].as_str()) else {
            continue;
        };
        let (orig, now) = (PathBuf::from(from), PathBuf::from(to));
        if !now.exists() {
            continue; // already moved back
        }
        // only ever move files out of this folder's _rejects/, back into this folder
        if !now.canonicalize()?.starts_with(&rej_real) {
            return Err(Box::new(Refusal(format!(
                "cull log entry {} is outside {}",
                now.display(),
                rej_real.display()
            ))));
        }
        if orig.exists() {
            sum.failed += 1;
            say_to(
                ev,
                &format!(
                    "  skipped {}: something is back there already",
                    orig.display()
                ),
            );
            continue;
        }
        if !dry_run {
            std::fs::rename(&now, &orig)?;
        }
        sum.done += 1;
        ev.emit(
            if dry_run { "move_planned" } else { "moved" },
            json!({ "from": now, "to": orig }),
        );
        say_to(
            ev,
            &format!(
                "  {} {} -> {}",
                if dry_run { "would restore" } else { "restored" },
                now.display(),
                orig.display()
            ),
        );
    }
    if !dry_run && sum.failed == 0 {
        // keep the log as a record, renamed so a second --undo is a no-op
        let done = logp.with_file_name(format!("cull-log-undone-{}.jsonl", manifest::utc_now()));
        std::fs::rename(&logp, done)?;
    }
    ev.emit(
        "cull_end",
        json!({ "dry_run": dry_run, "undo": true, "moved": sum.done, "failed": sum.failed }),
    );
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_dates() {
        assert!(valid_date("2026-10-18"));
        assert!(!valid_date("2026-1-18"));
        assert!(valid_name("park-walk"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(".hidden"));
    }

    #[test]
    fn verified_copy_and_cull_round_trip() {
        let d = std::env::temp_dir().join(format!("rawdev-ingest-{}", std::process::id()));
        std::fs::create_dir_all(d.join("card/DCIM/100CANON")).unwrap();
        std::fs::create_dir_all(d.join("shoot/raw")).unwrap();
        std::fs::write(d.join("card/DCIM/100CANON/IMG_1.CR3"), vec![7u8; 100_000]).unwrap();
        let h = verified_copy(
            &d.join("card/DCIM/100CANON/IMG_1.CR3"),
            &d.join("shoot/raw/IMG_1.CR3"),
        )
        .unwrap();
        assert_eq!(
            h,
            sha256_file(&d.join("card/DCIM/100CANON/IMG_1.CR3")).unwrap()
        );
        assert!(
            std::fs::metadata(d.join("shoot/raw/IMG_1.CR3"))
                .unwrap()
                .permissions()
                .readonly()
        );
        let ev = Events { json: false };
        let s = cull(&d.join("shoot"), &["1".into()], false, &ev, &[]).unwrap();
        assert_eq!(s.done, 1);
        assert!(d.join("shoot/_rejects/raw/IMG_1.CR3").exists());
        assert!(!d.join("shoot/raw/IMG_1.CR3").exists());
        let s = cull_undo(&d.join("shoot"), false, &ev, &[]).unwrap();
        assert_eq!(s.done, 1);
        assert!(d.join("shoot/raw/IMG_1.CR3").exists());
        // read-only files: make writable again so the test can clean up
        let p = d.join("shoot/raw/IMG_1.CR3");
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }
}
