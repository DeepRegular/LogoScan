//! Finds where the logo is.
//!
//! The pictures change, the logo does not: an edge that shows up at the same
//! pixel in most of a hundred frames spread over the recording belongs to an
//! overlay. Keyframes are sampled across the input, the share of frames with
//! an edge is kept per pixel, and the pixels above a share are grouped into
//! candidates (letters close together form one logo).

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::source::{Rect, VideoInfo};

/// Sobel magnitude (|gx| + |gy| on 8-bit luma) counted as an edge.
const EDGE: i32 = 32;

#[derive(Clone, Debug)]
pub struct Candidate {
    /// The rectangle to analyse: `bbox` plus the margin.
    pub rect: Rect,
    /// Tight box around the edge pixels.
    pub bbox: Rect,
    pub pixels: u32,
    pub score: f64,
}

#[derive(Clone, Debug)]
pub struct Detection {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    /// Per pixel, the share of frames with an edge there (0..1).
    pub presence: Vec<f32>,
}

pub struct DetectOptions {
    pub samples: u32,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub threads: usize,
}

/// Samples `opt.samples` keyframes and measures edge presence.
pub fn measure(
    path: &Path,
    info: &VideoInfo,
    opt: &DetectOptions,
    progress: &(dyn Fn(f64) + Sync),
    cancel: &AtomicBool,
) -> Result<Detection, String> {
    let (w, h) = (info.width as usize, info.height as usize);
    let t0 = opt.start.unwrap_or(0.0).max(0.0);
    let t1 = opt.end.unwrap_or(info.duration).min(if info.duration > 0.0 { info.duration } else { f64::MAX });
    if t1.partial_cmp(&t0) != Some(std::cmp::Ordering::Greater) {
        return Err(if opt.end.is_none() && info.duration <= 0.0 { "録画の長さがわかりません" } else { "読む範囲が録画の中にありません" }.into());
    }
    let n = opt.samples.max(4) as usize;
    let times: Vec<f64> = (0..n).map(|i| t0 + (t1 - t0) * (i as f64 + 0.5) / n as f64).collect();
    let counts = Mutex::new(vec![0u32; w * h]);
    let frames = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..opt.threads.clamp(1, 6) {
            s.spawn(|| {
                let mut local = vec![0u32; w * h];
                let mut got = 0;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= times.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(luma) = grab_key_luma(path, w, h, times[i]) {
                        edges(&luma, w, h, &mut local);
                        got += 1;
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress(d as f64 / times.len() as f64);
                }
                let mut c = counts.lock().unwrap();
                for (a, b) in c.iter_mut().zip(&local) {
                    *a += b;
                }
                frames.fetch_add(got, Ordering::Relaxed);
            });
        }
    });
    if cancel.load(Ordering::Relaxed) {
        return Err("中止しました".into());
    }
    let frames = frames.into_inner() as u32;
    if frames < 4 {
        return Err("フレームをほとんど読めませんでした".into());
    }
    let presence = counts.into_inner().unwrap().iter().map(|&c| c as f32 / frames as f32).collect();
    Ok(Detection { width: info.width, height: info.height, frames, presence })
}

/// The first keyframe at or after `at`, luma only.
fn grab_key_luma(path: &Path, w: usize, h: usize, at: f64) -> Option<Vec<u8>> {
    let out = crate::source::tool("ffmpeg")
        .args(["-hide_banner", "-loglevel", "quiet", "-nostdin", "-threads", "2"])
        .args(["-skip_frame", "nokey", "-noaccurate_seek", "-ss", &format!("{at:.3}")])
        .arg("-i")
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-sn", "-dn", "-vf", "format=gray", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (out.stdout.len() >= w * h).then(|| out.stdout[..w * h].to_vec())
}

pub(crate) fn edges(luma: &[u8], w: usize, h: usize, count: &mut [u32]) {
    let p = |x: usize, y: usize| luma[y * w + x] as i32;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let gx = p(x + 1, y - 1) + 2 * p(x + 1, y) + p(x + 1, y + 1) - p(x - 1, y - 1) - 2 * p(x - 1, y) - p(x - 1, y + 1);
            let gy = p(x - 1, y + 1) + 2 * p(x, y + 1) + p(x + 1, y + 1) - p(x - 1, y - 1) - 2 * p(x, y - 1) - p(x + 1, y - 1);
            if gx.abs() + gy.abs() >= EDGE {
                count[y * w + x] += 1;
            }
        }
    }
}

