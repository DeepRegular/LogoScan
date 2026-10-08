//! One logo analysis from start to finish, shared by the CLI and the GUI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::lgd::{self, LogoPixel};
use crate::scan::{Background, Params, Scanner};
use crate::source::{self, ReadOptions, Reader, Rect, Scan};

#[derive(Clone, Debug)]
pub struct Job {
    pub inputs: Vec<PathBuf>,
    pub rect: Rect,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub step: u32,
    /// Largest spread on the background ring, in 8-bit levels.
    pub threshold: f64,
    pub background: Background,
    pub scan: Scan,
    pub max_frames: usize,
    pub passes: u32,
    pub threads: usize,
}

impl Job {
    pub fn new(inputs: Vec<PathBuf>, rect: Rect) -> Job {
        Job {
            inputs,
            rect,
            start: None,
            end: None,
            step: 1,
            threshold: 12.0,
            background: Background::Plane,
            scan: Scan::Auto,
            max_frames: 8000,
            passes: 3,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Index of the input being read.
    pub input: usize,
    pub inputs: usize,
    pub seen: u64,
    pub accepted: u64,
    /// 0..1 through the current input, when its length is known.
    pub fraction: f64,
    /// True while fitting, after every frame has been read.
    pub fitting: bool,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub pixels: Vec<LogoPixel>,
    pub seen: u64,
    pub accepted: u64,
    pub frames_used: usize,
    pub frames_without_logo: usize,
    /// Largest dp_y; small means no logo was found.
    pub peak: i16,
}

impl Outcome {
    pub fn logo(&self, name: Vec<u8>, rect: Rect) -> lgd::Logo {
        lgd::Logo {
            name,
            x: rect.x as i16,
            y: rect.y as i16,
            w: rect.w as i16,
            h: rect.h as i16,
            pixels: self.pixels.clone(),
            ..Default::default()
        }
    }
}

pub fn run(job: &Job, progress: &mut dyn FnMut(&Progress), cancel: &AtomicBool) -> Result<Outcome, String> {
    let r = job.rect;
    if r.w < 3 || r.h < 3 {
        return Err("矩形が小さすぎます（3×3 以上）".into());
    }
    if r.w > i16::MAX as u32 || r.h > i16::MAX as u32 || r.x > i16::MAX as u32 || r.y > i16::MAX as u32 {
        return Err("矩形が .lgd に収まりません".into());
    }
    if job.inputs.is_empty() {
        return Err("入力がありません".into());
    }
    // The threshold is given in 8-bit levels; convert to PIXEL_YC units.
    let params = Params {
        background: job.background,
        threshold_y: job.threshold * 4096.0 / 219.0,
        threshold_c: job.threshold * 4096.0 / 224.0,
        max_frames: job.max_frames,
        robust_passes: job.passes,
    };
    let mut scanner = Scanner::new(r.w as usize, r.h as usize, params);
    let mut p = Progress { inputs: job.inputs.len(), ..Default::default() };
    for (k, input) in job.inputs.iter().enumerate() {
        let info = source::probe(input).map_err(|e| e.to_string())?;
        let opt = ReadOptions {
            start: job.start,
            duration: job.end.map(|e| e - job.start.unwrap_or(0.0)),
            step: job.step,
            threads: job.threads.min(8) as u32,
            scan: job.scan,
            on_the_clock: false,
        };
        let span = match job.end {
            Some(e) => e - job.start.unwrap_or(0.0),
            None => info.duration - job.start.unwrap_or(0.0),
        };
        let mut reader = Reader::open(input, &info, r, &opt).map_err(|e| format!("{}: {e}", input.display()))?;
        p.input = k;
        let mut read = 0u64;
        while let Some(frame) = reader.next_frame().map_err(|e| e.to_string())? {
            if cancel.load(Ordering::Relaxed) {
                return Err("中止しました".into());
            }
            scanner.push(frame);
            read += 1;
            if read.is_multiple_of(200) {
                p.seen = scanner.seen;
                p.accepted = scanner.accepted;
                p.fraction = if span > 0.0 && info.frame_rate > 0.0 {
                    (read as f64 * job.step as f64 / info.frame_rate / span).min(1.0)
                } else {
                    0.0
                };
                progress(&p);
            }
        }
    }
    p.seen = scanner.seen;
    p.accepted = scanner.accepted;
    p.fraction = 1.0;
    p.fitting = true;
    progress(&p);
    let report = scanner.finish(job.threads);
    if report.frames_used < 2 {
        return Err("使えるフレームがありませんでした。背景の枠が一度も平らになりません（閾値を上げてください）".into());
    }
    let peak = report.pixels.iter().map(|p| p.dp_y).max().unwrap_or(0);
    Ok(Outcome {
        pixels: report.pixels,
        seen: scanner.seen,
        accepted: scanner.accepted,
        frames_used: report.frames_used,
        frames_without_logo: report.frames_without_logo,
        peak,
    })
}
