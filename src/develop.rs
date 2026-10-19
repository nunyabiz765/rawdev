//! One frame, start to finish: the whole night (or day) recipe in order, with a timing for every step.

use crate::Result;
use crate::day::{self, DayOptions};
use crate::decode::{self, Highlights};
use crate::export::{self, Chroma};
use crate::image::Rgb8;
use crate::look::{self, NrParams};
use crate::lookfile::Look;
use crate::pillow::{self, Filter};
use crate::reference::{self, ToneRef};
use crate::tone;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Per-frame choices that aren't automatic (they come from the shoot's selection notes).
#[derive(Clone, Copy, Debug)]
pub struct FrameOptions {
    /// Run the purple-fringe cleanup (off for frames whose real light is purple/magenta).
    pub defringe: bool,
    /// Stronger noise reduction for saturated coloured-LED frames.
    pub strong_nr: bool,
}

/// Which recipe a frame gets, with its per-frame choices.
#[derive(Clone, Copy, Debug)]
pub enum Recipe {
    Night(FrameOptions),
    Day(DayOptions),
}

impl Recipe {
    /// The dcraw_emu highlight mode this frame needs.
    pub fn highlights(&self) -> Highlights {
        match self {
            Recipe::Day(d) if d.recover_hl => Highlights::Blend,
            _ => Highlights::Clip,
        }
    }
}

/// Where a frame's inputs live and where its outputs go.
pub struct FrameJob {
    /// the output name (the RAW's file stem, e.g. `IMG_0001`)
    pub stem: String,
    pub raw: PathBuf,
    /// what the tone curve is fitted to (see `reference.rs`)
    pub tone_ref: ToneRef,
    pub iso: u32,
    pub recipe: Recipe,
    /// full-size output and the share copy (q92 4:4:4 and 2000 px q90 in the rich looks)
    pub out_full: PathBuf,
    pub out_share: PathBuf,
    /// if set, also dump each intermediate stage as raw RGB bytes here (for parity checks)
    pub stage_dir: Option<PathBuf>,
}