impl Detection {
    /// Pixels whose presence reaches `share`, without isolated specks and
    /// away from the picture's border.
    pub fn mask(&self, share: f32) -> Vec<bool> {
        let (w, h) = (self.width as usize, self.height as usize);
        let edge = (h / 135).max(4);
        let raw: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                x >= edge && y >= edge && x + edge < w && y + edge < h && self.presence[i] >= share
            })
            .collect();
        let mut out = raw.clone();
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                if !raw[y * w + x] {
                    continue;
                }
                let mut n = 0;
                for dy in 0..3 {
                    for dx in 0..3 {
                        n += raw[(y + dy - 1) * w + x + dx - 1] as u32;
                    }
                }
                // Itself plus at least two neighbours.
                out[y * w + x] = n >= 3;
            }
        }
        out
    }

    /// Groups the mask into candidates, strongest first.
    pub fn candidates(&self, share: f32, margin: u32) -> Vec<Candidate> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mask = self.mask(share);
        // Close the gaps between letters: dilate by a radius that scales with
        // the picture (8 pixels at 1080 lines).
        let r = (h / 135).max(3);
        let grown = dilate(&mask, w, h, r);
        let mut label = vec![u32::MAX; w * h];
        let mut out = Vec::new();
        let mut stack = Vec::new();
        for start in 0..w * h {
            if !grown[start] || label[start] != u32::MAX {
                continue;
            }
            let id = out.len() as u32;
            label[start] = id;
            stack.push(start);
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
            let (mut pixels, mut score) = (0u32, 0.0f64);
            while let Some(i) = stack.pop() {
                let (x, y) = (i % w, i / w);
                if mask[i] {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                    pixels += 1;
                    score += self.presence[i] as f64;
                }
                let mut visit = |j: usize| {
                    if grown[j] && label[j] == u32::MAX {
                        label[j] = id;
                        stack.push(j);
                    }
                };
                if x > 0 {
                    visit(i - 1);
                }
                if x + 1 < w {
                    visit(i + 1);
                }
                if y > 0 {
                    visit(i - w);
                }
                if y + 1 < h {
                    visit(i + w);
                }
            }
            out.push((x0, y0, x1, y1, pixels, score));
        }
        let mut cands: Vec<Candidate> = out
            .into_iter()
            .filter(|&(x0, y0, x1, y1, pixels, _)| {
                let (bw, bh) = (x1 + 1 - x0, y1 + 1 - y0);
                // Letterbox and pillarbox borders are static too, but long.
                pixels >= 20 && bw * 2 < w && bh * 2 < h && bw >= 3 && bh >= 3
            })
            .map(|(x0, y0, x1, y1, pixels, score)| {
                let bbox = Rect { x: x0 as u32, y: y0 as u32, w: (x1 + 1 - x0) as u32, h: (y1 + 1 - y0) as u32 };
                Candidate { rect: grow(bbox, margin, self.width, self.height), bbox, pixels, score }
            })
            .collect();
        cands.sort_by(|a, b| b.score.total_cmp(&a.score));
        cands.truncate(10);
        cands
    }
}

pub fn grow(r: Rect, margin: u32, width: u32, height: u32) -> Rect {
    let x = r.x.saturating_sub(margin);
    let y = r.y.saturating_sub(margin);
    let x1 = (r.x + r.w + margin).min(width);
    let y1 = (r.y + r.h + margin).min(height);
    Rect { x, y, w: x1 - x, h: y1 - y }
}

pub(crate) fn dilate(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    // Separable square dilation with running counts.
    let mut tmp = vec![false; w * h];
    for y in 0..h {
        let row = &mask[y * w..(y + 1) * w];
        let mut n = row[..(r + 1).min(w)].iter().filter(|v| **v).count() as i32;
        for x in 0..w {
            tmp[y * w + x] = n > 0;
            if x + r + 1 < w {
                n += row[x + r + 1] as i32;
            }
            if x >= r {
                n -= row[x - r] as i32;
            }
        }
    }
    let mut out = vec![false; w * h];
    for x in 0..w {
        let mut n = 0i32;
        for y in 0..(r + 1).min(h) {
            n += tmp[y * w + x] as i32;
        }
        for y in 0..h {
            out[y * w + x] = n > 0;
            if y + r + 1 < h {
                n += tmp[(y + r + 1) * w + x] as i32;
            }
            if y >= r {
                n -= tmp[(y - r) * w + x] as i32;
            }
        }
    }
    out
}
