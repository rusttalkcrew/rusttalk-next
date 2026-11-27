//! Sensitive pictures and videos (#682). One shows blurred until tapped when its
//! sender marked it, when this person blurred it from its menu, or, with automatic
//! detection on, when a small model on this device finds it likely nude. Nothing
//! here leaves the device: the sender's mark travels inside the encrypted message,
//! and choices and check results stay local.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::mpsc,
    thread,
};

use anyhow::Result;
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use rusttalk_core::client::{Attachment, save_json};
use serde::{Deserialize, Serialize};

use crate::server::{Id, Server};

/// Hentai + porn scores at or above this blur a picture. Set high, so ordinary
/// pictures are not blurred by mistake; marking covers what it misses (see
/// assets/sensitive/README.md).
pub const THRESHOLD: f32 = 0.8;
/// Check results kept, by picture; each is a few bytes.
const MAX_CHECKS: usize = 8192;
/// This device's own blur choices, by file, in the profile folder. Never synced.
const CHOICES: &str = "sensitive.json";
/// The blurred copy's longest side in pixels, stretched smoothly to the picture's size.
const BLUR_EDGE: usize = 12;

/// "Sensitive media" in Privacy settings. This device only, never synced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Blur pictures and videos their sender marked as sensitive.
    pub blur_marked: bool,
    /// Check pictures on this device and blur likely nude ones.
    pub detect: bool,
    /// Never blur anything, except what this person blurred from a picture's menu.
    pub show_all: bool,
    /// In NSFW channels this person agreed to, show sensitive media unblurred.
    pub nsfw_unblurred: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            blur_marked: true,
            detect: true,
            show_all: false,
            nsfw_unblurred: false,
        }
    }
}

/// How a picture shows right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hidden {
    /// Blurred without a label for a moment, while it is being checked.
    Checking,
    /// Blurred with a label until tapped.
    Sensitive,
}

enum Check {
    Running,
    Done(bool),
}

/// How likely a decoded picture is nude, from 0 to 1.
pub type Score = fn(&ColorImage) -> Result<f32>;

type Job = (Id, ColorImage, egui::Context);
/// A checked picture (by blob ID) and whether it looks nude.
type Done = (Id, bool);

pub struct Sensitive {
    pub prefs: Prefs,
    /// Set while a channel this person agreed to as NSFW is drawn (`agreed_nsfw`).
    pub nsfw_here: bool,
    /// Blur (`true`) or show, as picked from a picture's menu, by file ID.
    choices: HashMap<Id, bool>,
    /// Tapped open since the app started, by file ID.
    revealed: HashSet<Id>,
    /// By the decoded picture's blob ID: a file's thumbnail, or the file itself.
    checks: HashMap<Id, Check>,
    score: Score,
    /// Score on the UI thread: tests, with a fake detector.
    inline: bool,
    jobs: Option<mpsc::Sender<Job>>,
    results: (mpsc::Sender<Done>, mpsc::Receiver<Done>),
    /// The profile folder.
    data: PathBuf,
}