/// What happened, for the log line and the benchmark.
pub struct FrameReport {
    pub size: (usize, usize),
    pub info: tone::ToneInfo,
    /// `None` when NR was skipped (the look's `nr.off_max_iso`: ISO <= 200 in rich-day)
    pub nr: Option<NrParams>,
    /// extra notes for the log line (e.g. "matrix-fit shadow-lift")
    pub notes: Vec<&'static str>,
    pub timings: Vec<(&'static str, f64)>,
    /// what the curve was fitted to: "sidecar", "preview:<tag>" or "neutral"
    pub tone_source: String,
}

impl FrameReport {
    pub fn total(&self) -> f64 {
        self.timings.iter().map(|t| t.1).sum()
    }
}

/// Small helper: run a step, remember how long it took.
struct Timer {
    timings: Vec<(&'static str, f64)>,
}

impl Timer {
    fn step<T>(&mut self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let t0 = Instant::now();
        let out = f();
        self.timings.push((name, t0.elapsed().as_secs_f64()));
        out
    }
}

/// Decode the camera JPEG and shrink it to fit 800 x 800 exactly like `PIL.Image.thumbnail((800, 800))`.
/// (Only its brightness distribution is used, to fit the tone curve.)
pub fn camera_thumbnail(path: &Path) -> Result<Rgb8> {
    let full = export::read_jpeg(path)?;
    Ok(pillow::thumbnail(&full, 800, 800, Filter::Bicubic))
}

/// Load the frame's tone reference (timed as "camera jpeg" for a sidecar, "preview jpeg" otherwise).
fn load_reference(
    t: &mut Timer,
    job: &FrameJob,
    raw: &decode::Linear16,
) -> Result<Option<reference::Loaded>> {
    let name = match job.tone_ref {
        ToneRef::Sidecar(_) => "camera jpeg",
        ToneRef::Preview(_) => "preview jpeg",
        ToneRef::Neutral => return Ok(None),
    };
    t.step(name, || {
        reference::load(&job.raw, &job.tone_ref, (raw.width, raw.height))
    })
}

fn source_of(r: &Option<reference::Loaded>) -> String {
    r.as_ref()
        .map(|l| l.source.clone())
        .unwrap_or_else(|| "neutral".to_string())
}

fn dump(dir: &Option<PathBuf>, name: &str, img: &Rgb8) -> Result<()> {
    if let Some(d) = dir {
        std::fs::write(d.join(format!("{name}.bin")), &img.data)?;
    }
    Ok(())
}

/// Develop one frame with its recipe and look. (The caller checks that the look's `recipe` matches.)
pub fn develop(job: &FrameJob, look: &Look, raw: decode::Linear16) -> Result<FrameReport> {
    match job.recipe {
        Recipe::Night(o) => develop_night(job, look, o, raw),
        Recipe::Day(o) => develop_day(job, look, o, raw),
    }
}

/// The night recipe (ported from the Python night prototype).
fn develop_night(
    job: &FrameJob,
    look: &Look,
    options: FrameOptions,
    raw: decode::Linear16,
) -> Result<FrameReport> {
    let mut t = Timer {
        timings: Vec::new(),
    };
    if let Some(d) = &job.stage_dir {
        std::fs::create_dir_all(d)?;
    }

    let reference = load_reference(&mut t, job, &raw)?;
    let tone_source = source_of(&reference);
    let cam = reference.map(|r| pillow::thumbnail(&r.image, 800, 800, Filter::Bicubic));
    if let Some(c) = &cam {
        dump(&job.stage_dir, "cam_thumb", c)?;
    }
    let (toned, info) = t.step("tone", || {
        tone::tone_with(
            &raw,
            cam.as_ref(),
            look.tone.curve_scale,
            look.tone.vibrance,
            look.tone.neutral,
        )
    });
    if let Some(d) = &job.stage_dir {
        let bytes: Vec<u8> = info.lut.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(d.join("lut_f32.bin"), bytes)?;
    }
    drop(raw); // free the 16-bit buffer (~145 MB) as soon as we're done with it
    dump(&job.stage_dir, "s1_tone", &toned)?;

    // noise first, before any chroma lift (which would amplify colour speckle)
    let nr = look.nr_params(job.iso, options.strong_nr);
    let img = match &nr {
        Some(p) => t.step("noise reduction", || look::noise_reduction(&toned, p)),
        None => toned,
    };
    dump(&job.stage_dir, "s2_nr", &img)?;

    let img = t.step("chroma lift", || look::chroma_lift(&img, &look.chroma));
    dump(&job.stage_dir, "s3_chroma", &img)?;

    let img = if options.defringe && look.defringe.enabled {
        t.step("defringe", || look::defringe(&img, &look.defringe))
    } else {
        img
    };
    dump(&job.stage_dir, "s4_fringe", &img)?;

    finish(job, look, img, t, info, nr, Vec::new(), tone_source)
}

/// The day recipe (ported from the Python day prototype). See `day.rs` for what differs.
fn develop_day(
    job: &FrameJob,
    look: &Look,
    options: DayOptions,
    raw: decode::Linear16,
) -> Result<FrameReport> {
    let mut t = Timer {
        timings: Vec::new(),
    };
    if let Some(d) = &job.stage_dir {
        std::fs::create_dir_all(d)?;
    }
    let mut notes = Vec::new();
    let frame = (raw.width, raw.height);
    let reference = load_reference(&mut t, job, &raw)?;
    let tone_source = source_of(&reference);
    let cam_full = reference.map(|r| r.image);
    let cam = cam_full.as_ref().map(|c| {
        t.step("camera thumb", || {
            pillow::thumbnail(c, 800, 800, Filter::Bicubic)
        })
    });
    let (mut rgb, info) = t.step("tone", || {
        tone::tone_linear(&raw, cam.as_ref(), look.tone.curve_scale, look.tone.neutral)
    });
    drop(raw);

    let mut sky_mask = None;
    if let (true, Some(sm)) = (options.match_sky, &look.sky_match) {
        // the matrix compares the frame and the reference pixel for pixel at 600 x 400, so the
        // reference has to exist, be big enough and line up (same aspect, same orientation)
        match &cam_full {
            None => notes.push("matrix-fit skipped (no camera reference)"),
            Some(c) if c.width < 600 || c.height < 400 || !reference::lines_up(c, frame) => {
                notes.push("matrix-fit skipped (reference doesn't line up)")
            }
            Some(c) => match t.step("sky matrix", || day::match_sky(&mut rgb, c, sm)) {
                Some((w, _m)) => {
                    sky_mask = Some(w);
                    notes.push("matrix-fit");
                }
                None => notes.push("matrix-fit skipped (no usable sky)"),
            },
        }
    }
    drop(cam_full);
    if let (true, Some(sl)) = (options.shadow_lift, &look.shadow_lift) {
        t.step("shadow lift", || day::shadow_lift(&mut rgb, sl));
        notes.push("shadow-lift");
    }
    let toned = t.step("srgb encode", || {
        tone::encode_srgb(&rgb, look.tone.vibrance)
    });
    drop(rgb);
    dump(&job.stage_dir, "s1_tone", &toned)?;

    let nr = look.nr_params(job.iso, false);
    let img = match &nr {
        Some(p) => t.step("noise reduction", || look::noise_reduction(&toned, p)),
        None => toned,
    };
    dump(&job.stage_dir, "s2_nr", &img)?;

    let img = t.step("chroma lift", || {
        look::chroma_lift_weighted(&img, sky_mask.as_ref(), &look.chroma)
    });
    drop(sky_mask);
    dump(&job.stage_dir, "s3_chroma", &img)?;
    let img = if look.defringe.enabled {
        t.step("defringe", || look::defringe(&img, &look.defringe))
    } else {
        img
    };
    let img = match &look.ca_fringe {
        Some(c) if c.enabled => t.step("ca fringe", || day::ca_fringe(&img, c)),
        _ => img,
    };
    dump(&job.stage_dir, "s4_fringe", &img)?;

    finish(job, look, img, t, info, nr, notes, tone_source)
}

/// The look file's "4:4:4" / "4:2:0" (already validated by `Look::parse`).
fn chroma(s: &str) -> Chroma {
    if s == "4:2:0" {
        Chroma::Half420
    } else {
        Chroma::Full444
    }
}

/// The shared tail of both recipes: unsharp mask, share copy, JPEG encodes, EXIF/GPS copy.
#[allow(clippy::too_many_arguments)]
fn finish(
    job: &FrameJob,
    look: &Look,
    img: Rgb8,
    mut t: Timer,
    info: tone::ToneInfo,
    nr: Option<NrParams>,
    notes: Vec<&'static str>,
    tone_source: String,
) -> Result<FrameReport> {
    let s = &look.sharpen;
    let img = t.step("unsharp", || {
        pillow::unsharp_mask(&img, s.radius, s.percent, s.threshold)
    });
    dump(&job.stage_dir, "s5_usm", &img)?;

    let share = t.step("share resize", || {
        let n = look.export.share_long_edge;
        pillow::thumbnail(&img, n, n, Filter::Lanczos)
    });
    dump(&job.stage_dir, "s6_share", &share)?;

    let e = &look.export;
    // the two JPEG encodes are independent, so run them side by side
    let (a, b) = t.step("jpeg encode", || {
        rayon::join(
            || export::write_jpeg(&job.out_full, &img, e.full_quality, chroma(&e.full_chroma)),
            || {
                export::write_jpeg(
                    &job.out_share,
                    &share,
                    e.share_quality,
                    chroma(&e.share_chroma),
                )
            },
        )
    });
    a?;
    b?;
    t.step("exif copy", || {
        export::copy_exif(&job.raw, &[&job.out_full, &job.out_share])
    })?;

    Ok(FrameReport {
        size: (img.width, img.height),
        info,
        nr,
        notes,
        timings: t.timings,
        tone_source,
    })
}
