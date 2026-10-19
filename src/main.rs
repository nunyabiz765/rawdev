//! The `rawdev` command line. See `rawdev help`, README.md and docs/JSON.md.

use rawdev::day::DayOptions;
use rawdev::decode::{self, Highlights};
use rawdev::develop::{self, FrameJob, FrameOptions, FrameReport, Recipe};
use rawdev::export::{self, RawMeta};
use rawdev::ingest;
use rawdev::lookfile::{self, Look, RecipeKind};
use rawdev::manifest::{self, Events, sha256_bytes, sha256_file};
use rawdev::recipe::{self, FrameFlags, RecipeFile};
use rawdev::reference::{self, RefMode, ToneRef};
use rawdev::tools::{self, MissingTool, Tool};
use rawdev::{gpu, safety};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
rawdev - develop RAW photos to JPEG, built to be driven by scripts and AI agents

usage:
  rawdev develop --out DIR [options] RAW|DIR...    develop any LibRaw-supported RAW files
  rawdev night   --shoot DIR --out DIR [options] FRAME...   shoot layout, night look (rich-night)
  rawdev day     --shoot DIR --out DIR [options] FRAME...   shoot layout, day look (rich-day)
  rawdev ingest  SRC... --to DIR [--name NAME] [--date YYYY-MM-DD] [--unmount]
                                                   verified copy of a camera card (SHA-256)
  rawdev cull    DIR --reject LIST | --undo        move rejects into DIR/_rejects (never deletes)
  rawdev looks [NAME]                              list the built-in looks, or print one as TOML
  rawdev check                                     find exiftool / dcraw_emu / GPUs, print versions
  rawdev gpus                                      list Vulkan adapters
  rawdev version