impl Sensitive {
    pub fn new(data: &Path) -> Self {
        let choices = crate::load_json::<HashMap<String, bool>>(&data.join(CHOICES))
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "reading blur choices failed");
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, blur)| Some((hex::decode(id).ok()?.try_into().ok()?, blur)))
            .collect();
        Self {
            prefs: Prefs::default(),
            nsfw_here: false,
            choices,
            revealed: HashSet::new(),
            checks: HashMap::new(),
            // Tests never load the real model unless they ask (see `set_detector` and
            // tests/sensitive_model.rs); this one finds nothing.
            #[cfg(not(test))]
            score: model::score,
            #[cfg(test)]
            score: |_| Ok(0.0),
            inline: cfg!(test),
            jobs: None,
            results: mpsc::channel(),
            data: data.to_owned(),
        }
    }

    /// Use another detector, scoring on the UI thread: for tests.
    #[cfg(test)]
    pub fn set_detector(&mut self, score: Score) {
        self.score = score;
        self.inline = true;
    }

    /// Whether `file` shows blurred here, and how. A tap or this person's own choice
    /// wins over the settings; showing everything wins over marks and checks.
    pub fn hidden(&self, file: &Attachment) -> Option<Hidden> {
        self.hidden_in(file, self.nsfw_here)
    }

    /// `hidden`, in an agreed NSFW channel or not.
    pub fn hidden_in(&self, file: &Attachment, nsfw: bool) -> Option<Hidden> {
        let id = file.file.id;
        if self.revealed.contains(&id) {
            return None;
        }
        if let Some(&blur) = self.choices.get(&id) {
            return blur.then_some(Hidden::Sensitive);
        }
        let prefs = &self.prefs;
        if prefs.show_all || (nsfw && prefs.nsfw_unblurred) {
            return None;
        }
        if prefs.blur_marked && file.sensitive() {
            return Some(Hidden::Sensitive);
        }
        if !prefs.detect {
            return None;
        }
        let mut checking = false;
        let decoded = file.thumbnail.iter().map(|thumb| thumb.id).chain([id]);
        for check in decoded.filter_map(|id| self.checks.get(&id)) {
            match check {
                Check::Done(true) => return Some(Hidden::Sensitive),
                Check::Running => checking = true,
                Check::Done(false) => {}
            }
        }
        checking.then_some(Hidden::Checking)
    }

    /// Show a blurred picture until the app closes.
    pub fn reveal(&mut self, file: &Attachment) {
        self.revealed.insert(file.file.id);
    }

    /// Blur or show a picture from now on, on this device.
    pub fn choose(&mut self, file: &Attachment, blur: bool) {
        self.revealed.remove(&file.file.id);
        self.choices.insert(file.file.id, blur);
        let saved: HashMap<String, bool> = (self.choices.iter())
            .map(|(id, blur)| (hex::encode(id), *blur))
            .collect();
        if let Err(error) = save_json(&self.data, &self.data.join(CHOICES), &saved) {
            tracing::warn!(error = %format_args!("{error:#}"), "saving blur choices failed");
        }
    }

    /// Whether a decoded picture (by blob ID) still needs a check.
    pub fn wants(&self, id: &Id) -> bool {
        self.prefs.detect && !self.prefs.show_all && !self.checks.contains_key(id)
    }

    /// Check a picture that was just decoded to be shown (`id` is its blob), once.
    pub fn check(&mut self, ctx: &egui::Context, id: Id, image: &ColorImage) {
        if !self.wants(&id) {
            return;
        }
        if self.checks.len() >= MAX_CHECKS {
            self.checks
                .retain(|_, check| matches!(check, Check::Running));
        }
        if self.inline {
            let flagged = flagged((self.score)(image));
            self.checks.insert(id, Check::Done(flagged));
            return;
        }
        let jobs = self.jobs.get_or_insert_with(|| {
            let (jobs, queue) = mpsc::channel::<Job>();
            let (done, score) = (self.results.0.clone(), self.score);
            let started = thread::Builder::new()
                .name("sensitive-check".into())
                .spawn(move || {
                    for (id, image, ctx) in queue {
                        if done.send((id, flagged(score(&image)))).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            if let Err(error) = started {
                tracing::warn!(%error, "starting the picture check failed");
            }
            jobs
        });
        if jobs.send((id, image.clone(), ctx.clone())).is_ok() {
            self.checks.insert(id, Check::Running);
        }
    }

    /// Take the checks finished since the last frame.
    pub fn poll(&mut self) {
        while let Ok((id, flagged)) = self.results.1.try_recv() {
            self.checks.insert(id, Check::Done(flagged));
        }
    }
}

/// A failed check leaves the picture as it is, with the reason logged.
fn flagged(score: Result<f32>) -> bool {
    score
        .inspect_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "checking a picture failed");
        })
        .is_ok_and(|score| score >= THRESHOLD)
}

/// A tiny copy of a picture that looks blurred when stretched to its size: nothing of
/// the picture's detail survives, only its colours. Its texture is made the first time
/// it shows, so pictures that never blur cost a few hundred bytes and no texture.
pub struct Blur {
    small: ColorImage,
    texture: std::cell::OnceCell<TextureHandle>,
}

impl Blur {
    pub fn texture(&self, ctx: &egui::Context) -> &TextureHandle {
        self.texture
            .get_or_init(|| ctx.load_texture("blurred", self.small.clone(), TextureOptions::LINEAR))
    }
}

