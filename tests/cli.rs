//! Command-line contract tests: exit codes, JSON events, refusals, and "a dry run writes nothing".
//! None of these needs a real RAW file (the guards run before any decoding); the dry-run test needs
//! exiftool installed.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rawdev() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rawdev"))
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rawdev-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("shoot/raw")).unwrap();
    std::fs::create_dir_all(d.join("shoot/dev")).unwrap();
    std::fs::write(d.join("shoot/raw/IMG_1.CR3"), b"not really a raw").unwrap();
    d
}

fn events(o: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect()
}

fn last_error(o: &Output) -> Value {
    let ev = events(o);
    let e = ev.last().expect("an event").clone();
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(e["v"], 1);
    e
}

fn tree(d: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(d).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            v.extend(tree(&p));
        }
        v.push(p);
    }
    v.sort();
    v
}

#[test]
fn refuses_to_write_next_to_the_raws() {
    let d = tmp("next");
    let o = rawdev()
        .args(["develop", "--json", "--out"])
        .arg(d.join("shoot/raw"))
        .arg(d.join("shoot/raw/IMG_1.CR3"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(last_error(&o)["kind"], "refused");
}

#[test]
fn refuses_a_shoots_dev_folder_and_writes_nothing() {
    let d = tmp("dev");
    let before = tree(&d);
    for args in [
        vec!["develop".to_string()],
        vec![
            "day".to_string(),
            "--shoot".to_string(),
            d.join("shoot").display().to_string(),
        ],
    ] {
        let mut c = rawdev();
        c.args(&args)
            .arg("--json")
            .arg("--out")
            .arg(d.join("shoot/dev/rawdev"));
        if args[0] == "develop" {
            c.arg(d.join("shoot/raw/IMG_1.CR3"));
        } else {
            c.arg("1");
        }
        let o = c.output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert_eq!(last_error(&o)["kind"], "refused");
    }
    assert_eq!(tree(&d), before, "nothing may be created on a refusal");
}

#[test]
fn refuses_protected_folders_from_the_config() {
    let d = tmp("protected");
    let cfg = d.join("config.toml");
    std::fs::write(&cfg, format!("protected = [{:?}]\n", d.join("archive"))).unwrap();
    let o = rawdev()
        .env("RAWDEV_CONFIG", &cfg)
        .args(["develop", "--json", "--out"])
        .arg(d.join("archive/out"))
        .arg(d.join("shoot/raw/IMG_1.CR3"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(!d.join("archive").exists());
}

#[test]
fn rotate_and_crop_are_refused() {
    for flag in [
        "--rotate",
        "--crop",
        "--level",
        "--straighten",
        "--auto-rotate",
    ] {
        let o = rawdev().args(["develop", "--json", flag]).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{flag}");
        assert_eq!(last_error(&o)["kind"], "refused");
    }
}

#[test]
fn missing_tool_is_exit_3() {
    let d = tmp("missing");
    let o = rawdev()
        .env("RAWDEV_EXIFTOOL", "/nonexistent/exiftool")
        .args(["develop", "--json", "--dry-run", "--out"])
        .arg(d.join("out"))
        .arg(d.join("shoot/raw/IMG_1.CR3"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(last_error(&o)["kind"], "missing_tool");
}

#[test]
fn bad_arguments_are_exit_2() {
    let d = tmp("args");
    let raw = d.join("shoot/raw/IMG_1.CR3");
    let cases: Vec<Vec<String>> = vec![
        vec!["develop".into(), "--frobnicate".into()],
        vec!["develop".into(), raw.display().to_string()], // no --out
        vec![
            "develop".into(),
            "--out".into(),
            d.join("o").display().to_string(),
            "--match".into(),
            "999".into(),
            raw.display().to_string(),
        ], // selector names no frame
        vec![
            "develop".into(),
            "--look".into(),
            "rich-night".into(),
            "--match".into(),
            "1".into(),
            "--out".into(),
            d.join("o").display().to_string(),
            raw.display().to_string(),
        ], // day flag on a night look
        vec!["night".into(), "--look".into(), "rich-day".into()],
        vec!["ingest".into(), "--out".into(), "x".into()],
        vec!["develop".into(), "--threads".into(), "0".into()],
        vec!["develop".into(), "--threads".into(), "many".into()],
        vec!["ingest".into(), "--threads".into(), "2".into()], // develop-only option
    ];
    for c in cases {
        let o = rawdev().args(&c).arg("--json").output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{c:?}");
        assert_eq!(last_error(&o)["kind"], "usage", "{c:?}");
    }
}

#[test]
fn existing_outputs_need_overwrite() {
    let d = tmp("overwrite");
    std::fs::create_dir_all(d.join("out")).unwrap();
    std::fs::write(d.join("out/IMG_1.jpg"), b"keep me").unwrap();
    let o = rawdev()
        .args(["develop", "--json", "--dry-run", "--out"])
        .arg(d.join("out"))
        .arg(d.join("shoot/raw/IMG_1.CR3"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(std::fs::read(d.join("out/IMG_1.jpg")).unwrap(), b"keep me");
}

#[test]
fn dry_run_plans_and_writes_nothing() {
    if Command::new("exiftool").arg("-ver").output().is_err()
        && !Path::new("/usr/bin/vendor_perl/exiftool").exists()
    {
        eprintln!("skipped: exiftool not installed");
        return;
    }
    let d = tmp("dry");
    let before = tree(&d);
    let o = rawdev()
        .args(["develop", "--json", "--dry-run", "--out"])
        .arg(d.join("out"))
        .arg(d.join("shoot/raw"))
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let ev = events(&o);
    let kinds: Vec<&str> = ev.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["batch_start", "frame_planned", "batch_end"]);
    assert_eq!(ev[0]["dry_run"], true);
    assert_eq!(ev[0]["look"]["name"], "neutral-day");
    assert_eq!(ev[1]["stem"], "IMG_1");
    assert_eq!(ev[1]["tone_ref"], "neutral"); // a fake file has no preview
    assert_eq!(tree(&d), before, "a dry run writes nothing");
}

#[test]
fn looks_list_as_json() {
    let o = rawdev().args(["looks", "--json"]).output().unwrap();
    assert!(o.status.success());
    let names: Vec<String> = events(&o)
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["neutral-day", "neutral-night", "rich-day", "rich-night"]
    );
}

#[test]
fn cull_dry_run_moves_nothing() {
    let d = tmp("cull");
    let before = tree(&d);
    let o = rawdev()
        .args(["cull", "--json", "--dry-run", "--reject", "1"])
        .arg(d.join("shoot"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let ev = events(&o);
    assert_eq!(ev[1]["event"], "move_planned");
    assert_eq!(tree(&d), before);
}
