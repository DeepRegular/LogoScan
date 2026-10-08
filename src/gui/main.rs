#![cfg_attr(windows, windows_subsystem = "windows")]

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use eframe::egui::{self, Color32, CursorIcon, Key, PointerButton, Pos2, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use lgdscan::anim::{self, AnimJob, AnimOutcome, Stage};
use lgdscan::detect::{self, Candidate, DetectOptions, Detection};
use lgdscan::erase;
use lgdscan::job::{self, Job, Outcome};
use lgdscan::lgd::{self, Logo};
use lgdscan::scan::Background;
use lgdscan::source::{self, ReadOptions, Reader, Rect, Scan, VideoInfo};
use lgdscan::spans::{self, Fades, Span};

fn main() -> eframe::Result {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 860.0])
            .with_title("lgdscan — ロゴ解析")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png")).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native(
        "lgdscan",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            let mut app = App::default();
            for a in args {
                app.open_path(&cc.egui_ctx, a);
            }
            Ok(Box::new(app))
        }),
    )
}

/// egui's own fonts have no kana or kanji; add a system font as fallback.
fn install_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/ipafont-gothic/ipagp.ttf",
        "/usr/share/fonts/truetype/fonts-japanese-gothic.ttf",
        "C:\\Windows\\Fonts\\YuGothM.ttc",
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("jp".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("jp".into());
    }
    ctx.set_fonts(fonts);
}

/// Work on another thread, with progress and a way to stop it.
/// The stretches found in an input: the input, the stretches, the first
/// frame read, and the station's fades.
type Found = (PathBuf, Vec<Span>, u64, Fades);

struct Task<T> {
    rx: mpsc::Receiver<Result<T, String>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<(f32, String)>>,
}

impl<T: Send + 'static> Task<T> {
    fn spawn(
        ctx: &egui::Context,
        f: impl FnOnce(&AtomicBool, &(dyn Fn(f32, String) + Sync)) -> Result<T, String> + Send + 'static,
    ) -> Task<T> {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((0.0, String::new())));
        let (c, p, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let report = |f: f32, s: String| {
                *p.lock().unwrap() = (f, s);
                ctx.request_repaint();
            };
            let r = f(&c, &report);
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        Task { rx, cancel, progress }
    }

    fn poll(&self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("処理が途中で止まりました".into())),
        }
    }

    fn progress(&self) -> (f32, String) {
        self.progress.lock().unwrap().clone()
    }
}

struct Grabbed {
    time: f64,
    /// The picture size of the frame, which may be from an input no longer
    /// shown.
    size: (u32, u32),
    /// For a moving logo: the input and frame shown.
    frame: Option<(usize, u64)>,
    rgb: Vec<u8>,
    erased: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Still,
    Moving,
}

/// A moving-logo analysis with what it was run on: the inputs, the start
/// of the range, and where that start is in each input, in frames.
struct AnimRun {
    outcome: AnimOutcome,
    inputs: Vec<PathBuf>,
    start: Option<f64>,
    offsets: Vec<i64>,
}

/// A moving logo: one logo per frame of the animation.
struct Anim {
    logos: Vec<Logo>,
    /// The logo the animation settles into, when it was analysed here.
    still: Option<Logo>,
    /// Per input: where the animation starts, in frames from `start`.
    starts: Vec<(PathBuf, i64)>,
    /// Per input: the frame `start` falls on, counted from the first.
    offsets: Vec<i64>,
    /// What the frames were counted from (the range, when one was set).
    start: Option<f64>,
    /// Recordings each frame was fitted on.
    samples: Vec<usize>,
    /// delogomod's end and fadeout, when measured.
    hold: Option<anim::Hold>,
    /// The frame and input looked at.
    k: usize,
    input: usize,
    tex: Option<(usize, TextureHandle)>,
    /// What ffprobe said about the inputs looked at.
    infos: std::collections::HashMap<usize, Option<VideoInfo>>,
}

#[derive(Clone, Copy)]
enum Drag {
    New { anchor: Pos2 },
    Move { from: Pos2, start: Rect },
    Resize { left: bool, top: bool, right: bool, bottom: bool, start: Rect },
}

struct View {
    /// Screen points per picture pixel.
    scale: f32,
    /// Picture coordinates shown at the middle of the view.
    center: Vec2,
    fit: bool,
}

impl View {
    fn to_screen(&self, area: egui::Rect, p: Pos2) -> Pos2 {
        area.center() + (p.to_vec2() - self.center) * self.scale
    }
    fn to_image(&self, area: egui::Rect, s: Pos2) -> Pos2 {
        ((s - area.center()) / self.scale + self.center).to_pos2()
    }
    fn rect_to_screen(&self, area: egui::Rect, r: Rect) -> egui::Rect {
        egui::Rect::from_min_max(
            self.to_screen(area, Pos2::new(r.x as f32, r.y as f32)),
            self.to_screen(area, Pos2::new((r.x + r.w) as f32, (r.y + r.h) as f32)),
        )
    }
}

struct App {
    inputs: Vec<PathBuf>,
    info: Option<VideoInfo>,
    /// The time of the first picture of the first input: frame 0.
    first: Option<f64>,
    status: String,

    time: f64,
    shown_time: Option<f64>,
    frame_rgb: Option<Vec<u8>>,
    /// The size of the picture in `frame_rgb` and `erased_rgb`.
    frame_size: (u32, u32),
    erased_rgb: Option<Vec<u8>>,
    frame_tex: Option<TextureHandle>,
    tex_nearest: bool,
    tex_erased: bool,
    grab: Option<Task<Grabbed>>,
    grab_again: bool,

    rect: Rect,
    drag: Option<Drag>,
    view: View,

    detect_task: Option<Task<Detection>>,
    detection: Option<Detection>,
    share: f32,
    margin: u32,
    candidates: Vec<Candidate>,
    show_presence: bool,
    presence_tex: Option<(f32, TextureHandle)>,

    range_on: bool,
    range: (f64, f64),
    step: u32,
    threshold: f64,
    background: Background,
    scan: Scan,
    scan_task: Option<Task<Outcome>>,
    scan_rect: Rect,

    logo: Option<Logo>,
    logo_tex: Option<TextureHandle>,
    outcome: Option<Outcome>,
    /// Where the station logo is on screen in the first input, and the
    /// frame the stretches count from.
    spans_task: Option<Task<Found>>,
    spans: Option<Found>,
    /// Where the logo shown was last saved, so stretches found after the
    /// save can still go into the sample beside it.
    saved: Option<PathBuf>,
    show_erased: bool,
    name: String,

    mode: Mode,
    search: usize,
    anim_task: Option<Task<AnimRun>>,
    anim: Option<Anim>,
    /// Showing a frame of the animation rather than the time on the slider.
    anim_preview: bool,
    /// Write a sample AviSynth script beside each logo file saved.
    write_sample: bool,
    shown_frame: Option<(usize, u64)>,
}

impl Default for App {
    fn default() -> App {
        App {
            inputs: Vec::new(),
            info: None,
            first: None,
            status: "録画ファイルをウィンドウに落とすか、「開く…」で選んでください".into(),
            time: 0.0,
            shown_time: None,
            frame_rgb: None,
            frame_size: (0, 0),
            erased_rgb: None,
            frame_tex: None,
            tex_nearest: false,
            tex_erased: false,
            grab: None,
            grab_again: false,
            rect: Rect { x: 0, y: 0, w: 0, h: 0 },
            drag: None,
            view: View { scale: 1.0, center: Vec2::ZERO, fit: true },
            detect_task: None,
            detection: None,
            share: 0.45,
            margin: 3,
            candidates: Vec::new(),
            show_presence: true,
            presence_tex: None,
            range_on: false,
            range: (0.0, 0.0),
            step: 1,
            threshold: 12.0,
            background: Background::Plane,
            scan: Scan::Auto,
            scan_task: None,
            scan_rect: Rect { x: 0, y: 0, w: 0, h: 0 },
            logo: None,
            logo_tex: None,
            outcome: None,
            spans_task: None,
            spans: None,
            saved: None,
            show_erased: false,
            name: String::new(),
            mode: Mode::Still,
            search: 90,
            anim_task: None,
            anim: None,
            anim_preview: false,
            write_sample: true,
            shown_frame: None,
        }
    }
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let s = t % 60.0;
    format!("{h}:{m:02}:{s:06.3}")
}