pub fn blurred(image: &ColorImage) -> Blur {
    let [w, h] = image.size;
    let scale = (w.max(h) as f32 / BLUR_EDGE as f32).max(1.0);
    let (sw, sh) = (
        ((w as f32 / scale).round() as usize).max(1),
        ((h as f32 / scale).round() as usize).max(1),
    );
    let mut pixels = Vec::with_capacity(sw * sh);
    for y in 0..sh {
        for x in 0..sw {
            let (x0, x1) = (x * w / sw, ((x + 1) * w / sw).max(x * w / sw + 1));
            let (y0, y1) = (y * h / sh, ((y + 1) * h / sh).max(y * h / sh + 1));
            let mut sum = [0u32; 4];
            for row in y0..y1.min(h) {
                for pixel in &image.pixels[row * w + x0..row * w + x1.min(w)] {
                    for (total, part) in sum.iter_mut().zip(pixel.to_array()) {
                        *total += u32::from(part);
                    }
                }
            }
            let count = ((x1.min(w) - x0) * (y1.min(h) - y0)).max(1) as u32;
            let [r, g, b, a] = sum.map(|total| (total / count) as u8);
            pixels.push(Color32::from_rgba_premultiplied(r, g, b, a));
        }
    }
    Blur {
        small: ColorImage::new([sw, sh], pixels),
        texture: Default::default(),
    }
}

/// Whether `channel` is an NSFW channel this person agreed to, where sensitive media
/// may show unblurred when they choose that in settings.
// STUB(#681): NSFW channels are built in parallel. Until they merge, no channel is
// one; then return `channel.is_nsfw && channel.agreed` (or however #681 names them).
pub fn agreed_nsfw(_server: &Server, _channel: &Id) -> bool {
    false
}

/// The bundled model (see assets/sensitive/README.md), run with tract.
pub mod model {
    use std::sync::{Arc, OnceLock};

    use anyhow::{Result, anyhow};
    use eframe::egui::ColorImage;
    use image::{RgbaImage, imageops::FilterType};
    use tract_onnx::prelude::*;

    static BYTES: &[u8] = include_bytes!("../assets/sensitive/nsfw-mobilenet-v2-int8.onnx");
    const SIZE: usize = 224;
    /// The model's classes, in its output order.
    pub const CLASSES: [&str; 5] = ["drawings", "hentai", "neutral", "porn", "sexy"];

    type Plan = Arc<TypedSimplePlan>;

    /// Loaded on first use, on the checking thread; a failure is kept and reported.
    fn plan() -> Result<&'static Plan> {
        static PLAN: OnceLock<std::result::Result<Plan, String>> = OnceLock::new();
        PLAN.get_or_init(|| load().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("loading the picture model failed: {error}"))
    }

    fn load() -> TractResult<Plan> {
        tract_onnx::onnx()
            .model_for_read(&mut &BYTES[..])?
            .with_input_fact(0, f32::fact([1, SIZE, SIZE, 3]).into())?
            .into_optimized()?
            .into_runnable()
    }

    /// The five class scores for a picture.
    pub fn classes(image: &RgbaImage) -> Result<[f32; 5]> {
        let small = image::imageops::resize(image, SIZE as u32, SIZE as u32, FilterType::Triangle);
        let input: Tensor =
            tract_ndarray::Array4::from_shape_fn((1, SIZE, SIZE, 3), |(_, y, x, c)| {
                f32::from(small.get_pixel(x as u32, y as u32)[c]) / 255.0
            })
            .into();
        let output = plan()?.run(tvec!(input.into()))?;
        let scores = output[0].to_plain_array_view::<f32>()?;
        let mut out = [0.0; 5];
        for (slot, score) in out.iter_mut().zip(scores.iter()) {
            *slot = *score;
        }
        Ok(out)
    }

    /// How likely a picture is nude: hentai + porn. "Sexy" (swimwear and the like)
    /// is left out on purpose, to avoid false alarms.
    pub fn nude(classes: &[f32; 5]) -> f32 {
        classes[1] + classes[3]
    }

    pub fn score(image: &ColorImage) -> Result<f32> {
        let [w, h] = image.size;
        let bytes = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        let rgba = RgbaImage::from_raw(w as u32, h as u32, bytes)
            .ok_or_else(|| anyhow!("picture size doesn't match its pixels"))?;
        Ok(nude(&classes(&rgba)?))
    }
}