develop / night / day options:
  --out DIR            output folder: DIR/<name>.jpg (full size), DIR/share/<name>.jpg (long edge
                       from the look), DIR/progress.log, DIR/rawdev-manifest.json
  --look NAME|FILE     a built-in look or a .toml file (develop: neutral-day; night: rich-night;
                       day: rich-day). 'rawdev looks' lists them
  --recipe FILE        per-frame choices in a TOML file (see README, 'Recipe files')
  --tone-ref MODE      what the tone curve is fitted to: auto (camera JPEG next to the RAW, else the
                       RAW's embedded preview, else a neutral curve), preview, neutral. Default auto
  --overwrite          replace existing outputs (default: refuse)
  --gpu                noise reduction on the GPU (Vulkan); bit-exact with the CPU; falls back to CPU
  --gpu-adapter NAME   pick the adapter whose name contains NAME (implies --gpu)
  --dump-stages        also write every intermediate stage to DIR/stages/<name>/ (raw RGB bytes)
  per-frame lists (comma-separated names like IMG_0007 or numbers like 7):
  --match LIST         day looks: sky colour matrix fitted to the camera JPEG
  --shadow-lift LIST   day looks: gentle linear-light shadow lift
  --recover-hl LIST    day looks: decode with highlight blend to keep texture in bright areas
  --strong LIST        night looks: stronger NR for saturated coloured-LED frames
  --nofringe LIST      night looks: skip the purple-ring cleanup (real purple/magenta light)

common options:
  --json               one JSON object per line on stdout (docs/JSON.md); human text goes to stderr
  --dry-run            run every check and print the plan; write nothing
  --exiftool PATH      exiftool to use (also RAWDEV_EXIFTOOL, or 'exiftool =' in the config file)
  --dcraw-emu PATH     dcraw_emu to use (also RAWDEV_DCRAW_EMU)

safety: rawdev only ever reads RAW files; never deletes anything; refuses to overwrite without
--overwrite; refuses to write next to the RAWs, into a shoot's dev/ folder, or into any folder listed
under 'protected' in ~/.config/rawdev/config.toml; and never rotates, levels or crops a frame.

exit codes: 0 ok, 1 some frames/files failed, 2 bad arguments or a safety refusal, 3 a missing tool";

/// An error with an exit code, for the CLI.
struct Fail {
    code: i32,
    kind: &'static str,
    msg: String,
}

fn usage_err(msg: impl Into<String>) -> Fail {
    Fail {
        code: 2,
        kind: "usage",
        msg: msg.into(),
    }
}

fn refuse(msg: impl Into<String>) -> Fail {
    Fail {
        code: 2,
        kind: "refused",
        msg: msg.into(),
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for Fail {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Fail {
        if e.downcast_ref::<MissingTool>().is_some() {
            return Fail {
                code: 3,
                kind: "missing_tool",
                msg: e.to_string(),
            };
        }
        if let Some(r) = e.downcast_ref::<safety::Refusal>() {
            return refuse(r.0.clone());
        }
        Fail {
            code: 1,
            kind: "error",
            msg: e.to_string(),
        }
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Fail {
        Fail {
            code: 1,
            kind: "error",
            msg: e.to_string(),
        }
    }
}

type R<T> = std::result::Result<T, Fail>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    Develop,
    Night,
    Day,
    Ingest,
    Cull,
    Looks,
    Check,
    Gpus,
}

/// Everything the command line can say. Each command accepts a subset (checked in `parse`).
#[derive(Default)]
struct Opts {
    values: HashMap<&'static str, String>,
    flags: HashSet<&'static str>,
    positional: Vec<String>,
    argv: Vec<String>,
}

impl Opts {
    fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).map(String::as_str)
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains(k)
    }
    fn list(&self, k: &str) -> Vec<String> {
        self.get(k).map(recipe::parse_list).unwrap_or_default()
    }
    fn path(&self, k: &str) -> Option<PathBuf> {
        self.get(k).map(PathBuf::from)
    }
}

const VALUE_OPTS: &[&str] = &[
    "--out",
    "--shoot",
    "--look",
    "--recipe",
    "--tone-ref",
    "--gpu-adapter",
    "--exiftool",
    "--dcraw-emu",
    "--match",
    "--shadow-lift",
    "--recover-hl",
    "--strong",
    "--nofringe",
    "--to",
    "--name",
    "--date",
    "--reject",
];
const BOOL_OPTS: &[&str] = &[
    "--json",
    "--dry-run",
    "--overwrite",
    "--gpu",
    "--dump-stages",
    "--unmount",
    "--undo",
];

/// The develop options shared by `develop`, `night` and `day`.
const DEV_OPTS: &[&str] = &[
    "--out",
    "--look",
    "--recipe",
    "--tone-ref",
    "--gpu-adapter",
    "--exiftool",
    "--dcraw-emu",
    "--match",
    "--shadow-lift",
    "--recover-hl",
    "--strong",
    "--nofringe",
    "--json",
    "--dry-run",
    "--overwrite",
    "--gpu",
    "--dump-stages",
];

fn allowed(cmd: Cmd, k: &str) -> bool {
    match cmd {
        Cmd::Develop => DEV_OPTS.contains(&k),
        Cmd::Night | Cmd::Day => k == "--shoot" || DEV_OPTS.contains(&k),
        Cmd::Ingest => [
            "--to",
            "--name",
            "--date",
            "--exiftool",
            "--json",
            "--dry-run",
            "--unmount",
        ]
        .contains(&k),
        Cmd::Cull => ["--reject", "--undo", "--json", "--dry-run"].contains(&k),
        Cmd::Check => ["--json", "--exiftool", "--dcraw-emu"].contains(&k),
        Cmd::Looks | Cmd::Gpus => k == "--json",
    }
}

fn parse(args: Vec<String>) -> R<(Cmd, Opts)> {
    let mut it = args.iter();
    let cmd = match it.next().map(String::as_str) {
        Some("develop") => Cmd::Develop,
        Some("night") => Cmd::Night,
        Some("day") => Cmd::Day,
        Some("ingest") => Cmd::Ingest,
        Some("cull") => Cmd::Cull,
        Some("looks") => Cmd::Looks,
        Some("check") => Cmd::Check,
        Some("gpus") => Cmd::Gpus,
        Some("version") | Some("--version") | Some("-V") => {
            println!("rawdev {VERSION}");
            std::process::exit(0);
        }
        Some("help") | Some("-h") | Some("--help") | None => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        Some(s) => {
            return Err(usage_err(format!(
                "unknown command {s} (see 'rawdev help')"
            )));
        }
    };
    let mut o = Opts {
        argv: args.clone(),
        ..Default::default()
    };
    while let Some(arg) = it.next() {
        if arg == "-h" || arg == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if matches!(
            arg.as_str(),
            "--rotate" | "--level" | "--straighten" | "--crop" | "--auto-rotate"
        ) {
            return Err(refuse(format!(
                "{arg}: rawdev never rotates, levels or crops a frame. Do that by hand, after \
                 the develop, if you want it"
            )));
        }
        if !arg.starts_with('-') || arg == "-" {
            o.positional.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        if let Some(&k) = VALUE_OPTS.iter().find(|&&k| k == name) {
            if !allowed(cmd, k) {
                return Err(usage_err(format!("{k} doesn't apply to this command")));
            }
            let v = match inline {
                Some(v) => v,
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage_err(format!("{k} needs a value")))?,
            };
            o.values.insert(k, v);
        } else if let Some(&k) = BOOL_OPTS.iter().find(|&&k| k == name) {
            if !allowed(cmd, k) {
                return Err(usage_err(format!("{k} doesn't apply to this command")));
            }
            o.flags.insert(k);
        } else {
            return Err(usage_err(format!(
                "unknown option {arg} (see 'rawdev help')"
            )));
        }
    }
    Ok((cmd, o))
}

/// Human-readable progress: stdout normally, stderr when stdout carries JSON. Also appended to
/// `<out>/progress.log` when there is an out folder.
struct Human {
    json: bool,
    log_file: Option<PathBuf>,
}

impl Human {
    fn say(&self, msg: &str) {
        if self.json {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    }
    fn log(&self, msg: &str) -> R<()> {
        let line = format!("{} {msg}", clock());
        self.say(&line);
        if let Some(p) = &self.log_file {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

/// HH:MM:SS local time, via `date` (keeps us free of a time-zone crate for one log prefix).
fn clock() -> String {
    std::process::Command::new("date")
        .arg("+%T")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn tools_json(ex: &Tool, dc: &Tool) -> Value {
    json!({
        "exiftool": { "path": ex.path, "version": ex.version },
        "dcraw_emu": { "path": dc.path, "version": dc.version },
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let events = Events { json };
    let code = match run(args, &events) {
        Ok(code) => code,
        Err(f) => {
            events.emit(
                "error",
                json!({ "code": f.code, "kind": f.kind, "message": f.msg }),
            );
            eprintln!("rawdev: {}", f.msg);
            f.code
        }
    };
    std::process::exit(code);
}

fn run(args: Vec<String>, ev: &Events) -> R<i32> {
    let (cmd, o) = parse(args)?;
    match cmd {
        Cmd::Looks => looks(&o, ev),
        Cmd::Gpus => {
            let list = gpu::list();
            for a in &list {
                ev.emit(
                    "gpu",
                    json!({ "name": a.name, "kind": a.kind, "driver": a.driver }),
                );
                if !ev.json {
                    println!("{:<44} {:<10} {}", a.name, a.kind, a.driver);
                }
            }
            if list.is_empty() && !ev.json {
                println!("no Vulkan adapters (rawdev will use the CPU)");
            }
            Ok(0)
        }
        Cmd::Check => check(&o, ev),
        Cmd::Ingest => {
            let config = tools::load_config()?;
            let to = o
                .path("--to")
                .ok_or_else(|| usage_err("ingest needs --to DIR"))?;
            if o.positional.is_empty() {
                return Err(usage_err(
                    "ingest needs at least one source folder (the card)",
                ));
            }
            tools::init(o.path("--exiftool").as_deref(), None, &config)?;
            let opts = ingest::IngestOptions {
                sources: o.positional.iter().map(PathBuf::from).collect(),
                to,
                name: o.get("--name").map(str::to_string),
                date: o.get("--date").map(str::to_string),
                unmount: o.has("--unmount"),
                dry_run: o.has("--dry-run"),
                protected: config.protected.clone(),
            };
            let s = ingest::ingest(&opts, ev)?;
            Ok(if s.failed > 0 { 1 } else { 0 })
        }
        Cmd::Cull => {
            let config = tools::load_config()?;
            let dir = match o.positional.as_slice() {
                [d] => PathBuf::from(d),
                _ => return Err(usage_err("cull needs exactly one folder")),
            };
            let s = if o.has("--undo") {
                ingest::cull_undo(&dir, o.has("--dry-run"), ev, &config.protected)?
            } else {
                let rejects = o.list("--reject");
                if rejects.is_empty() {
                    return Err(usage_err("cull needs --reject LIST (or --undo)"));
                }
                ingest::cull(&dir, &rejects, o.has("--dry-run"), ev, &config.protected)?
            };
            Ok(if s.failed > 0 { 1 } else { 0 })
        }
        Cmd::Develop | Cmd::Night | Cmd::Day => develop_cmd(cmd, &o, ev),
    }
}

fn looks(o: &Opts, ev: &Events) -> R<i32> {
    match o.positional.as_slice() {
        [] => {
            for (name, text) in lookfile::BUILTIN {
                let l = Look::parse(text)?;
                ev.emit(
                    "look",
                    json!({ "name": name, "recipe": l.recipe.name(), "description": l.description,
                            "sha256": sha256_bytes(text.as_bytes()) }),
                );
                if !ev.json {
                    println!("{name:<14} {:<6} {}", l.recipe.name(), l.description);
                }
            }
            Ok(0)
        }
        [name] => {
            let text = lookfile::builtin_source(name).ok_or_else(|| {
                usage_err(format!(
                    "no built-in look called {name} (try 'rawdev looks')"
                ))
            })?;
            if ev.json {
                ev.emit("look", json!({ "name": name, "toml": text }));
            } else {
                print!("{text}");
            }
            Ok(0)
        }
        _ => Err(usage_err("looks takes at most one name")),
    }
}

fn check(o: &Opts, ev: &Events) -> R<i32> {
    let config = tools::load_config()?;
    let cfg_path = tools::config_path();
    let found = tools::init(
        o.path("--exiftool").as_deref(),
        o.path("--dcraw-emu").as_deref(),
        &config,
    );
    let gpus = gpu::list();
    let (code, tools_v) = match &found {
        Ok((ex, dc)) => (0, tools_json(ex, dc)),
        Err(e) => (3, json!({ "error": e.to_string() })),
    };
    ev.emit(
        "check",
        json!({
            "rawdev_version": VERSION,
            "ok": code == 0,
            "tools": tools_v,
            "config": cfg_path.as_ref().map(|p| json!({"path": p, "exists": p.exists()})),
            "protected": config.protected,
            "gpus": gpus.iter()
                .map(|a| json!({"name": a.name, "kind": a.kind, "driver": a.driver}))
                .collect::<Vec<_>>(),
        }),
    );
    if !ev.json {
        println!("rawdev {VERSION}");
        match &found {
            Ok((ex, dc)) => {
                println!("exiftool   {} ({})", ex.path.display(), ex.version);
                println!("dcraw_emu  {} ({})", dc.path.display(), dc.version);
            }
            Err(e) => println!("MISSING: {e}"),
        }
        if let Some(p) = &cfg_path {
            println!(
                "config     {}{}",
                p.display(),
                if p.exists() {
                    ""
                } else {
                    " (not present; optional)"
                }
            );
        }
        for p in &config.protected {
            println!("protected  {}", p.display());
        }
        if gpus.is_empty() {
            println!("gpu        none (the CPU path is used; it is the reference anyway)");
        }
        for a in &gpus {
            println!("gpu        {} [{}] {}", a.name, a.kind, a.driver);
        }
    }
    Ok(code)
}

/// One frame's plan: everything decided before any pixel work.
struct Planned {
    job: FrameJob,
    meta: RawMeta,
    flags: FrameFlags,
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The inputs: files as given, folders expanded to the RAW files directly inside them.
fn collect_inputs(cmd: Cmd, o: &Opts) -> R<Vec<PathBuf>> {
    if o.positional.is_empty() {
        return Err(usage_err("no input frames given (see 'rawdev help')"));
    }
    let mut out = Vec::new();
    if cmd == Cmd::Develop {
        for a in &o.positional {
            let p = PathBuf::from(a);
            if p.is_dir() {
                let found = safety::raws_in(&p)?;
                if found.is_empty() {
                    return Err(usage_err(format!("no RAW files in {}", p.display())));
                }
                out.extend(found);
            } else if p.is_file() {
                if !safety::is_raw(&p) {
                    return Err(usage_err(format!(
                        "{}: not a RAW file extension rawdev knows ({})",
                        p.display(),
                        safety::RAW_EXT.join(" ")
                    )));
                }
                out.push(p);
            } else {
                return Err(usage_err(format!("missing input {}", p.display())));
            }
        }
    } else {
        // shoot layout: FRAME is a number or a stem, looked up in <shoot>/raw
        let shoot = o
            .path("--shoot")
            .ok_or_else(|| usage_err("--shoot is required"))?;
        let raw_dir = shoot.join("raw");
        for a in &o.positional {
            // the common case first: <shoot>/raw/IMG_<n>.CR3
            let canon = raw_dir.join(format!("IMG_{a}.CR3"));
            if canon.is_file() {
                out.push(canon);
                continue;
            }
            let all = if raw_dir.is_dir() {
                safety::raws_in(&raw_dir)?
            } else {
                Vec::new()
            };
            let hits: Vec<PathBuf> = all
                .into_iter()
                .filter(|p| recipe::matches(a, &stem_of(p)))
                .collect();
            if hits.is_empty() {
                return Err(usage_err(format!(
                    "no RAW for frame {a} in {}",
                    raw_dir.display()
                )));
            }
            out.extend(hits);
        }
    }
    // drop exact duplicates, keep order
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.canonicalize().unwrap_or(p.clone())));
    Ok(out)
}

fn nr_json(p: &Option<rawdev::look::NrParams>) -> Value {
    match p {
        Some(p) => json!({
            "h_luma": p.h_luma, "h_color": p.h_color, "pre_median": p.pre_median,
            "chroma_median": p.chroma_median, "luma_blend": p.luma_blend,
            "blend_from_input": p.blend_from_input,
        }),
        None => Value::Null,
    }
}

fn options_json(r: &Recipe) -> Value {
    match r {
        Recipe::Night(o) => json!({ "defringe": o.defringe, "strong_nr": o.strong_nr }),
        Recipe::Day(o) => json!({
            "match_sky": o.match_sky, "shadow_lift": o.shadow_lift, "recover_hl": o.recover_hl,
        }),
    }
}

fn tone_ref_path(t: &ToneRef) -> Value {
    match t {
        ToneRef::Sidecar(s) => json!(s),
        _ => Value::Null,
    }
}

fn plan_json(i: usize, p: &Planned, look: &Look) -> Value {
    let j = &p.job;
    json!({
        "index": i,
        "input": j.raw,
        "stem": j.stem,
        "make": p.meta.make,
        "model": p.meta.model,
        "iso": j.iso,
        "options": options_json(&j.recipe),
        "tone_ref": j.tone_ref.kind(),
        "tone_ref_path": tone_ref_path(&j.tone_ref),
        "previews": p.meta.previews.iter().map(|(t, n)| json!({"tag": t, "bytes": n}))
            .collect::<Vec<_>>(),
        "nr": nr_json(&look.nr_params(j.iso, p.flags.strong)),
        "outputs": [
            { "kind": "full", "path": j.out_full },
            { "kind": "share", "path": j.out_share },
        ],
    })
}

/// Collect the per-frame choices from the flags and the recipe file, keyed by stem.
fn frame_flags(
    o: &Opts,
    recipe_file: Option<&RecipeFile>,
    stems: &[String],
    look: &Look,
) -> R<HashMap<String, FrameFlags>> {
    let night = look.recipe == RecipeKind::Night;
    let mut flags: HashMap<String, FrameFlags> = HashMap::new();
    type Setter = fn(&mut FrameFlags);
    let lists: [(&str, Setter); 5] = [
        ("--match", |f| f.match_sky = true),
        ("--shadow-lift", |f| f.shadow_lift = true),
        ("--recover-hl", |f| f.recover_hl = true),
        ("--strong", |f| f.strong = true),
        ("--nofringe", |f| f.nofringe = true),
    ];
    for (opt, set) in lists {
        for s in recipe::resolve(opt, &o.list(opt), stems).map_err(|e| usage_err(e.to_string()))? {
            set(flags.entry(s).or_default());
        }
    }
    if let Some(r) = recipe_file {
        for (sel, ff) in &r.frames {
            let hits = recipe::resolve("recipe frame", std::slice::from_ref(sel), stems)
                .map_err(|e| usage_err(e.to_string()))?;
            let day_used = ff.match_sky || ff.shadow_lift || ff.recover_hl;
            let night_used = ff.strong || ff.nofringe;
            if (night && day_used) || (!night && night_used) {
                return Err(usage_err(format!(
                    "recipe frame {sel}: these options are for the other recipe (look {} is {})",
                    look.name,
                    look.recipe.name()
                )));
            }
            for s in hits {
                let e = flags.entry(s).or_default();
                e.match_sky |= ff.match_sky;
                e.shadow_lift |= ff.shadow_lift;
                e.recover_hl |= ff.recover_hl;
                e.strong |= ff.strong;
                e.nofringe |= ff.nofringe;
            }
        }
    }
    let any = |f: fn(&FrameFlags) -> bool| flags.values().any(f);
    for (used, missing, what, table) in [
        (
            any(|f| f.strong),
            look.nr.strong.is_none(),
            "strong",
            "[nr.strong]",
        ),
        (
            any(|f| f.match_sky),
            look.sky_match.is_none(),
            "match",
            "[sky_match]",
        ),
        (
            any(|f| f.shadow_lift),
            look.shadow_lift.is_none(),
            "shadow-lift",
            "[shadow_lift]",
        ),
    ] {
        if used && missing {
            return Err(usage_err(format!(
                "{what} needs a {table} table in look {}",
                look.name
            )));
        }
    }
    Ok(flags)
}

fn develop_cmd(cmd: Cmd, o: &Opts, ev: &Events) -> R<i32> {
    let dry = o.has("--dry-run");
    let config = tools::load_config()?;

    // ---- the look -------------------------------------------------------------------------------
    let recipe_file = match o.path("--recipe") {
        Some(p) => Some(RecipeFile::load(&p).map_err(|e| usage_err(e.to_string()))?),
        None => None,
    };
    let default_look = match cmd {
        Cmd::Night => "rich-night",
        Cmd::Day => "rich-day",
        _ => "neutral-day",
    };
    let look_name = o
        .get("--look")
        .map(str::to_string)
        .or_else(|| recipe_file.as_ref().and_then(|r| r.look.clone()))
        .unwrap_or_else(|| default_look.to_string());
    let look = Look::load(&look_name).map_err(|e| usage_err(e.to_string()))?;
    let want = match cmd {
        Cmd::Night => Some(RecipeKind::Night),
        Cmd::Day => Some(RecipeKind::Day),
        _ => None,
    };
    if let Some(w) = want
        && look.recipe != w
    {
        return Err(usage_err(format!(
            "look {} is for the {} recipe; this is 'rawdev {}'",
            look.name,
            look.recipe.name(),
            w.name()
        )));
    }
    let night = look.recipe == RecipeKind::Night;
    for (flag, for_night) in [
        ("--match", false),
        ("--shadow-lift", false),
        ("--recover-hl", false),
        ("--strong", true),
        ("--nofringe", true),
    ] {
        if o.get(flag).is_some() && for_night != night {
            return Err(usage_err(format!(
                "{flag} is for {} looks; look {} is a {} look",
                if for_night { "night" } else { "day" },
                look.name,
                look.recipe.name()
            )));
        }
    }
    let tone_mode = match o.get("--tone-ref") {
        None => RefMode::Auto,
        Some(s) => RefMode::parse(s)
            .ok_or_else(|| usage_err(format!("--tone-ref {s}: use auto, preview or neutral")))?,
    };

    // ---- inputs, per-frame choices -------------------------------------------------------------
    let out = o
        .path("--out")
        .ok_or_else(|| usage_err("--out DIR is required"))?;
    let inputs = collect_inputs(cmd, o)?;
    let stems: Vec<String> = inputs.iter().map(|p| stem_of(p)).collect();
    let mut seen = HashMap::new();
    for (s, p) in stems.iter().zip(&inputs) {
        if let Some(prev) = seen.insert(s.clone(), p.clone()) {
            return Err(usage_err(format!(
                "two inputs would both be written as {s}.jpg: {} and {}",
                prev.display(),
                p.display()
            )));
        }
    }
    let flags = frame_flags(o, recipe_file.as_ref(), &stems, &look)?;

    // ---- safety, before anything is written ---------------------------------------------------
    let shoot = o.path("--shoot");
    safety::check_out(&out, &inputs, shoot.as_deref(), &config.protected)?;
    let outs: Vec<(PathBuf, PathBuf)> = stems
        .iter()
        .map(|s| {
            (
                out.join(format!("{s}.jpg")),
                out.join("share").join(format!("{s}.jpg")),
            )
        })
        .collect();
    if !o.has("--overwrite") {
        let existing: Vec<String> = outs
            .iter()
            .flat_map(|(a, b)| [a, b])
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if !existing.is_empty() {
            return Err(refuse(format!(
                "{} output file(s) already exist, e.g. {} (use --overwrite)",
                existing.len(),
                existing[0]
            )));
        }
    }

    // ---- tools, metadata ----------------------------------------------------------------------
    let (ex, dc) = tools::init(
        o.path("--exiftool").as_deref(),
        o.path("--dcraw-emu").as_deref(),
        &config,
    )?;
    let metas = export::read_meta(&inputs)?;

    let mut planned = Vec::new();
    for (((raw, stem), meta), (out_full, out_share)) in
        inputs.iter().zip(&stems).zip(metas).zip(outs)
    {
        let f = flags.get(stem).copied().unwrap_or_default();
        let recipe = if night {
            Recipe::Night(FrameOptions {
                defringe: !f.nofringe,
                strong_nr: f.strong,
            })
        } else {
            Recipe::Day(DayOptions {
                match_sky: f.match_sky,
                shadow_lift: f.shadow_lift,
                recover_hl: f.recover_hl,
            })
        };
        let job = FrameJob {
            stem: stem.clone(),
            raw: raw.clone(),
            tone_ref: reference::plan(raw, tone_mode, &meta.previews),
            iso: meta.iso,
            recipe,
            out_full,
            out_share,
            stage_dir: o
                .has("--dump-stages")
                .then(|| out.join("stages").join(stem)),
        };
        planned.push(Planned {
            job,
            meta,
            flags: f,
        });
    }

    let command = match cmd {
        Cmd::Night => "night",
        Cmd::Day => "day",
        _ => "develop",
    };
    let look_json = json!({
        "name": look.name, "recipe": look.recipe.name(), "origin": look.origin,
        "sha256": sha256_bytes(look.source_text.as_bytes()),
    });
    let human = Human {
        json: ev.json,
        log_file: (!dry).then(|| out.join("progress.log")),
    };

    if dry {
        ev.emit(
            "batch_start",
            json!({ "rawdev_version": VERSION, "command": command, "dry_run": true,
                    "look": look_json, "out": out, "frames": planned.len(),
                    "tools": tools_json(&ex, &dc) }),
        );
        human.say(&format!(
            "rawdev {command} --dry-run: {} frame(s), look {}, out {} (nothing will be written)",
            planned.len(),
            look.name,
            out.display()
        ));
        for (i, p) in planned.iter().enumerate() {
            ev.emit("frame_planned", plan_json(i, p, &look));
            let nr = match look.nr_params(p.job.iso, p.flags.strong) {
                Some(n) => format!("{}/{}", n.h_luma, n.h_color),
                None => "off".to_string(),
            };
            human.say(&format!(
                "  {} ({} {}, ISO {}) tone-ref {} nr={nr} {} -> {}",
                p.job.stem,
                p.meta.make,
                p.meta.model,
                p.job.iso,
                match &p.job.tone_ref {
                    ToneRef::Sidecar(s) => format!("sidecar {}", s.display()),
                    ToneRef::Preview(t) => format!("preview ({})", t.join(", ")),
                    ToneRef::Neutral => "neutral curve".to_string(),
                },
                options_json(&p.job.recipe),
                p.job.out_full.display()
            ));
        }
        ev.emit(
            "batch_end",
            json!({ "dry_run": true, "frames": planned.len(), "done": 0, "failed": 0 }),
        );
        return Ok(0);
    }

    // ---- develop --------------------------------------------------------------------------------
    std::fs::create_dir_all(out.join("share"))?;
    let started = manifest::utc_now();

    // Decode (and hash) the next RAW on a background thread while the current frame develops. The
    // channel holds at most one decoded frame, so memory stays bounded.
    let (tx, rx) = mpsc::sync_channel(1);
    let decode_list: Vec<(PathBuf, Highlights)> = planned
        .iter()
        .map(|p| (p.job.raw.clone(), p.job.recipe.highlights()))
        .collect();
    let decoder = std::thread::spawn(move || {
        for (r, hl) in decode_list {
            let sha = sha256_file(&r).map_err(|e| e.to_string());
            let t0 = Instant::now();
            let res = decode::decode_raw_with(&r, hl).map_err(|e| e.to_string());
            if tx.send((res, t0.elapsed().as_secs_f64(), sha)).is_err() {
                break;
            }
        }
    });

    // --gpu: open the graphics card once for the whole batch (compiling the shaders takes a moment).
    let want_gpu = o.has("--gpu") || o.get("--gpu-adapter").is_some();
    let (gpu_name, nlm_on) = if want_gpu {
        match gpu::enable_adapter(o.get("--gpu-adapter")) {
            Ok(name) => (json!(name), format!(", NLM on GPU: {name}")),
            Err(_) => (
                json!("cpu (no usable GPU)"),
                ", NLM on CPU (--gpu: no usable GPU)".to_string(),
            ),
        }
    } else {
        (Value::Null, String::new())
    };

    ev.emit(
        "batch_start",
        json!({ "rawdev_version": VERSION, "command": command, "dry_run": false,
                "look": look_json, "out": out, "frames": planned.len(), "gpu": gpu_name,
                "tools": tools_json(&ex, &dc) }),
    );
    let batch_start = Instant::now();
    human.log(&format!(
        "rawdev {command} batch start ({} frames, look {}{nlm_on})",
        planned.len(),
        look.name
    ))?;
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for (i, p) in planned.iter().enumerate() {
        let job = &p.job;
        let frame_start = Instant::now();
        let Ok((decoded, decode_secs, sha)) = rx.recv() else {
            return Err(Fail::from(std::io::Error::other(
                "the decoder thread stopped",
            )));
        };
        let result = decoded
            .map_err(|e| e.into())
            .and_then(|raw| develop::develop(job, &look, raw));
        match result {
            Ok(rep) => {
                let secs = frame_start.elapsed().as_secs_f64();
                human.log(&log_line(job, &rep, secs, decode_secs))?;
                let mut outputs = Vec::new();
                for (kind, path) in [("full", &job.out_full), ("share", &job.out_share)] {
                    outputs.push(json!({
                        "kind": kind, "path": path,
                        "bytes": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                        "sha256": sha256_file(path).unwrap_or_default(),
                    }));
                }
                let mut timings = serde_json::Map::new();
                timings.insert("decode".into(), json!(round3(decode_secs)));
                for (n, s) in &rep.timings {
                    timings.insert((*n).into(), json!(round3(*s)));
                }
                let rec = json!({
                    "index": i,
                    "input": job.raw,
                    "input_sha256": sha.unwrap_or_default(),
                    "stem": job.stem,
                    "make": p.meta.make,
                    "model": p.meta.model,
                    "iso": job.iso,
                    "options": options_json(&job.recipe),
                    "tone_ref": rep.tone_source,
                    "tone_ref_path": tone_ref_path(&job.tone_ref),
                    "curve": rep.info.curve,
                    "clip_g": round3(rep.info.clip_g),
                    "nr": nr_json(&rep.nr),
                    "notes": rep.notes,
                    "size": [rep.size.0, rep.size.1],
                    "outputs": outputs,
                    "timings": timings,
                    "seconds": round3(secs),
                });
                ev.emit("frame_done", rec.clone());
                done.push(rec);
            }
            Err(e) => {
                human.log(&format!("{} FAILED {e}", job.stem))?;
                let rec = json!({ "index": i, "input": job.raw, "stem": job.stem,
                                  "error": e.to_string() });
                ev.emit("frame_failed", rec.clone());
                failed.push(rec);
            }
        }
    }
    drop(rx);
    decoder.join().ok();
    let total = batch_start.elapsed().as_secs_f64();
    human.log(&format!(
        "rawdev batch end: {} frames in {:.1}s ({:.1} s/frame), {} failed",
        planned.len(),
        total,
        total / planned.len() as f64,
        failed.len()
    ))?;
    let manifest_path = out.join(manifest::MANIFEST_NAME);
    let (n_done, n_failed) = (done.len(), failed.len());
    let manifest_res = manifest::append_run(
        &out,
        json!({
            "rawdev_version": VERSION,
            "command": command,
            "argv": o.argv,
            "started": started,
            "finished": manifest::utc_now(),
            "look": look_json,
            "tools": tools_json(&ex, &dc),
            "gpu": gpu_name,
            "frames": done,
            "failed": failed,
        }),
    );
    if let Err(e) = &manifest_res {
        human.say(&format!("rawdev: could not write the manifest: {e}"));
    }
    ev.emit(
        "batch_end",
        json!({ "dry_run": false, "frames": planned.len(), "done": n_done,
                "failed": n_failed, "seconds": round3(total),
                "manifest": manifest_res.is_ok().then_some(manifest_path) }),
    );
    Ok(if n_failed == 0 && manifest_res.is_ok() {
        0
    } else {
        1
    })
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The human log line: the same fields, in the same order, as the original recipes' log lines.
fn log_line(job: &FrameJob, rep: &FrameReport, secs: f64, decode_secs: f64) -> String {
    let steps: Vec<String> = rep
        .timings
        .iter()
        .map(|(n, s)| format!("{n} {s:.2}"))
        .collect();
    let nr = match &rep.nr {
        Some(p) => format!("{}/{}", p.h_luma, p.h_color),
        None => "off".to_string(),
    };
    let curve = if rep.tone_source == "sidecar" {
        rep.info.curve.clone()
    } else {
        format!("{} [{}]", rep.info.curve, rep.tone_source)
    };
    let recipe = match job.recipe {
        Recipe::Night(o) => format!(
            "{curve} nr={nr} fringe={}",
            if o.defringe { "on" } else { "off" }
        ),
        Recipe::Day(o) => {
            let mut parts = Vec::new();
            if o.recover_hl {
                parts.push("hl2".to_string());
            }
            parts.push(curve);
            parts.extend(rep.notes.iter().map(|s| s.to_string()));
            format!("{} nr={nr}", parts.join(" "))
        }
    };
    format!(
        "{} done iso={} clipG={:.3} {recipe} size=({}, {}) {:.1}s [decode {:.2} (background) | {}]",
        job.stem,
        job.iso,
        rep.info.clip_g,
        rep.size.0,
        rep.size.1,
        secs,
        decode_secs,
        steps.join(", ")
    )
}