fn rect_text(r: Rect) -> String {
    format!("{},{}  {}×{}", r.x, r.y, r.w, r.h)
}

fn same_rect(a: Rect, b: Rect) -> bool {
    (a.x, a.y, a.w, a.h) == (b.x, b.y, b.w, b.h)
}

impl App {
    fn open_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        if is_logo_file(&path) {
            self.load_lgd(ctx, &path);
            return;
        }
        if self.inputs.is_empty() {
            self.set_inputs(ctx, vec![path]);
        } else if !self.inputs.contains(&path) {
            self.inputs.push(path);
        }
    }

    fn set_inputs(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        let Some(first) = inputs.first() else { return };
        match source::probe(first) {
            Ok(info) => {
                self.status = format!(
                    "{}×{}  {:.3} fps  {}  {}",
                    info.width,
                    info.height,
                    info.frame_rate,
                    fmt_time(info.duration),
                    if info.interlaced { "インターレース" } else { "プログレッシブ" }
                );
                self.time = (info.duration / 2.0).max(0.0);
                self.range = (0.0, info.duration);
                if self.rect.w == 0 || self.rect.x + self.rect.w > info.width || self.rect.y + self.rect.h > info.height {
                    self.rect = Rect { x: info.width * 3 / 4, y: info.height / 20, w: info.width / 6, h: info.height / 12 };
                }
                self.first = source::first_picture(first).ok();
                self.info = Some(info);
                self.inputs = inputs;
                self.view.fit = true;
                // A detection still running is of the previous recording.
                if let Some(t) = self.detect_task.take() {
                    t.cancel.store(true, Ordering::Relaxed);
                }
                self.leave_anim_preview();
                self.detection = None;
                self.candidates.clear();
                self.presence_tex = None;
                self.frame_rgb = None;
                self.erased_rgb = None;
                self.frame_tex = None;
                self.shown_time = None;
                self.request_frame(ctx);
            }
            Err(e) => self.status = format!("開けません: {e}"),
        }
    }

    fn load_lgd(&mut self, ctx: &egui::Context, path: &Path) {
        let r = File::open(path).and_then(|f| lgd::read(BufReader::new(f)));
        match r {
            Ok(logos) if logos.len() > 1 => {
                self.status = format!("{} を読みました（動くロゴ、{} フレーム）", path.display(), logos.len());
                self.mode = Mode::Moving;
                let samples = vec![0; logos.len()];
                self.anim = Some(Anim { logos, still: None, starts: Vec::new(), offsets: Vec::new(), start: None, samples, hold: None, k: 0, input: 0, tex: None, infos: Default::default() });
                self.anim_preview = false;
            }
            Ok(logos) if !logos.is_empty() => {
                let l = logos.into_iter().next().unwrap();
                self.rect = Rect { x: l.x.max(0) as u32, y: l.y.max(0) as u32, w: l.w as u32, h: l.h as u32 };
                self.name = l.name_lossy();
                self.status = format!("{} を読みました（{}）", path.display(), rect_text(self.rect));
                self.set_logo(ctx, l, None);
            }
            Ok(_) => self.status = "ロゴが入っていません".into(),
            Err(e) => self.status = format!("{}: {e}", path.display()),
        }
    }

    fn set_logo(&mut self, ctx: &egui::Context, logo: Logo, outcome: Option<Outcome>) {
        let hd = self.info.as_ref().is_none_or(source::is_hd);
        let rgb = erase::logo_to_rgb(&logo, hd);
        let img = egui::ColorImage::from_rgb([logo.w as usize, logo.h as usize], &rgb);
        self.logo_tex = Some(ctx.load_texture("logo", img, TextureOptions::NEAREST));
        self.logo = Some(logo);
        self.outcome = outcome;
        self.spans = None;
        self.saved = None;
        // Stretches still being looked for are of the previous logo.
        if let Some(t) = self.spans_task.take() {
            t.cancel.store(true, Ordering::Relaxed);
        }
        self.erased_rgb = None;
        self.frame_tex = None;
        if self.show_erased {
            self.request_frame(ctx);
        }
    }

    fn request_frame(&mut self, ctx: &egui::Context) {
        if self.grab.is_some() {
            self.grab_again = true;
            return;
        }
        if self.anim_preview {
            let info = self.preview_info();
            if let (Some(a), Some(info)) = (&self.anim, info) {
                if let Some((path, s)) = a.starts.get(a.input) {
                    let (path, k, input, start) = (path.clone(), a.k, a.input, a.start);
                    let n = (s + k as i64).max(0) as u64;
                    let logo = if self.show_erased { a.logos.get(k).cloned() } else { None };
                    let at = self.time;
                    self.grab = Some(Task::spawn(ctx, move |_, _| {
                        let rgb = source::grab_rgb_frame(&path, &info, start, n).map_err(|e| e.to_string())?;
                        let erased = logo.and_then(|l| erased_frame(&path, &info, &l, start, n, &rgb).ok());
                        Ok(Grabbed { time: at, size: (info.width, info.height), frame: Some((input, n)), rgb, erased })
                    }));
                    return;
                }
            }
        }
        let (Some(path), Some(info)) = (self.inputs.first().cloned(), self.info.clone()) else { return };
        let at = self.time;
        let logo = if self.show_erased { self.logo.clone() } else { None };
        self.grab = Some(Task::spawn(ctx, move |_, _| {
            let rgb = source::grab_rgb(&path, &info, at).map_err(|e| e.to_string())?;
            let erased = logo.and_then(|l| erased_picture(&path, &info, &l, at, &rgb).ok());
            Ok(Grabbed { time: at, size: (info.width, info.height), frame: None, rgb, erased })
        }));
    }

    fn poll_tasks(&mut self, ctx: &egui::Context) {
        self.poll_anim(ctx);
        if let Some(r) = self.grab.as_ref().and_then(|t| t.poll()) {
            self.grab = None;
            match r {
                Ok(g) => {
                    self.shown_time = Some(g.time);
                    self.shown_frame = g.frame;
                    self.frame_rgb = Some(g.rgb);
                    self.frame_size = g.size;
                    self.erased_rgb = g.erased;
                    self.frame_tex = None;
                }
                Err(e) => self.status = e,
            }
            if self.grab_again {
                self.grab_again = false;
                self.request_frame(ctx);
            }
        }
        if let Some(r) = self.detect_task.as_ref().and_then(|t| t.poll()) {
            self.detect_task = None;
            match r {
                Ok(d) => {
                    self.detection = Some(d);
                    self.refresh_candidates(true);
                }
                Err(e) => self.status = e,
            }
        }
        if let Some(r) = self.spans_task.as_ref().and_then(|t| t.poll()) {
            self.spans_task = None;
            match r {
                Ok(found) if found.1.is_empty() => self.status = "ロゴの出ている区間が見つかりませんでした".into(),
                Ok(found) => {
                    self.status = format!("ロゴの出ている区間が {} か所見つかりました", found.1.len());
                    self.spans = Some(found);
                    // Saved before the stretches were found: the sample
                    // beside it was written without them.
                    if let Some(path) = self.saved.clone().filter(|_| self.write_sample) {
                        self.status = match self.write_still_sample(&path) {
                            Ok(sample) => format!("{}。サンプル {} にフェードを書きました", self.status, sample.display()),
                            Err(e) => format!("{}。サンプルは書けません: {e}", self.status),
                        };
                    }
                }
                Err(e) => self.status = e,
            }
        }
        if let Some(r) = self.scan_task.as_ref().and_then(|t| t.poll()) {
            self.scan_task = None;
            match r {
                Ok(o) => {
                    let logo = o.logo(Vec::new(), self.scan_rect);
                    self.status = if o.peak < 50 {
                        format!("ロゴが見つかりませんでした（dp の最大 {}）。範囲か閾値を見直してください", o.peak)
                    } else {
                        format!("解析が終わりました（{} フレームを使用）", o.frames_used)
                    };
                    self.show_erased = true;
                    self.show_presence = false;
                    self.set_logo(ctx, logo, Some(o));
                }
                Err(e) => self.status = e,
            }
        }
    }

    fn poll_anim(&mut self, ctx: &egui::Context) {
        let Some(r) = self.anim_task.as_ref().and_then(|t| t.poll()) else { return };
        self.anim_task = None;
        match r {
            Ok(run) => {
                let o = run.outcome;
                let logos: Vec<Logo> = o
                    .frames
                    .iter()
                    .enumerate()
                    .map(|(k, f)| Logo {
                        name: k.to_string().into_bytes(),
                        x: f.rect.x as i16,
                        y: f.rect.y as i16,
                        w: f.rect.w as i16,
                        h: f.rect.h as i16,
                        pixels: f.pixels.clone(),
                        ..Default::default()
                    })
                    .collect();
                let least = o.frames.iter().map(|f| f.samples).min().unwrap_or(0);
                self.status = format!("解析が終わりました（{} フレーム、どのフレームも {least} 本以上の録画から）", logos.len());
                for w in &o.warnings {
                    self.status = format!("{}。{w}", self.status);
                }
                // The inputs as they were when the analysis started: the
                // list may have changed since.
                let starts = run.inputs.into_iter().zip(o.starts.iter().copied()).collect();
                let (start, offsets) = (run.start, run.offsets);
                self.anim = Some(Anim {
                    samples: o.frames.iter().map(|f| f.samples).collect(),
                    hold: o.hold,
                    logos,
                    still: Some(o.still),
                    starts,
                    offsets,
                    start,
                    k: 0,
                    input: 0,
                    tex: None,
                    infos: Default::default(),
                });
                self.show_erased = true;
                self.show_presence = false;
                self.anim_preview = true;
                self.erased_rgb = None;
                self.frame_tex = None;
                self.request_frame(ctx);
            }
            Err(e) => self.status = e,
        }
    }

    fn start_anim(&mut self, ctx: &egui::Context) {
        if self.inputs.len() < 3 {
            self.status = "動くロゴには、アニメーションの映っている録画が 3 本以上要ります（「追加…」で足してください）".into();
            return;
        }
        let mut job = AnimJob::new(self.inputs.clone(), self.rect);
        if self.range_on {
            job.start = Some(self.range.0);
            job.end = Some(self.range.1);
        }
        job.scan = self.scan;
        job.search = self.search;
        self.anim_task = Some(Task::spawn(ctx, move |cancel, report| {
            let outcome = anim::run(
                &job,
                &|p| {
                    let (i, what) = match p.stage {
                        Stage::Align => (0, "録画どうしの位置を合わせています".to_string()),
                        Stage::Locate => (1, "局ロゴを探しています".to_string()),
                        Stage::Still => (2, "局ロゴを当てはめています".to_string()),
                        Stage::Coarse => (3, "粗く当てはめています".to_string()),
                        Stage::Fine(n) => (3 + n as usize, format!("当てはめています（{n}/4）")),
                        Stage::Fade => (8, "局ロゴの消え方を測っています".to_string()),
                    };
                    let f = (i as f64 + p.done as f64 / p.total.max(1) as f64) / 9.0;
                    report(f as f32, format!("{what}  {}/{} 本", p.done, p.total));
                },
                cancel,
            )?;
            // The starts count from the range; the call counts from the
            // first picture of each recording.
            let offsets = match job.start {
                None => vec![0; job.inputs.len()],
                Some(s) => job
                    .inputs
                    .iter()
                    .map(|p| {
                        let info = source::probe(p).map_err(|e| e.to_string())?;
                        let first = source::first_picture(p).map_err(|e| e.to_string())?;
                        Ok(source::clock_frame(&info, first, s) as i64)
                    })
                    .collect::<Result<_, String>>()?,
            };
            Ok(AnimRun { outcome, inputs: job.inputs, start: job.start, offsets })
        }));
    }

    fn save_ldp(&mut self) {
        let Some(a) = &self.anim else { return };
        let default = if self.name.is_empty() { "logo.ldp".to_string() } else { format!("{}.ldp", self.name) };
        let Some(path) = rfd::FileDialog::new().add_filter("動くロゴ", &["ldp"]).set_file_name(&default).save_file() else { return };
        let r = File::create(&path).and_then(|f| {
            let mut w = BufWriter::new(f);
            lgd::write(&mut w, &a.logos)?;
            w.flush()
        });
        self.status = match r {
            Ok(()) => {
                let mut msg = format!("{} に保存しました（{} フレーム）", path.display(), a.logos.len());
                if self.write_sample {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let sample = lgdscan::avs::path_for(&path);
                    msg = match lgdscan::avs::write(&sample, &lgdscan::avs::moving(&name, a.logos.len(), a.hold)) {
                        Ok(()) => format!("{msg}。サンプルを {} に書きました", sample.display()),
                        Err(e) => format!("{msg}。サンプルは書けません: {e}"),
                    };
                }
                msg
            }
            Err(e) => format!("保存できません: {e}"),
        };
    }

    /// delogo's EraseLOGO for the station logo, over the analysed range or
    /// the whole recording.
    fn still_call(&self) -> Option<String> {
        let info = self.info.as_ref()?;
        if info.frame_rate <= 0.0 {
            return None;
        }
        // Frame 0 is the first picture that decodes, as AviSynth counts.
        let first = self.first.unwrap_or(info.start_time);
        let frame = |t: f64| source::clock_frame(info, first, t);
        let (start, end) = if self.range_on {
            (frame(self.range.0), frame(self.range.1).saturating_sub(1))
        } else {
            (0, frame(info.duration).saturating_sub(1))
        };
        let interlaced = match self.scan {
            Scan::Auto => info.interlaced,
            Scan::Progressive => false,
            Scan::Interlaced => true,
        };
        let lgd = if self.name.is_empty() { "logo.lgd".to_string() } else { format!("{}.lgd", self.name) };
        if let Some((_, spans, offset, _)) = self.found_spans() {
            return Some(spans::erase_call(&lgd, spans, *offset, interlaced));
        }
        Some(format!("EraseLOGO(logofile=\"{lgd}\", start={start}, end={end}, interlaced={interlaced})"))
    }

    /// The stretches found, while they still belong to the first input.
    fn found_spans(&self) -> Option<&Found> {
        self.spans.as_ref().filter(|(p, ..)| self.inputs.first() == Some(p))
    }

    /// Saves the calls for the recording read, as a script of its own.
    fn save_recording_avs(&mut self, call: &str) {
        let Some(input) = self.inputs.first() else { return };
        let video = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let stem = input.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lgd = if self.name.is_empty() { "logo.lgd".to_string() } else { format!("{}.lgd", self.name) };
        let Some(path) = rfd::FileDialog::new().add_filter("AviSynth", &["avs"]).set_file_name(format!("{stem}.avs")).save_file() else { return };
        if self.inputs.iter().any(|p| p == &path) {
            self.status = "録画と同じファイルには書けません".into();
            return;
        }
        self.status = match lgdscan::avs::write(&path, &lgdscan::avs::recording(&lgd, &video, call)) {
            Ok(()) => format!("{} に書きました", path.display()),
            Err(e) => format!("書けません: {e}"),
        };
    }

    /// The message for a saved .lgd, after writing its sample script.
    fn saved_lgd(&self, path: &Path) -> String {
        let msg = format!("{} に保存しました", path.display());
        if !self.write_sample {
            return msg;
        }
        match self.write_still_sample(path) {
            Ok(sample) => format!("{msg}。サンプルを {} に書きました", sample.display()),
            Err(e) => format!("{msg}。サンプルは書けません: {e}"),
        }
    }

    /// Writes the sample beside a .lgd, with the fades when measured.
    fn write_still_sample(&self, path: &Path) -> Result<PathBuf, String> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let sample = lgdscan::avs::path_for(path);
        let fades = self.found_spans().map(|f| f.3).unwrap_or_default();
        lgdscan::avs::write(&sample, &lgdscan::avs::still(&name, fades))?;
        Ok(sample)
    }

    fn save_still(&mut self) {
        let Some(still) = self.anim.as_ref().and_then(|a| a.still.clone()) else { return };
        let default = if self.name.is_empty() { "still.lgd".to_string() } else { format!("{}.lgd", self.name) };
        let Some(path) = rfd::FileDialog::new().add_filter("ロゴデータ", &["lgd"]).set_file_name(&default).save_file() else { return };
        let name = if self.name.is_empty() {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            self.name.clone()
        };
        let mut logo = still;
        logo.name = match lgd::encode_name(&name) {
            Ok(b) => lgd::stored_name(&b).to_vec(),
            Err(bad) => {
                self.status = format!("ロゴ名に CP932 で書けない文字があります: {bad}");
                return;
            }
        };
        let r = File::create(&path).and_then(|f| {
            let mut w = BufWriter::new(f);
            lgd::write(&mut w, &[logo])?;
            w.flush()
        });
        self.status = match r {
            Ok(()) => {
                self.saved = Some(path.clone());
                self.saved_lgd(&path)
            }
            Err(e) => format!("保存できません: {e}"),
        };
    }

    /// The result of a moving-logo analysis: the frames, where each
    /// recording starts, and saving.
    fn anim_result(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(a) = &mut self.anim else {
            ui.label(egui::RichText::new("まだありません").weak());
            return;
        };
        let n = a.logos.len();
        let mut changed = false;
        ui.horizontal(|ui| {
            if ui.small_button("◀").clicked() && a.k > 0 {
                a.k -= 1;
                changed = true;
            }
            if ui.small_button("▶").clicked() && a.k + 1 < n {
                a.k += 1;
                changed = true;
            }
            changed |= ui.add(egui::Slider::new(&mut a.k, 0..=n.saturating_sub(1)).text(format!("/ {} フレーム", n))).changed();
        });
        let k = a.k.min(n.saturating_sub(1));
        let logo = &a.logos[k];
        if a.tex.as_ref().is_none_or(|(t, _)| *t != k) {
            let rgb = erase::logo_to_rgb(logo, true);
            let img = egui::ColorImage::from_rgb([logo.w as usize, logo.h as usize], &rgb);
            a.tex = Some((k, ctx.load_texture("anim", img, TextureOptions::NEAREST)));
        }
        if let Some((_, tex)) = &a.tex {
            let scale = (ui.available_width() / logo.w as f32).min(140.0 / logo.h as f32).min(2.0);
            ui.add(egui::Image::new(tex).fit_to_exact_size(Vec2::new(logo.w as f32 * scale, logo.h as f32 * scale)));
        }
        let lr = Rect { x: logo.x as u32, y: logo.y as u32, w: logo.w as u32, h: logo.h as u32 };
        let peak = logo.pixels.iter().map(|p| p.dp_y).max().unwrap_or(0);
        let used = a.samples.get(k).copied().unwrap_or(0);
        ui.label(if used > 0 {
            format!("{}   不透明度の最大 {peak} / 1000   録画 {used} 本から", rect_text(lr))
        } else {
            format!("{}   不透明度の最大 {peak} / 1000", rect_text(lr))
        });
        if !a.starts.is_empty() {
            ui.add_space(4.0);
            ui.label("録画ごとの開始フレーム（選ぶとその録画で見られます）");
            egui::ScrollArea::vertical().id_salt("starts").max_height(140.0).show(ui, |ui| {
                for (i, (p, st)) in a.starts.iter().enumerate() {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.selectable_label(i == a.input, format!("{st:6}  {name}")).clicked() {
                        a.input = i;
                        changed = true;
                    }
                }
            });
            let i = a.input.min(a.starts.len() - 1);
            let (p, st) = &a.starts[i];
            let st = st + a.offsets.get(i).copied().unwrap_or(0);
            let file = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let ldp = if self.name.is_empty() { "anim.ldp".to_string() } else { format!("{}.ldp", self.name) };
            // Begun before the recording's first frame: from frame 0, with
            // the logos already played skipped (logo_start).
            let call = match (a.hold, st < 0) {
                (Some(h), false) => format!("EraseLogomod(logofile=\"{ldp}\", start={st}, end={st}+{}, fadeout={})", h.end, h.fadeout),
                (None, false) => format!("EraseLogomod(logofile=\"{ldp}\", start={st})"),
                (Some(h), true) => format!("EraseLogomod(logofile=\"{ldp}\", start=0, end={}, fadeout={}, logo_start={})", h.end + st, h.fadeout, -st),
                (None, true) => format!("EraseLogomod(logofile=\"{ldp}\", start=0, logo_start={})", -st),
            };
            // Wrapped on its own line: at the normal size it is wider than
            // the panel.
            ui.add(egui::Label::new(egui::RichText::new(&call).monospace()).wrap());
            if ui.button("コピー").on_hover_text(format!("{file} での呼び方")).clicked() {
                ctx.copy_text(call.clone());
            }
            if !self.anim_preview && ui.button("この録画のこのフレームを表示").clicked() {
                changed = true;
            }
        }
        if changed {
            self.anim_preview = !a.starts.is_empty();
            self.erased_rgb = None;
            self.frame_tex = None;
            if self.anim_preview {
                self.request_frame(&ctx);
            }
        }
        if ui.checkbox(&mut self.show_erased, "ロゴを消して表示").changed() {
            self.erased_rgb = None;
            self.frame_tex = None;
            if self.show_erased {
                self.request_frame(&ctx);
            }
        }
        ui.horizontal(|ui| {
            ui.label("ファイル名");
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            if ui.button("保存（.ldp）…").on_hover_text("delogomod の EraseLogomod が上から 1 フレームずつ使う形で書きます").clicked() {
                self.save_ldp();
            }
            let has_still = self.anim.as_ref().is_some_and(|a| a.still.is_some());
            if ui.add_enabled(has_still, egui::Button::new("局ロゴを保存（.lgd）…")).clicked() {
                self.save_still();
            }
        });
        ui.checkbox(&mut self.write_sample, "サンプルの .avs も書く")
            .on_hover_text("delogomod での使い方を、.ldp と同じ名前の .avs に書きます（end と fadeout は測れたときはその値）");
    }

    fn refresh_candidates(&mut self, pick_first: bool) {
        let Some(d) = &self.detection else { return };
        self.candidates = d.candidates(self.share, self.margin);
        if pick_first {
            match self.candidates.first() {
                Some(c) => {
                    self.rect = c.rect;
                    self.status = format!(
                        "キーフレーム {} 枚から、ロゴらしい場所が {} か所見つかりました。1 番目に枠を合わせました",
                        d.frames,
                        self.candidates.len()
                    );
                }
                None => self.status = "ロゴらしい場所が見つかりませんでした。割合を下げてみてください".into(),
            }
        }
        self.presence_tex = None;
    }

    fn start_detect(&mut self, ctx: &egui::Context) {
        let (Some(path), Some(info)) = (self.inputs.first().cloned(), self.info.clone()) else { return };
        let (start, end) = if self.range_on { (Some(self.range.0), Some(self.range.1)) } else { (None, None) };
        self.detect_task = Some(Task::spawn(ctx, move |cancel, report| {
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let opt = DetectOptions { samples: 120, start, end, threads };
            detect::measure(&path, &info, &opt, &|f| report(f as f32, format!("キーフレームを読んでいます {:.0}%", f * 100.0)), cancel)
        }));
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let mut job = Job::new(self.inputs.clone(), self.rect);
        if self.range_on {
            job.start = Some(self.range.0);
            job.end = Some(self.range.1);
        }
        job.step = self.step;
        job.threshold = self.threshold;
        job.background = self.background;
        job.scan = self.scan;
        self.scan_rect = self.rect;
        self.scan_task = Some(Task::spawn(ctx, move |cancel, report| {
            job::run(
                &job,
                &mut |p| {
                    let text = if p.fitting {
                        format!("{} フレーム中 {} フレームで当てはめています…", p.seen, p.accepted)
                    } else {
                        format!("{}/{} 本目  {} フレーム読み、{} フレームが使えます", p.input + 1, p.inputs, p.seen, p.accepted)
                    };
                    let f = (p.input as f64 + p.fraction) / p.inputs as f64;
                    report(f as f32, text);
                },
                cancel,
            )
        }));
    }

    /// Reads the first input again for where the station logo is on
    /// screen and how it fades.
    fn start_spans(&mut self, ctx: &egui::Context) {
        let (Some(path), Some(info), Some(logo)) = (self.inputs.first().cloned(), self.info.clone(), self.logo.clone()) else { return };
        let (start, duration) = if self.range_on { (Some(self.range.0), Some(self.range.1 - self.range.0)) } else { (None, None) };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as u32;
        let opt = ReadOptions { start, duration, step: 1, threads, scan: self.scan, on_the_clock: true };
        self.spans_task = Some(Task::spawn(ctx, move |cancel, report| {
            let (depths, offset) =
                spans::measure(&path, &info, &logo, &opt, &|f| report(f as f32, "ロゴの出ている区間を探しています…".into()), cancel)?;
            let (found, fades) = spans::find_with_fades(&depths, info.frame_rate);
            Ok((path, found, offset, fades))
        }));
    }

    fn save_lgd(&mut self) {
        let Some(logo) = &self.logo else { return };
        if let Err(bad) = lgd::encode_name(&self.name) {
            self.status = format!("ロゴ名に CP932 で書けない文字があります: {bad}");
            return;
        }
        let default = if self.name.is_empty() { "logo.lgd".to_string() } else { format!("{}.lgd", self.name) };
        let Some(path) = rfd::FileDialog::new().add_filter("ロゴデータ", &["lgd"]).set_file_name(&default).save_file() else { return };
        let mut logo = logo.clone();
        let name = if self.name.is_empty() {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            self.name.clone()
        };
        logo.name = match lgd::encode_name(&name) {
            Ok(b) => lgd::stored_name(&b).to_vec(),
            Err(bad) => {
                self.status = format!("ロゴ名に CP932 で書けない文字があります: {bad}");
                return;
            }
        };
        let r = File::create(&path).and_then(|f| {
            let mut w = BufWriter::new(f);
            lgd::write(&mut w, &[logo])?;
            w.flush()
        });
        self.status = match r {
            Ok(()) => self.saved_lgd(&path),
            Err(e) => format!("保存できません: {e}"),
        };
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("入力");
            ui.horizontal(|ui| {
                if ui.button("開く…").clicked() {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter("動画", &["ts", "m2ts", "mts", "mp4", "mkv", "mpg", "m2v"])
                        .add_filter("すべて", &["*"])
                        .pick_files()
                    {
                        self.set_inputs(&ctx, files);
                    }
                }
                if ui
                    .add_enabled(!self.inputs.is_empty(), egui::Button::new("追加…"))
                    .on_hover_text("同じ局の録画を足すと、背景の色に幅が出て結果が安定します")
                    .clicked()
                {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        for f in files {
                            self.open_path(&ctx, f);
                        }
                    }
                }
                if ui.button(".lgd / .ldp を開く…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("ロゴデータ", &["lgd", "ldp"]).pick_file() {
                        self.load_lgd(&ctx, &f);
                    }
                }
            });
            let mut remove = None;
            if self.inputs.len() > 1 {
                ui.label(egui::RichText::new(format!("{} 本", self.inputs.len())).small().weak());
            }
            // Many recordings (a moving logo wants dozens) must not push
            // everything else off the panel.
            egui::ScrollArea::vertical().id_salt("inputs").max_height(150.0).show(ui, |ui| {
                for (i, p) in self.inputs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if i > 0 && ui.small_button("×").clicked() {
                            remove = Some(i);
                        }
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        ui.add(egui::Label::new(name).truncate()).on_hover_text(p.display().to_string());
                    });
                }
            });
            if let Some(i) = remove {
                self.inputs.remove(i);
            }

            ui.separator();
            ui.heading("ロゴの位置");
            let (mw, mh) = self.info.as_ref().map_or((u32::MAX, u32::MAX), |i| (i.width, i.height));
            egui::Grid::new("rect").num_columns(4).show(ui, |ui| {
                ui.label("X");
                ui.add(egui::DragValue::new(&mut self.rect.x).range(0..=mw.saturating_sub(3)));
                ui.label("Y");
                ui.add(egui::DragValue::new(&mut self.rect.y).range(0..=mh.saturating_sub(3)));
                ui.end_row();
                ui.label("幅");
                ui.add(egui::DragValue::new(&mut self.rect.w).range(3..=mw));
                ui.label("高さ");
                ui.add(egui::DragValue::new(&mut self.rect.h).range(3..=mh));
                ui.end_row();
            });
            if self.rect.x + self.rect.w > mw {
                self.rect.w = mw - self.rect.x;
            }
            if self.rect.y + self.rect.h > mh {
                self.rect.h = mh - self.rect.y;
            }
            let hint = if self.mode == Mode::Still {
                "絵の上でドラッグして囲む・動かす・辺を伸ばす。矢印キーで 1 画素ずつ動かし、Shift+矢印で幅と高さを変えます。\
                 外周の 1 画素を背景として読むので、ロゴにかからないよう少し余白をとってください。"
            } else {
                "絵の上でドラッグして囲む・動かす・辺を伸ばす。矢印キーで 1 画素ずつ動かし、Shift+矢印で幅と高さを変えます。\
                 動くロゴでは、ロゴが動き回る範囲が全部入るように囲んでください（広すぎると時間とメモリを使います）。"
            };
            ui.label(egui::RichText::new(hint).small().weak());

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if let Some(t) = &self.detect_task {
                    let (f, text) = t.progress();
                    ui.add(egui::ProgressBar::new(f).text(text).desired_width(220.0));
                    if ui.button("中止").clicked() {
                        t.cancel.store(true, Ordering::Relaxed);
                    }
                } else if ui.add_enabled(self.info.is_some(), egui::Button::new("ロゴの位置を検出")).clicked() {
                    self.start_detect(&ctx);
                }
            });
            if self.detection.is_some() {
                let mut changed = false;
                changed |= ui
                    .add(egui::Slider::new(&mut self.share, 0.15..=0.95).text("縁の出る割合"))
                    .on_hover_text("この割合以上のフレームで縁が出ている画素をロゴとみなします")
                    .changed();
                changed |= ui.add(egui::Slider::new(&mut self.margin, 0..=24).text("余白（画素）")).changed();
                if changed {
                    self.refresh_candidates(false);
                }
                ui.checkbox(&mut self.show_presence, "検出結果を重ねて表示");
                let mut pick = None;
                for (i, c) in self.candidates.iter().enumerate().take(5) {
                    let text = format!("{}.  {}   （縁 {} 画素）", i + 1, rect_text(c.rect), c.pixels);
                    if ui.selectable_label(same_rect(c.rect, self.rect), text).clicked() {
                        pick = Some(c.rect);
                    }
                }
                if let Some(r) = pick {
                    self.rect = r;
                }
            }

            ui.separator();
            ui.heading("解析");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, Mode::Still, "局ロゴ");
                ui.selectable_value(&mut self.mode, Mode::Moving, "動くロゴ")
                    .on_hover_text("番組の頭でアニメーションしながら出てくるロゴ。1 フレームごとのロゴを .ldp に書きます");
            });
            if self.mode == Mode::Moving {
                ui.label(egui::RichText::new("動くロゴの解析のしかた").small().strong());
                for step in [
                    "1. 録画を開きます。1 本では解析できないので、同じ局の録画を何本も開いてください\
                     （30 本ほどあると安定します。十数本では、最後のほうのフレームがうまく求められません）。",
                    "2. 録画はそれぞれ、ロゴのアニメーションが始まる少し前から、ロゴが画面から消えるまでを切り出します（10 秒ほど）。",
                    "3. ロゴが動き回る範囲を、全部入るように枠で囲みます。",
                    "4. 「解析開始」を押します。",
                ] {
                    ui.label(egui::RichText::new(step).small());
                }
            }
            ui.checkbox(&mut self.range_on, "範囲を指定");
            if self.range_on {
                let dur = self.info.as_ref().map_or(0.0, |i| i.duration);
                ui.horizontal(|ui| {
                    ui.label("開始");
                    ui.add(egui::DragValue::new(&mut self.range.0).range(0.0..=dur).speed(1.0).custom_formatter(|v, _| fmt_time(v)));
                    if ui.small_button("現在位置").clicked() {
                        self.range.0 = self.time;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("終了");
                    ui.add(egui::DragValue::new(&mut self.range.1).range(0.0..=dur).speed(1.0).custom_formatter(|v, _| fmt_time(v)));
                    if ui.small_button("現在位置").clicked() {
                        self.range.1 = self.time;
                    }
                });
                if self.range.1 < self.range.0 {
                    std::mem::swap(&mut self.range.0, &mut self.range.1);
                }
            }
            let still = self.mode == Mode::Still;
            if !still {
                egui::Grid::new("params").num_columns(2).show(ui, |ui| {
                    ui.label("ずれの探索幅");
                    ui.add(egui::DragValue::new(&mut self.search).range(5..=600).suffix(" フレーム"))
                        .on_hover_text("録画どうしで、アニメーションの始まる位置がこれだけずれていても合わせます");
                    ui.end_row();
                });
            }
            if still {
                egui::Grid::new("params2").num_columns(2).show(ui, |ui| {
                ui.label("間引き");
                ui.add(egui::DragValue::new(&mut self.step).range(1..=30).suffix(" フレームに 1 枚"));
                ui.end_row();
                ui.label("閾値");
                ui.add(egui::DragValue::new(&mut self.threshold).range(1.0..=60.0).speed(0.2).suffix(" 階調"))
                    .on_hover_text("枠の画素が背景のモデルからどれだけ外れてよいか（8 ビットの階調）");
                ui.end_row();
                ui.label("背景");
                let label = |b: Background| match b {
                    Background::Plane => "勾配",
                    Background::Flat => "1 色（logoscan と同じ）",
                };
                egui::ComboBox::from_id_salt("bg").selected_text(label(self.background)).show_ui(ui, |ui| {
                    for b in [Background::Plane, Background::Flat] {
                        ui.selectable_value(&mut self.background, b, label(b));
                    }
                });
                ui.end_row();
            });
            }
            egui::Grid::new("params3").num_columns(2).show(ui, |ui| {
                ui.label("色差の補間");
                let label = |s: Scan| match s {
                    Scan::Auto => "自動",
                    Scan::Progressive => "プログレッシブ",
                    Scan::Interlaced => "インターレース",
                };
                egui::ComboBox::from_id_salt("scan").selected_text(label(self.scan)).show_ui(ui, |ui| {
                    for s in [Scan::Auto, Scan::Progressive, Scan::Interlaced] {
                        ui.selectable_value(&mut self.scan, s, label(s));
                    }
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            if let Some(t) = &self.anim_task {
                let (f, text) = t.progress();
                ui.add(egui::ProgressBar::new(f).show_percentage());
                ui.label(text);
                if ui.button("中止").clicked() {
                    t.cancel.store(true, Ordering::Relaxed);
                }
            } else if !still {
                if ui
                    .add_enabled(self.info.is_some() && self.scan_task.is_none(), egui::Button::new("解析開始").min_size(Vec2::new(120.0, 28.0)))
                    .clicked()
                {
                    self.start_anim(&ctx);
                }
            } else if let Some(t) = &self.scan_task {
                let (f, text) = t.progress();
                ui.add(egui::ProgressBar::new(f).show_percentage());
                ui.label(text);
                if ui.button("中止").clicked() {
                    t.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui
                .add_enabled(self.info.is_some() && self.anim_task.is_none() && self.spans_task.is_none(), egui::Button::new("解析開始").min_size(Vec2::new(120.0, 28.0)))
                .clicked()
            {
                self.start_scan(&ctx);
            }

            ui.separator();
            ui.heading("結果");
            if !still {
                self.anim_result(ui);
            } else if let (Some(logo), Some(tex)) = (&self.logo, &self.logo_tex) {
                // Two screen points per pixel at most, and no taller than 120.
                let scale = (ui.available_width() / logo.w as f32).min(120.0 / logo.h as f32).min(2.0);
                let (w, h) = (logo.w as f32 * scale, logo.h as f32 * scale);
                ui.add(egui::Image::new(tex).fit_to_exact_size(Vec2::new(w, h)));
                let lr = Rect { x: logo.x as u32, y: logo.y as u32, w: logo.w as u32, h: logo.h as u32 };
                let peak = logo.pixels.iter().map(|p| p.dp_y).max().unwrap_or(0);
                ui.label(format!("{}   不透明度の最大 {peak} / 1000", rect_text(lr)));
                if let Some(o) = &self.outcome {
                    ui.label(format!(
                        "{} フレームを読み、{} フレームで当てはめ（{} フレームはロゴ無しとして除外）",
                        o.seen, o.frames_used, o.frames_without_logo
                    ));
                }
                if ui.checkbox(&mut self.show_erased, "ロゴを消して表示").changed() {
                    self.erased_rgb = None;
                    self.frame_tex = None;
                    if self.show_erased {
                        self.request_frame(&ctx);
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("ロゴ名");
                    ui.text_edit_singleline(&mut self.name);
                });
                match lgd::encode_name(&self.name) {
                    Err(bad) => {
                        ui.colored_label(Color32::from_rgb(255, 120, 100), format!("CP932 で書けない文字: {bad}"));
                    }
                    Ok(b) if b.len() > lgd::NAME_MAX_V1 => {
                        let kept = lgd::decode_name(lgd::stored_name(&b));
                        ui.colored_label(Color32::from_rgb(255, 200, 90), format!("{} バイトを超えるので「{kept}」まで書きます", lgd::NAME_MAX_V1));
                    }
                    Ok(b) => {
                        ui.label(egui::RichText::new(format!("CP932 で {} / {} バイト", b.len(), lgd::NAME_MAX_V1)).small().weak());
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("保存…").clicked() {
                        self.save_lgd();
                    }
                    ui.checkbox(&mut self.write_sample, "サンプルの .avs も書く")
                        .on_hover_text("delogo での使い方を、.lgd と同じ名前の .avs に書きます");
                });
                if let Some((_, spans, _, fades)) = self.found_spans() {
                    let fade = |what: &str, v: Option<u64>| match v {
                        Some(0) => format!("{what}なし"),
                        Some(f) => format!("{what} {f} フレーム"),
                        None => format!("{what}は測れず"),
                    };
                    ui.label(format!("ロゴの出ている区間 {} か所", spans.len()));
                    ui.label(format!("{}・{}", fade("フェードイン", fades.fadein), fade("フェードアウト", fades.fadeout)));
                }
                if let Some(call) = self.still_call() {
                    ui.add(egui::Label::new(egui::RichText::new(&call).monospace()).wrap());
                    let hover = if self.found_spans().is_some() {
                        "ロゴの出ている区間ごとの呼び方"
                    } else if self.range_on {
                        "指定した範囲での呼び方"
                    } else {
                        "録画全体での呼び方（CM の間は外してください）"
                    };
                    ui.horizontal(|ui| {
                        if ui.button("コピー").on_hover_text(hover).clicked() {
                            ctx.copy_text(call.clone());
                        }
                        if ui.button("この録画用の .avs を保存…").on_hover_text("この呼び出しを、読み込んだ録画専用のスクリプトとして書きます").clicked() {
                            self.save_recording_avs(&call);
                        }
                    });
                    ui.horizontal(|ui| {
                        if let Some(t) = &self.spans_task {
                            let (f, _) = t.progress();
                            ui.add(egui::ProgressBar::new(f).show_percentage().desired_width(120.0));
                            if ui.button("中止").clicked() {
                                t.cancel.store(true, Ordering::Relaxed);
                            }
                        } else if ui
                            .add_enabled(self.scan_task.is_none(), egui::Button::new("ロゴの出る区間とフェードを測る"))
                            .on_hover_text("録画を読み直して、ロゴの出ている区間（CM の間を除く）と、フェードする局ではフェードの長さを測ります")
                            .clicked()
                        {
                            self.start_spans(&ctx);
                        }
                    });
                }
            } else {
                ui.label(egui::RichText::new("まだありません").weak());
            }
        });
    }

    /// The input the moving-logo preview shows, probed once.
    fn preview_info(&mut self) -> Option<VideoInfo> {
        let a = self.anim.as_mut()?;
        let i = a.input;
        if let Some(v) = a.infos.get(&i) {
            return v.clone();
        }
        // A failure is kept too: this runs on every repaint.
        let info = a.starts.get(i).and_then(|(p, _)| source::probe(p).ok());
        a.infos.insert(i, info.clone());
        info
    }

    fn leave_anim_preview(&mut self) {
        if self.anim_preview {
            self.anim_preview = false;
            self.erased_rgb = None;
            self.frame_tex = None;
        }
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(info) = &self.info else {
            ui.label(&self.status);
            return;
        };
        let (dur, fps) = (info.duration.max(0.0), if info.frame_rate > 0.0 { info.frame_rate } else { 29.97 });
        ui.horizontal(|ui| {
            let mut jump = None;
            for (label, d) in [("−10秒", -10.0), ("−1秒", -1.0), ("◀", -1.0 / fps), ("▶", 1.0 / fps), ("+1秒", 1.0), ("+10秒", 10.0)] {
                if ui.button(label).clicked() {
                    jump = Some(d);
                }
            }
            ui.label(format!("{} / {}", fmt_time(self.time), fmt_time(dur)));
            if ui.button("全体表示").on_hover_text("ホイールで拡大・縮小、右ボタンか中ボタンのドラッグで移動、右ダブルクリックで全体表示").clicked() {
                self.view.fit = true;
            }
            if self.grab.is_some() {
                ui.spinner();
            }
            ui.spacing_mut().slider_width = (ui.available_width() - 16.0).max(100.0);
            let r = ui.add(egui::Slider::new(&mut self.time, 0.0..=dur).show_value(false));
            if let Some(d) = jump {
                self.time = (self.time + d).clamp(0.0, dur);
                self.leave_anim_preview();
                self.request_frame(&ctx);
            } else if r.drag_stopped() || (r.changed() && !r.dragged()) {
                self.leave_anim_preview();
                self.request_frame(&ctx);
            }
        });
        ui.label(&self.status);
    }

    fn picture(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(area, Sense::click_and_drag());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, Color32::from_gray(24));
        let preview_info = if self.anim_preview { self.preview_info() } else { None };
        let Some(info) = preview_info.or_else(|| self.info.clone()) else {
            painter.text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                "録画ファイルをここに落としてください",
                egui::FontId::proportional(20.0),
                Color32::GRAY,
            );
            return;
        };
        let (iw, ih) = (info.width as f32, info.height as f32);
        if self.view.fit {
            self.view.scale = (area.width() / iw).min(area.height() / ih);
            self.view.center = Vec2::new(iw / 2.0, ih / 2.0);
        }
        // Zoom around the pointer.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                if let Some(m) = ui.input(|i| i.pointer.hover_pos()) {
                    let before = self.view.to_image(area, m);
                    self.view.scale = (self.view.scale * (scroll / 600.0).exp()).clamp(0.05, 40.0);
                    let after = self.view.to_image(area, m);
                    self.view.center += before - after;
                    self.view.fit = false;
                }
            }
        }
        if resp.dragged_by(PointerButton::Secondary) || resp.dragged_by(PointerButton::Middle) {
            self.view.center -= resp.drag_delta() / self.view.scale;
            self.view.fit = false;
        }
        if resp.double_clicked_by(PointerButton::Secondary) || resp.double_clicked_by(PointerButton::Middle) {
            self.view.fit = true;
        }

        // The picture.
        let nearest = self.view.scale >= 2.0;
        let want_erased = self.show_erased && self.erased_rgb.is_some();
        if self.frame_tex.is_none() || self.tex_nearest != nearest || self.tex_erased != want_erased {
            let src = if want_erased { self.erased_rgb.as_ref() } else { self.frame_rgb.as_ref() };
            // A frame of another size is from the input shown before; the
            // one asked for since is on its way.
            if let Some(rgb) = src.filter(|_| self.frame_size == (info.width, info.height)) {
                let img = egui::ColorImage::from_rgb([info.width as usize, info.height as usize], rgb);
                let opt = if nearest { TextureOptions::NEAREST } else { TextureOptions::LINEAR };
                self.frame_tex = Some(ctx.load_texture("frame", img, opt));
                self.tex_nearest = nearest;
                self.tex_erased = want_erased;
            }
        }
        let whole = self.view.rect_to_screen(area, Rect { x: 0, y: 0, w: info.width, h: info.height });
        let uv = egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        if let Some(tex) = &self.frame_tex {
            painter.image(tex.id(), whole, uv, Color32::WHITE);
        }
        // Edge presence from the detection.
        if self.show_presence {
            if let Some(d) = &self.detection {
                if self.presence_tex.as_ref().is_none_or(|(s, _)| *s != self.share) {
                    let mask = d.mask(self.share);
                    let rgba: Vec<u8> = mask
                        .iter()
                        .zip(&d.presence)
                        .flat_map(|(m, p)| if *m { [0, 255, 255, (40.0 + 100.0 * p) as u8] } else { [0, 0, 0, 0] })
                        .collect();
                    let img = egui::ColorImage::from_rgba_unmultiplied([d.width as usize, d.height as usize], &rgba);
                    self.presence_tex = Some((self.share, ctx.load_texture("presence", img, TextureOptions::NEAREST)));
                }
                if let Some((_, tex)) = &self.presence_tex {
                    painter.image(tex.id(), whole, uv, Color32::WHITE);
                }
            }
        }
        let cyan = Color32::from_rgb(0, 200, 255);
        for (i, c) in self.candidates.iter().enumerate().take(5).filter(|_| self.show_presence) {
            let r = self.view.rect_to_screen(area, c.rect);
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, cyan), egui::StrokeKind::Outside);
            painter.text(r.left_bottom() + Vec2::new(0.0, 2.0), egui::Align2::LEFT_TOP, format!("{}", i + 1), egui::FontId::proportional(13.0), cyan);
        }

        // The rectangle and its handles.
        let sr = self.view.rect_to_screen(area, self.rect);
        let near = |p: Pos2| -> (bool, bool, bool, bool, bool) {
            let tol = 6.0;
            let inside_y = p.y >= sr.top() - tol && p.y <= sr.bottom() + tol;
            let inside_x = p.x >= sr.left() - tol && p.x <= sr.right() + tol;
            let l = inside_y && (p.x - sr.left()).abs() <= tol;
            let r = inside_y && (p.x - sr.right()).abs() <= tol;
            let t = inside_x && (p.y - sr.top()).abs() <= tol;
            let b = inside_x && (p.y - sr.bottom()).abs() <= tol;
            (l, t, r, b, sr.contains(p))
        };
        if let Some(p) = ui.input(|i| i.pointer.hover_pos()).filter(|_| resp.hovered() && self.drag.is_none()) {
            let (l, t, r, b, inside) = near(p);
            let icon = match (l || r, t || b) {
                (true, true) if (l && t) || (r && b) => CursorIcon::ResizeNwSe,
                (true, true) => CursorIcon::ResizeNeSw,
                (true, false) => CursorIcon::ResizeHorizontal,
                (false, true) => CursorIcon::ResizeVertical,
                _ if inside => CursorIcon::Move,
                _ => CursorIcon::Crosshair,
            };
            ctx.set_cursor_icon(icon);
        }
        if resp.drag_started_by(PointerButton::Primary) {
            // Judge by where the button went down, not where the drag was
            // recognised a few pixels later.
            if let Some(p) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let (l, t, r, b, inside) = near(p);
                let ip = self.view.to_image(area, p);
                self.drag = Some(if l || t || r || b {
                    Drag::Resize { left: l, top: t, right: r, bottom: b, start: self.rect }
                } else if inside {
                    Drag::Move { from: ip, start: self.rect }
                } else {
                    Drag::New { anchor: ip }
                });
            }
        }
        if let (Some(d), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
            if resp.dragged_by(PointerButton::Primary) {
                let ip = self.view.to_image(area, p);
                let cx = |v: f32| v.round().clamp(0.0, iw) as i64;
                let cy = |v: f32| v.round().clamp(0.0, ih) as i64;
                let (x0, y0, x1, y1) = match d {
                    Drag::New { anchor } => (cx(anchor.x.min(ip.x)), cy(anchor.y.min(ip.y)), cx(anchor.x.max(ip.x)), cy(anchor.y.max(ip.y))),
                    Drag::Move { from, start } => {
                        let dx = (ip.x - from.x).round() as i64;
                        let dy = (ip.y - from.y).round() as i64;
                        let x0 = (start.x as i64 + dx).clamp(0, (info.width as i64 - start.w as i64).max(0));
                        let y0 = (start.y as i64 + dy).clamp(0, (info.height as i64 - start.h as i64).max(0));
                        (x0, y0, x0 + start.w as i64, y0 + start.h as i64)
                    }
                    Drag::Resize { left, top, right, bottom, start } => {
                        let (mut x0, mut y0) = (start.x as i64, start.y as i64);
                        let (mut x1, mut y1) = ((start.x + start.w) as i64, (start.y + start.h) as i64);
                        if left {
                            x0 = cx(ip.x).min(x1 - 3);
                        }
                        if right {
                            x1 = cx(ip.x).max(x0 + 3);
                        }
                        if top {
                            y0 = cy(ip.y).min(y1 - 3);
                        }
                        if bottom {
                            y1 = cy(ip.y).max(y0 + 3);
                        }
                        (x0, y0, x1, y1)
                    }
                };
                if x1 - x0 >= 3 && y1 - y0 >= 3 {
                    self.rect = Rect { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 };
                }
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        // Arrow keys nudge the rectangle; with Shift they change its size.
        if !ctx.egui_wants_keyboard_input() {
            let (shift, l, r, u, d) = ui.input(|i| {
                (
                    i.modifiers.shift,
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::ArrowDown),
                )
            });
            let dx = r as i64 - l as i64;
            let dy = d as i64 - u as i64;
            if dx != 0 || dy != 0 {
                let mut q = self.rect;
                if shift {
                    // The box may be larger than a preview of another size.
                    q.w = (q.w as i64 + dx).clamp(3, (info.width as i64 - q.x as i64).max(3)) as u32;
                    q.h = (q.h as i64 + dy).clamp(3, (info.height as i64 - q.y as i64).max(3)) as u32;
                } else {
                    q.x = (q.x as i64 + dx).clamp(0, (info.width as i64 - q.w as i64).max(0)) as u32;
                    q.y = (q.y as i64 + dy).clamp(0, (info.height as i64 - q.h as i64).max(0)) as u32;
                }
                self.rect = q;
            }
        }
        let yellow = Color32::from_rgb(255, 220, 0);
        let sr = self.view.rect_to_screen(area, self.rect);
        painter.rect_stroke(sr, 0.0, Stroke::new(1.0, Color32::BLACK), egui::StrokeKind::Outside);
        painter.rect_stroke(sr.expand(1.0), 0.0, Stroke::new(1.0, yellow), egui::StrokeKind::Outside);
        for c in [sr.left_top(), sr.right_top(), sr.left_bottom(), sr.right_bottom()] {
            painter.rect_filled(egui::Rect::from_center_size(c, Vec2::splat(6.0)), 0.0, yellow);
        }
        painter.text(sr.left_top() - Vec2::new(0.0, 3.0), egui::Align2::LEFT_BOTTOM, rect_text(self.rect), egui::FontId::proportional(13.0), yellow);
        if self.anim_preview {
            if let Some(a) = &self.anim {
                let l = &a.logos[a.k.min(a.logos.len() - 1)];
                let lr = self.view.rect_to_screen(area, Rect { x: l.x.max(0) as u32, y: l.y.max(0) as u32, w: l.w as u32, h: l.h as u32 });
                painter.rect_stroke(lr, 0.0, Stroke::new(1.0, Color32::from_rgb(255, 120, 60)), egui::StrokeKind::Outside);
                let text = match self.shown_frame {
                    Some((i, n)) => {
                        let name = a.starts.get(i).and_then(|(p, _)| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        format!("アニメーションの {} フレーム目（{name} の {n} フレーム目）", a.k)
                    }
                    None => "読み込んでいます…".into(),
                };
                painter.text(area.left_top() + Vec2::new(8.0, 8.0), egui::Align2::LEFT_TOP, text, egui::FontId::proportional(13.0), Color32::from_rgb(255, 160, 100));
            }
        } else if let Some(t) = self.shown_time.filter(|t| (t - self.time).abs() > 1e-6) {
            painter.text(
                area.left_top() + Vec2::new(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                format!("表示中 {}", fmt_time(t)),
                egui::FontId::proportional(13.0),
                Color32::GRAY,
            );
        }
    }
}

/// `rgb` with the logo removed. The change that removal makes is computed in
/// PIXEL_YC and added to ffmpeg's RGB, so no seam shows where the two colour
/// conversions differ.
fn erased_picture(path: &Path, info: &VideoInfo, logo: &Logo, at: f64, rgb: &[u8]) -> Result<Vec<u8>, String> {
    if logo.x < 0 || logo.y < 0 || logo.x as u32 + logo.w as u32 > info.width || logo.y as u32 + logo.h as u32 > info.height {
        return Err("ロゴが絵の外にあります".into());
    }
    let rect = Rect { x: logo.x as u32, y: logo.y as u32, w: logo.w as u32, h: logo.h as u32 };
    let opt = ReadOptions { start: Some(at), duration: None, step: 1, threads: 2, scan: Scan::Auto, on_the_clock: false };
    let frame = Reader::open(path, info, rect, &opt)
        .map_err(|e| e.to_string())?
        .next_frame()
        .map_err(|e| e.to_string())?
        .ok_or("フレームがありません")?;
    Ok(apply_erase(info, logo, rect, frame, rgb))
}

/// `rgb` with `logo` removed from `frame`, which covers `rect`.
fn apply_erase(info: &VideoInfo, logo: &Logo, rect: Rect, frame: source::Frame, rgb: &[u8]) -> Vec<u8> {
    let hd = source::is_hd(info);
    let before = erase::frame_to_rgb(&frame, hd);
    let mut frame = frame;
    erase::remove(logo, &mut frame, rect);
    let after = erase::frame_to_rgb(&frame, hd);
    let mut out = rgb.to_vec();
    for r in 0..rect.h as usize {
        for c in 0..rect.w as usize {
            let i = (r * rect.w as usize + c) * 3;
            let o = ((rect.y as usize + r) * info.width as usize + rect.x as usize + c) * 3;
            for k in 0..3 {
                let v = out[o + k] as i32 + after[i + k] as i32 - before[i + k] as i32;
                out[o + k] = v.clamp(0, 255) as u8;
            }
        }
    }
    out
}

/// Like [`erased_picture`], for frame `n` counted from `start`.
fn erased_frame(path: &Path, info: &VideoInfo, logo: &Logo, start: Option<f64>, n: u64, rgb: &[u8]) -> Result<Vec<u8>, String> {
    if logo.x < 0 || logo.y < 0 || logo.x as u32 + logo.w as u32 > info.width || logo.y as u32 + logo.h as u32 > info.height {
        return Err("ロゴが絵の外にあります".into());
    }
    let rect = Rect { x: logo.x as u32, y: logo.y as u32, w: logo.w as u32, h: logo.h as u32 };
    let opt = ReadOptions { start, duration: None, step: 1, threads: 2, scan: Scan::Auto, on_the_clock: false };
    let mut reader = Reader::open(path, info, rect, &opt).map_err(|e| e.to_string())?;
    let mut frame = None;
    for _ in 0..=n {
        frame = reader.next_frame().map_err(|e| e.to_string())?;
        if frame.is_none() {
            break;
        }
    }
    let frame = frame.ok_or("フレームがありません")?;
    Ok(apply_erase(info, logo, rect, frame, rgb))
}

fn is_logo_file(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lgd") || e.eq_ignore_ascii_case("ldp"))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_tasks(&ctx);
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            let (lgds, videos): (Vec<PathBuf>, Vec<PathBuf>) =
                dropped.into_iter().partition(|p| is_logo_file(p));
            if !videos.is_empty() {
                // Shift while dropping adds to the inputs instead of replacing them.
                if self.inputs.is_empty() || !ctx.input(|i| i.modifiers.shift) {
                    self.set_inputs(&ctx, videos);
                } else {
                    for v in videos {
                        self.open_path(&ctx, v);
                    }
                }
            }
            for l in lgds {
                self.load_lgd(&ctx, &l);
            }
        }
        egui::Panel::right("side").default_size(380.0).min_size(300.0).show(ui, |ui| self.side_panel(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_panel(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| self.picture(ui));
        if self.grab.is_some() || self.scan_task.is_some() || self.spans_task.is_some() || self.detect_task.is_some() || self.anim_task.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}