/// `make sensitive-check DIR=<folder>`: each picture's scores in a local folder, made
/// small first as the app's thumbnails are. Reads only; sends nothing anywhere.
pub fn check_folder(dir: Option<OsString>) -> ExitCode {
    let Some(dir) = dir.map(PathBuf::from).filter(|dir| dir.is_dir()) else {
        eprintln!("usage: make sensitive-check DIR=<folder with pictures>");
        return ExitCode::FAILURE;
    };
    let mut paths: Vec<PathBuf> = walk(&dir);
    paths.sort();
    println!(
        "{:>7} {:>7} {:>7} {:>7}  {:<7} file",
        "nude", "porn", "hentai", "sexy", "blurred"
    );
    let (mut checked, mut blurred) = (0, 0);
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // The thumbnail a sender's app makes, which is what the receiving app checks.
        let thumbnail = rusttalk_core::client::prepare_file("check", "image/png", &bytes, &bytes)
            .ok()
            .and_then(|(_, thumbnail)| thumbnail)
            .and_then(|png| rusttalk_core::client::decode_thumbnail(&png).ok());
        let Some(small) = thumbnail.map(|image| image.into_rgba8()) else {
            eprintln!(
                "{}: not a picture the app makes a thumbnail of",
                path.display()
            );
            continue;
        };
        let started = std::time::Instant::now();
        match model::classes(&small) {
            Ok(classes) => {
                let nude = model::nude(&classes);
                let flagged = nude >= THRESHOLD;
                checked += 1;
                blurred += usize::from(flagged);
                println!(
                    "{nude:>7.3} {:>7.3} {:>7.3} {:>7.3}  {:<7} {} ({} ms)",
                    classes[3],
                    classes[1],
                    classes[4],
                    if flagged { "yes" } else { "no" },
                    path.display(),
                    started.elapsed().as_millis(),
                );
            }
            Err(error) => eprintln!("{}: {error:#}", path.display()),
        }
    }
    println!("{blurred} of {checked} pictures would be blurred (threshold {THRESHOLD}).");
    ExitCode::SUCCESS
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusttalk_core::client::BlobRef;

    use super::*;

    fn picture(id: u8, thumbnail: Option<u8>) -> Attachment {
        Attachment {
            file: BlobRef::for_tests([id; 32]),
            name: format!("picture {id}.png"),
            media_type: "image/png".into(),
            thumbnail: thumbnail.map(|thumb| BlobRef::for_tests([thumb; 32])),
        }
    }

    fn marked(id: u8) -> Attachment {
        let mut file = picture(id, None);
        assert!(file.mark_sensitive());
        file
    }

    /// The fake detector: "sensitive" for a magenta picture, "not sensitive" otherwise.
    fn fake(image: &ColorImage) -> Result<f32> {
        let magenta = Color32::from_rgb(255, 0, 255);
        Ok(if image.pixels[0] == magenta { 1.0 } else { 0.0 })
    }

    fn solid(color: Color32) -> ColorImage {
        ColorImage::new([8, 6], vec![color; 48])
    }

    fn state() -> (tempfile::TempDir, Sensitive) {
        let data = tempfile::tempdir().unwrap();
        let mut sensitive = Sensitive::new(data.path());
        sensitive.set_detector(fake);
        (data, sensitive)
    }

    #[test]
    fn the_three_settings_decide_what_blurs() {
        let (_data, mut s) = state();
        let ctx = egui::Context::default();
        let (mark, flagged, plain) = (marked(1), picture(2, Some(20)), picture(3, Some(30)));
        s.check(&ctx, [20; 32], &solid(Color32::from_rgb(255, 0, 255)));
        s.check(&ctx, [30; 32], &solid(Color32::from_rgb(40, 140, 60)));
        // Defaults: marked and detected ones blur, ordinary ones don't.
        assert_eq!(s.hidden(&mark), Some(Hidden::Sensitive));
        assert_eq!(s.hidden(&flagged), Some(Hidden::Sensitive));
        assert_eq!(s.hidden(&plain), None);
        // Blur marked pictures: off.
        s.prefs.blur_marked = false;
        assert_eq!(s.hidden(&mark), None);
        assert_eq!(s.hidden(&flagged), Some(Hidden::Sensitive));
        // Detect and blur automatically: off.
        s.prefs = Prefs {
            detect: false,
            ..Prefs::default()
        };
        assert_eq!(s.hidden(&mark), Some(Hidden::Sensitive));
        assert_eq!(s.hidden(&flagged), None);
        // Show everything without blurring.
        s.prefs = Prefs {
            show_all: true,
            ..Prefs::default()
        };
        assert_eq!(s.hidden(&mark), None);
        assert_eq!(s.hidden(&flagged), None);
    }

    #[test]
    fn a_tap_shows_one_picture_until_the_app_closes() {
        let (data, mut s) = state();
        let (one, two) = (marked(1), marked(2));
        s.reveal(&one);
        assert_eq!(s.hidden(&one), None);
        assert_eq!(s.hidden(&two), Some(Hidden::Sensitive));
        assert_eq!(
            Sensitive::new(data.path()).hidden(&one),
            Some(Hidden::Sensitive)
        );
    }

    #[test]
    fn a_picture_blurred_or_shown_from_its_menu_stays_so_on_this_device() {
        let (data, mut s) = state();
        let (mark, plain) = (marked(1), picture(2, None));
        s.choose(&plain, true);
        s.choose(&mark, false);
        for s in [&mut s, &mut Sensitive::new(data.path())] {
            assert_eq!(s.hidden(&plain), Some(Hidden::Sensitive));
            assert_eq!(s.hidden(&mark), None);
            // Showing everything still keeps what this person blurred themselves.
            s.prefs.show_all = true;
            assert_eq!(s.hidden(&plain), Some(Hidden::Sensitive));
        }
    }

    #[test]
    fn agreed_nsfw_channels_show_sensitive_media_only_when_chosen() {
        let (_data, mut s) = state();
        let mark = marked(1);
        s.nsfw_here = true;
        assert_eq!(s.hidden(&mark), Some(Hidden::Sensitive), "off by default");
        s.prefs.nsfw_unblurred = true;
        assert_eq!(s.hidden(&mark), None);
        assert_eq!(s.hidden_in(&mark, false), Some(Hidden::Sensitive));
        s.nsfw_here = false;
        assert_eq!(s.hidden(&mark), Some(Hidden::Sensitive));
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counted(image: &ColorImage) -> Result<f32> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        fake(image)
    }

    #[test]
    fn each_picture_is_checked_once_and_only_when_detection_is_on() {
        let (_data, mut s) = state();
        s.set_detector(counted);
        let ctx = egui::Context::default();
        let image = solid(Color32::from_rgb(255, 0, 255));
        for _ in 0..3 {
            s.check(&ctx, [20; 32], &image);
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        s.prefs.detect = false;
        s.check(&ctx, [21; 32], &image);
        s.prefs = Prefs {
            show_all: true,
            ..Prefs::default()
        };
        s.check(&ctx, [22; 32], &image);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn checks_run_off_the_ui_thread_and_show_blurred_meanwhile() {
        let data = tempfile::tempdir().unwrap();
        let mut s = Sensitive::new(data.path());
        s.score = fake;
        s.inline = false;
        let ctx = egui::Context::default();
        let file = picture(2, Some(20));
        s.check(&ctx, [20; 32], &solid(Color32::from_rgb(255, 0, 255)));
        assert_eq!(s.hidden(&file), Some(Hidden::Checking));
        for _ in 0..200 {
            s.poll();
            if s.hidden(&file) != Some(Hidden::Checking) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(s.hidden(&file), Some(Hidden::Sensitive));
    }

    #[test]
    fn the_blurred_copy_keeps_only_colours() {
        let ctx = egui::Context::default();
        let textures = || ctx.tex_manager().read().num_allocated();
        let before = textures();
        let image = ColorImage::new([320, 200], vec![Color32::from_rgb(10, 200, 30); 64_000]);
        let blur = blurred(&image);
        assert_eq!(textures(), before, "no texture until shown");
        assert_eq!(blur.texture(&ctx).size(), [12, 8]);
        assert_eq!(textures(), before + 1);
    }
}
