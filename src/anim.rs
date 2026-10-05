//! Analysis of a logo that moves: an animation that plays the same way each
//! time (some channels bring their logo in this way at the start of a
//! programme), seen in several recordings that each contain it once.
//!
//! Every frame of the animation is a logo of its own, fitted like a still
//! one but across the recordings instead of across time: frame k of the
//! animation is blended over a different picture in each recording.
//!
//! 1. Alignment. Where the animation starts differs from one recording to
//!    the next. The pictures change at random moments, the animation at the
//!    same moment in every recording, so each recording is shifted until its
//!    changes line up with those of the others.
//! 2. The still logo the animation settles into is fitted with the ordinary
//!    scanner over the frames after it has settled.
//! 3. Each animation frame is fitted against a known background: the
//!    recording's picture just before the animation starts, or just after it
//!    has settled with the still logo removed, whichever still matches the
//!    frame around the logo.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::lgd::{self, LogoPixel, LOGO_MAX_DP};
use crate::scan::{self, Background, Params, Scanner};
use crate::source::{self, Frame, Input, ReadOptions, Reader, Rect, Scan, VideoInfo};

/// Low-resolution grid used for alignment: one cell per 8x8 pixels.
const CELL: usize = 8;

#[derive(Clone, Debug)]
pub struct AnimJob {
    pub inputs: Vec<Input>,
    /// The area the whole animation plays in.
    pub rect: Rect,
    /// A range read from every input (within its own stretch, if it has one).
    pub start: Option<f64>,
    pub end: Option<f64>,
    /// Largest shift tried between two recordings, in frames.
    pub search: usize,
    pub scan: Scan,
    pub threads: usize,
}

impl AnimJob {
    pub fn new(inputs: Vec<Input>, rect: Rect) -> AnimJob {
        AnimJob {
            inputs,
            rect,
            start: None,
            end: None,
            search: 90,
            scan: Scan::Auto,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        }
    }

    /// How to read one input; None when it has nothing in the range.
    fn read_options(&self, input: &Input) -> Option<ReadOptions> {
        let (start, end) = input.window(self.start, self.end)?;
        Some(ReadOptions {
            start,
            duration: end.map(|e| e - start.unwrap_or(0.0)),
            step: 1,
            threads: 4,
            scan: self.scan,
        })
    }
}

struct Opened {
    path: std::path::PathBuf,
    info: VideoInfo,
    opt: ReadOptions,
}

/// Runs `each` on every input, a few at a time; ffmpeg decodes with
/// several threads of its own.
fn for_inputs<T: Send>(
    inputs: &[Opened],
    threads: usize,
    cancel: &AtomicBool,
    each: &(dyn Fn(usize, &Opened) -> Result<T, String> + Sync),
) -> Result<Vec<T>, String> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<Result<T, String>>>> = Mutex::new((0..inputs.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..(threads / 4).clamp(1, 4) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= inputs.len() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let r = each(i, &inputs[i]);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    if cancel.load(Ordering::Relaxed) {
        return Err("中止しました".into());
    }
    out.into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect()
}

/// Hands every frame to `each` until it returns false.
fn read_all(input: &Opened, rect: Rect, mut each: impl FnMut(usize, Frame) -> bool) -> Result<usize, String> {
    let mut rd = Reader::open(&input.path, &input.info, rect, &input.opt).map_err(|e| format!("{}: {e}", input.path.display()))?;
    let mut n = 0;
    while let Some(f) = rd.next_frame().map_err(|e| format!("{}: {e}", input.path.display()))? {
        let go = each(n, f);
        n += 1;
        if !go {
            break;
        }
    }
    Ok(n)
}

/// Luma averaged over 8x8 cells.
fn cells(y: &[i16], w: usize, h: usize) -> Vec<i16> {
    let (cw, ch) = (w / CELL, h / CELL);
    let mut out = vec![0i16; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let mut s = 0i32;
            for r in 0..CELL {
                let row = &y[(cy * CELL + r) * w + cx * CELL..][..CELL];
                s += row.iter().map(|v| *v as i32).sum::<i32>();
            }
            out[cy * cw + cx] = (s / (CELL * CELL) as i32) as i16;
        }
    }
    out
}

/// Where each recording's changes line up with the others'.
pub struct Alignment {
    /// Per input: the frame that aligned time 0 falls on (may be negative).
    pub offsets: Vec<i64>,
    /// Aligned time of the first entry of `activity`.
    pub t0: i64,
    /// Per aligned time: how much more the pictures change together than
    /// they do at random.
    pub activity: Vec<f64>,
    /// Inputs contributing at each aligned time.
    pub count: Vec<usize>,
}

/// A change in a cell between consecutive frames, in PIXEL_YC units
/// (four 8-bit levels).
const CHANGE: i32 = 75;

fn changes(lo: &[Vec<i16>]) -> Vec<Vec<bool>> {
    let mut out = vec![vec![false; lo.first().map_or(0, |f| f.len())]];
    for w in lo.windows(2) {
        out.push(w[0].iter().zip(&w[1]).map(|(a, b)| (*a as i32 - *b as i32).abs() > CHANGE).collect());
    }
    out
}

/// How far the consensus may move a recording from its first offset.
const REFINE: i64 = 6;

fn bits(d: &[Vec<bool>]) -> Vec<Vec<u64>> {
    d.iter()
        .map(|f| {
            let mut w = vec![0u64; f.len().div_ceil(64)];
            for (i, b) in f.iter().enumerate() {
                if *b {
                    w[i / 64] |= 1 << (i % 64);
                }
            }
            w
        })
        .collect()
}

/// For every shift of `a` against `b` (`a` frame `t + o` on `b` frame `t`),
/// how many more cells change in both than chance would put there.
fn pair_scores(a: &[Vec<u64>], b: &[Vec<u64>], ca: &[u32], cb: &[u32], cells: f64, search: i64) -> Vec<f64> {
    (-search..=search)
        .map(|o| {
            let mut s = 0.0;
            for (t, fb) in b.iter().enumerate().skip(1) {
                let f = t as i64 + o;
                if f < 1 || f >= a.len() as i64 {
                    continue;
                }
                let fa = &a[f as usize];
                let both: u32 = fa.iter().zip(fb).map(|(x, y)| (x & y).count_ones()).sum();
                s += both as f64 - ca[f as usize] as f64 * cb[t] as f64 / cells;
            }
            s
        })
        .collect()
}

/// A first guess at the offsets: every pair of recordings is compared at
/// every shift, and the recording whose comparisons peak most clearly is
/// taken as the yardstick for the others.
fn first_offsets(d: &[Vec<Vec<bool>>], search: usize) -> Vec<i64> {
    let n = d.len();
    let s = search as i64;
    let cells = d.iter().find_map(|c| c.first().map(|f| f.len())).unwrap_or(1).max(1) as f64;
    let b: Vec<Vec<Vec<u64>>> = d.iter().map(|c| bits(c)).collect();
    let counts: Vec<Vec<u32>> = b.iter().map(|c| c.iter().map(|f| f.iter().map(|w| w.count_ones()).sum()).collect()).collect();
    // best[a][r] = (shift of a against r, how clearly it peaks)
    let best: Vec<Vec<(i64, f64)>> = {
        let next = AtomicUsize::new(0);
        let rows: Mutex<Vec<Vec<(i64, f64)>>> = Mutex::new(vec![vec![(0, 0.0); n]; n]);
        std::thread::scope(|sc| {
            for _ in 0..std::thread::available_parallelism().map(|v| v.get()).unwrap_or(4) {
                sc.spawn(|| loop {
                    let a = next.fetch_add(1, Ordering::Relaxed);
                    if a >= n {
                        break;
                    }
                    let mut row = vec![(0, 0.0); n];
                    for r in 0..n {
                        if r == a {
                            continue;
                        }
                        let sc = pair_scores(&b[a], &b[r], &counts[a], &counts[r], cells, s);
                        let (i, top) = sc.iter().enumerate().fold((0, f64::MIN), |m, (i, v)| if *v > m.1 { (i, *v) } else { m });
                        let mut rest: Vec<f64> = sc.iter().enumerate().filter(|(j, _)| j.abs_diff(i) > 2).map(|(_, v)| *v).collect();
                        let peak = if rest.len() > 4 {
                            let m = rest.len() / 2;
                            rest.select_nth_unstable_by(m, |x, y| x.total_cmp(y));
                            let med = rest[m];
                            let mut dev: Vec<f64> = rest.iter().map(|v| (v - med).abs()).collect();
                            dev.select_nth_unstable_by(m, |x, y| x.total_cmp(y));
                            (top - med) / dev[m].max(1.0)
                        } else {
                            0.0
                        };
                        row[r] = (i as i64 - s, peak);
                    }
                    rows.lock().unwrap()[a] = row;
                });
            }
        });
        rows.into_inner().unwrap()
    };
    let yard = (0..n)
        .max_by(|x, y| {
            let sx: f64 = (0..n).map(|a| best[a][*x].1.min(20.0)).sum();
            let sy: f64 = (0..n).map(|a| best[a][*y].1.min(20.0)).sum();
            sx.total_cmp(&sy)
        })
        .unwrap_or(0);
    let off: Vec<i64> = (0..n).map(|a| if a == yard { 0 } else { best[a][yard].0 }).collect();
    let m = off.iter().copied().min().unwrap_or(0);
    off.iter().map(|o| o - m).collect()
}

pub fn align(lo: &[Vec<Vec<i16>>], search: usize) -> Alignment {
    let d: Vec<Vec<Vec<bool>>> = lo.iter().map(|c| changes(c)).collect();
    let cells = d.iter().find_map(|c| c.first().map(|f| f.len())).unwrap_or(0);
    let n = d.len();
    let mut off = first_offsets(&d, search);
    for _round in 0..8 {
        // Aligned time t covers frame t + off[c] of input c.
        let t_lo = (0..n).map(|c| -off[c]).min().unwrap_or(0);
        let t_hi = (0..n).map(|c| d[c].len() as i64 - off[c]).max().unwrap_or(0);
        let len = (t_hi - t_lo).max(0) as usize;
        let mut sum = vec![vec![0u16; cells]; len];
        let mut cnt = vec![0u16; len];
        for c in 0..n {
            for (f, ch) in d[c].iter().enumerate().skip(1) {
                let t = (f as i64 - off[c] - t_lo) as usize;
                cnt[t] += 1;
                for (a, b) in sum[t].iter_mut().zip(ch) {
                    *a += *b as u16;
                }
            }
        }
        let mut changed = false;
        let mut new = off.clone();
        for c in 0..n {
            // The others' share of changes at each aligned time and cell,
            // less that cell's usual share.
            let mut z = vec![vec![0f32; cells]; len];
            let mut mean = vec![0f64; cells];
            let mut times = 0.0;
            for t in 0..len {
                let own = d[c].get((t as i64 + t_lo + off[c]) as usize).filter(|_| t as i64 + t_lo + off[c] >= 1);
                let k = cnt[t] as i32 - own.is_some() as i32;
                if k < 2 {
                    continue;
                }
                for p in 0..cells {
                    let v = (sum[t][p] as i32 - own.map_or(0, |o| o[p] as i32)) as f32 / k as f32;
                    z[t][p] = v;
                    mean[p] += v as f64;
                }
                times += 1.0;
            }
            if times < 1.0 {
                continue;
            }
            // Less each cell's usual share, and less what changes everywhere
            // at once (a cut lined up with the animation must not count).
            for zt in z.iter_mut() {
                for (v, m) in zt.iter_mut().zip(&mean) {
                    *v -= (m / times) as f32;
                }
                let all = zt.iter().sum::<f32>() / cells as f32;
                zt.iter_mut().for_each(|v| *v -= all);
            }
            let mut best = (f64::MIN, off[c]);
            for o in off[c] - REFINE..=off[c] + REFINE {
                let mut score = 0.0f64;
                for (f, ch) in d[c].iter().enumerate().skip(1) {
                    let t = f as i64 - o - t_lo;
                    if t < 0 || t >= len as i64 {
                        continue;
                    }
                    let zt = &z[t as usize];
                    for p in 0..cells {
                        if ch[p] {
                            score += zt[p] as f64;
                        }
                    }
                }
                if score > best.0 {
                    best = (score, o);
                }
            }
            if best.1 != off[c] {
                changed = true;
            }
            new[c] = best.1;
        }
        // Keep the earliest start at aligned time 0.
        let m = new.iter().copied().min().unwrap_or(0);
        off = new.iter().map(|o| o - m).collect();
        if !changed {
            break;
        }
    }
    // Activity on the final alignment.
    let t_lo = (0..n).map(|c| -off[c]).min().unwrap_or(0);
    let t_hi = (0..n).map(|c| d[c].len() as i64 - off[c]).max().unwrap_or(0);
    let len = (t_hi - t_lo).max(0) as usize;
    let mut share = vec![vec![0f32; cells]; len];
    let mut count = vec![0usize; len];
    for c in 0..n {
        for (f, ch) in d[c].iter().enumerate().skip(1) {
            let t = (f as i64 - off[c] - t_lo) as usize;
            count[t] += 1;
            for (a, b) in share[t].iter_mut().zip(ch) {
                *a += *b as u8 as f32;
            }
        }
    }
    for t in 0..len {
        if count[t] > 0 {
            for v in share[t].iter_mut() {
                *v /= count[t] as f32;
            }
        }
    }
    // A cell's usual share is its median over time.
    let mut usual = vec![0f32; cells];
    for (p, u) in usual.iter_mut().enumerate() {
        let mut v: Vec<f32> = (0..len).filter(|t| count[*t] * 2 >= n).map(|t| share[t][p]).collect();
        if !v.is_empty() {
            let m = v.len() / 2;
            v.select_nth_unstable_by(m, |a, b| a.total_cmp(b));
            *u = v[m];
        }
    }
    // Above the usual share, less what rose everywhere at once (cuts).
    let activity = (0..len)
        .map(|t| {
            let mut e: Vec<f32> = share[t].iter().zip(&usual).map(|(s, u)| s - u).collect();
            let m = e.len() / 2;
            let all = if e.is_empty() { 0.0 } else { *e.clone().select_nth_unstable_by(m, |a, b| a.total_cmp(b)).1 };
            e.iter_mut().map(|v| (*v - all).max(0.0) as f64).sum()
        })
        .collect();
    Alignment { offsets: off, t0: t_lo, activity, count }
}

/// The span of aligned time the animation plays in, from the activity:
/// it starts where the shared changes rise and stay up, and has settled
/// where they fall back and stay down.
fn active_span(a: &Alignment) -> Option<(i64, i64)> {
    let act = &a.activity;
    let n = act.len();
    if n < 8 {
        return None;
    }
    let mut sorted = act.clone();
    sorted.sort_by(|x, y| x.total_cmp(y));
    // Between the quiet level and the moving one, nearer the quiet.
    let (quiet_level, high) = (sorted[n / 4], sorted[n * 9 / 10]);
    if high <= quiet_level {
        return None;
    }
    let thr = quiet_level + (high - quiet_level) * 0.35;
    // Five frames of activity in a row.
    let start = (0..n.saturating_sub(5)).find(|&t| act[t..t + 5].iter().all(|v| *v > thr))?;
    // Smoothed over five frames, it must stay below for ten.
    let smooth: Vec<f64> = (0..n).map(|t| act[t.saturating_sub(2)..(t + 3).min(n)].iter().sum::<f64>() / 5.0).collect();
    let mut end = start;
    let mut quiet = 0;
    for (t, v) in smooth.iter().enumerate().skip(start) {
        if *v > thr {
            quiet = 0;
            end = t;
        } else {
            quiet += 1;
            if quiet >= 10 {
                break;
            }
        }
    }
    // The last frame that still changed on its own.
    while end > start && act[end] <= thr {
        end -= 1;
    }
    Some((start as i64 + a.t0, end as i64 + a.t0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Align,
    Locate,
    Still,
    Coarse,
    Fine(u32),
    Fade,
}

#[derive(Clone, Debug)]
pub struct AnimProgress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug)]
pub struct AnimFrame {
    /// Where this frame's logo lies in the picture.
    pub rect: Rect,
    pub pixels: Vec<LogoPixel>,
    /// Recordings whose background was known for this frame.
    pub samples: usize,
    /// Pixels whose opacity the data show.
    pub shown: usize,
}

#[derive(Clone, Debug)]
pub struct AnimOutcome {
    pub frames: Vec<AnimFrame>,
    /// What the result should be read with.
    pub warnings: Vec<String>,
    /// The logo the animation settles into.
    pub still: lgd::Logo,
    /// Per input, the frame (from the start of what was read) the animation
    /// starts on.
    pub starts: Vec<i64>,
    /// How long the still logo stays and how it fades, when the recordings
    /// show it go.
    pub hold: Option<Hold>,
}

/// delogomod's `end` and `fadeout` for the still logo, counted like its
/// `start`: `end` frames after the first frame of the animation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub end: i64,
    pub fadeout: i64,
}

/// A recording's background, PIXEL_YC per plane over the job's area.
type Planes = [Vec<f32>; 3];

fn plane(f: &Frame, p: usize) -> &[i16] {
    match p {
        0 => &f.y,
        1 => &f.cb,
        _ => &f.cr,
    }
}

fn mean_frames(frames: &[Frame]) -> Option<Planes> {
    let first = frames.first()?;
    let n = frames.len() as f32;
    Some(std::array::from_fn(|p| {
        let mut v = vec![0f32; first.y.len()];
        for f in frames {
            for (a, b) in v.iter_mut().zip(plane(f, p)) {
                *a += *b as f32;
            }
        }
        v.iter_mut().for_each(|a| *a /= n);
        v
    }))
}

fn crop(f: &Frame, w: usize, r: Rect) -> Frame {
    let pick = |src: &[i16]| {
        let mut v = Vec::with_capacity((r.w * r.h) as usize);
        for y in r.y..r.y + r.h {
            let at = y as usize * w + r.x as usize;
            v.extend_from_slice(&src[at..at + r.w as usize]);
        }
        v
    };
    Frame { y: pick(&f.y), cb: pick(&f.cb), cr: pick(&f.cr) }
}

/// Least-squares sums of one pixel: per plane n, x, y and xx, xy, yy.
///
/// Kept in integers, so that they come out the same whatever order the
/// recordings arrive in, and a sample can be taken out again exactly. Every
/// value is a mean of at most 16 PIXEL_YC samples, a whole number of
/// sixteenths; offset from the middle of the range, it fits an i32.
#[derive(Clone, Copy, Default)]
struct Sums {
    first: [[i32; 3]; 3],
    second: [[i64; 3]; 3],
}

const CENTRE: [f32; 3] = [2048.0, 0.0, 0.0];
const SCALE: f32 = 16.0;

impl Sums {
    fn add(&mut self, p: usize, x: f32, y: f32) {
        self.put(p, x, y, 1);
    }

    fn remove(&mut self, p: usize, x: f32, y: f32) {
        self.put(p, x, y, -1);
    }

    fn put(&mut self, p: usize, x: f32, y: f32, w: i32) {
        let x = ((x - CENTRE[p]) * SCALE).round() as i32;
        let y = ((y - CENTRE[p]) * SCALE).round() as i32;
        let f = &mut self.first[p];
        f[0] += w;
        f[1] += w * x;
        f[2] += w * y;
        let (x, y, w) = (x as i64, y as i64, w as i64);
        let s = &mut self.second[p];
        s[0] += w * x * x;
        s[1] += w * x * y;
        s[2] += w * y * y;
    }

    /// n, x, y, xx, xy, yy in (offset) PIXEL_YC.
    fn get(&self, p: usize) -> [f64; 6] {
        let f = self.first[p].map(|v| v as f64);
        let s = self.second[p].map(|v| v as f64);
        let (k, k2) = (SCALE as f64, (SCALE * SCALE) as f64);
        [f[0], f[1] / k, f[2] / k, s[0] / k2, s[1] / k2, s[2] / k2]
    }

    /// `obs = a * bg + b` in PIXEL_YC, and the spread of the samples about
    /// it.
    fn line(&self, p: usize) -> Option<Line> {
        let [n, sx, sy, sxx, sxy, syy] = self.get(p);
        let den = n * sxx - sx * sx;
        if n < 3.0 || den <= 1e-6 * n * n {
            return None;
        }
        let a = (n * sxy - sx * sy) / den;
        let b = (sy - a * sx) / n;
        let ssr = (syy - 2.0 * a * sxy - 2.0 * b * sy + a * a * sxx + 2.0 * a * b * sx + n * b * b).max(0.0);
        let sigma = (ssr / (n - 2.0)).sqrt();
        let c = CENTRE[p] as f64;
        // Back from the offset coordinates: y + c = a (x + c) + b'.
        Some(Line { a, b: b + c - a * c, sigma, se: sigma / (den / n).sqrt() })
    }
}

impl Sums {
    /// Lines of all three planes. The opacity is one for all planes, so the
    /// chroma slopes are drawn toward the luma slope: where the backgrounds
    /// hardly vary in colour the chroma data cannot tell the slope by
    /// themselves, and where they can (along edges, where chroma is
    /// subsampled) they move away from it.
    fn lines(&self) -> [Option<Line>; 3] {
        let y = self.line(0);
        let mut out = [y, None, None];
        if let Some(ly) = y {
            for (p, o) in out.iter_mut().enumerate().skip(1) {
                *o = self.line_toward(p, ly.a, CHROMA_PRIOR).or(self.line(p));
            }
        } else {
            out[1] = self.line(1);
            out[2] = self.line(2);
        }
        out
    }

    /// Least squares with a Gaussian prior `a0 ± tau` on the slope.
    fn line_toward(&self, p: usize, a0: f64, tau: f64) -> Option<Line> {
        let [n, sx, sy, sxx, sxy, syy] = self.get(p);
        if n < 1.0 {
            return None;
        }
        let (mx, my) = (sx / n, sy / n);
        let cxx = (sxx - sx * mx).max(0.0);
        let cxy = sxy - sx * my;
        let cyy = (syy - sy * my).max(0.0);
        let var = if n >= 3.0 && cxx > 0.0 {
            let a = cxy / cxx;
            ((cyy - a * cxy).max(0.0) / (n - 2.0)).max(16.0 * 16.0)
        } else {
            16.0 * 16.0
        };
        let a = (cxy / var + a0 / (tau * tau)) / (cxx / var + 1.0 / (tau * tau));
        let b = my - a * mx;
        let ssr = (cyy - 2.0 * a * cxy + a * a * cxx).max(0.0);
        let sigma = (ssr / (n - 1.0).max(1.0)).sqrt();
        let c = CENTRE[p] as f64;
        Some(Line { a, b: b + c - a * c, sigma, se: (var / (cxx + var / (tau * tau))).sqrt() })
    }
}

/// How far the chroma opacity may stray from the luma opacity without the
/// data insisting.
const CHROMA_PRIOR: f64 = 0.05;

#[derive(Clone, Copy, Debug)]
struct Line {
    a: f64,
    b: f64,
    sigma: f64,
    /// Standard error of `a`.
    se: f64,
}

impl Line {
    /// Whether a sample lies within three standard deviations of what this
    /// line, fitted on `sums`, predicts for it. The prediction is the less
    /// certain the farther the background lies from those it was fitted on:
    /// a lone dark background among bright ones is what fixes the slope, and
    /// must not be thrown out for disagreeing with a line the others cannot
    /// tilt.
    fn predicts(&self, sums: &Sums, p: usize, x: f32, y: f32) -> bool {
        let [n, sx, ..] = sums.get(p);
        if n < 1.0 {
            return true;
        }
        let dx = (x - CENTRE[p]) as f64 - sx / n;
        let s = self.sigma.max(16.0);
        let var = s * s * (1.0 + 1.0 / n) + self.se * self.se * dx * dx;
        if !var.is_finite() {
            return true;
        }
        ((y as f64) - (self.a * x as f64 + self.b)).abs() <= 3.0 * var.sqrt()
    }

    /// Whether the data suggest an opacity: twice its uncertainty.
    fn hints(&self, min: f64) -> bool {
        let alpha = 1.0 - self.a;
        alpha >= min && alpha >= 2.0 * self.se
    }

    /// Whether the data show an opacity at all: one that stands clear of
    /// its own uncertainty.
    fn shows(&self, min: f64) -> bool {
        let alpha = 1.0 - self.a;
        alpha >= min && alpha >= 4.0 * self.se
    }
}

/// One animation frame being fitted: the pixels of the area it may cover
/// and their sums.
struct Slot {
    /// Indices into the job's area.
    idx: Vec<u32>,
    /// Per area pixel: whether it is in `idx` (to measure the background
    /// around the logo).
    inside: Vec<bool>,
    /// Sums of the latest fit.
    prev: Option<Vec<Sums>>,
    /// Luma lines the recordings vote on, per pixel.
    cands: Vec<[(f32, f32); CANDIDATES]>,
    /// The line most recordings agreed with, per pixel.
    gate: Vec<(f32, f32)>,
    used: usize,
}

struct Shared {
    sums: Vec<Sums>,
    votes: Vec<[u16; CANDIDATES]>,
    used: usize,
}

const CANDIDATES: usize = 5;

/// How far (PIXEL_YC luma) a recording may lie from a line and still agree
/// with it: about three 8-bit levels.
const AGREE: f32 = 64.0;

/// Largest difference from the background, in PIXEL_YC, that nine in ten
/// pixels around the logo (half of all pixels, in the coarse pass) stay
/// within for the picture to count as unchanged (three 8-bit levels).
const MATCH: f32 = 56.0;

/// Side of the blocks the background is compared in.
const BLOCK: usize = 16;

/// Per block, the 90th percentile of |frame - bg| (luma) over the pixels
/// outside the logo; blocks the logo fills take the mean of their
/// neighbours, as the picture under them cannot be checked directly (what
/// differs under the logo is left to the per-pixel vote).
fn block_mismatch(f: &Frame, bg: &Planes, inside: &[bool], w: usize, h: usize) -> Vec<f32> {
    let (gw, gh) = (w.div_ceil(BLOCK), h.div_ceil(BLOCK));
    let mut m = vec![f32::NAN; gw * gh];
    let mut d = Vec::with_capacity(BLOCK * BLOCK);
    for gy in 0..gh {
        for gx in 0..gw {
            d.clear();
            for y in gy * BLOCK..((gy + 1) * BLOCK).min(h) {
                for x in gx * BLOCK..((gx + 1) * BLOCK).min(w) {
                    let i = y * w + x;
                    if !inside[i] {
                        d.push((f.y[i] as f32 - bg[0][i]).abs());
                    }
                }
            }
            if d.len() >= BLOCK * BLOCK / 8 {
                let q = d.len() * 9 / 10;
                m[gy * gw + gx] = *d.select_nth_unstable_by(q, |a, b| a.total_cmp(b)).1;
            }
        }
    }
    loop {
        let mut next = m.clone();
        let mut open = false;
        for gy in 0..gh {
            for gx in 0..gw {
                if !m[gy * gw + gx].is_nan() {
                    continue;
                }
                let (mut sum, mut n) = (0.0, 0);
                for ny in gy.saturating_sub(1)..(gy + 2).min(gh) {
                    for nx in gx.saturating_sub(1)..(gx + 2).min(gw) {
                        let v = m[ny * gw + nx];
                        if !v.is_nan() {
                            sum += v;
                            n += 1;
                        }
                    }
                }
                let v = if n > 0 { sum / n as f32 } else { f32::NAN };
                next[gy * gw + gx] = v;
                open |= v.is_nan();
            }
        }
        let progressed = next.iter().zip(&m).any(|(a, b)| a.is_nan() != b.is_nan());
        m = next;
        if !open || !progressed {
            break;
        }
    }
    m.iter().map(|v| if v.is_nan() { f32::MAX } else { *v }).collect()
}

/// Median of |frame - bg| (luma) over every 5th pixel of the area.
fn mismatch(f: &Frame, bg: &Planes) -> f32 {
    let mut d: Vec<f32> = (0..f.y.len()).step_by(5).map(|i| (f.y[i] as f32 - bg[0][i]).abs()).collect();
    if d.is_empty() {
        return f32::MAX;
    }
    let k = d.len() / 2;
    *d.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

pub fn run(job: &AnimJob, progress: &(dyn Fn(&AnimProgress) + Sync), cancel: &AtomicBool) -> Result<AnimOutcome, String> {
    let r = job.rect;
    if r.w < 16 || r.h < 16 {
        return Err("枠が小さすぎます。ロゴが動き回る範囲が全部入るように囲んでください".into());
    }
    if job.inputs.len() < 3 {
        return Err(format!(
            "録画が {} 本しかありません。動くロゴの解析には、同じ局の録画が 3 本以上要ります（30 本ほどあると安定します）",
            job.inputs.len()
        ));
    }
    let inputs: Vec<Opened> = job
        .inputs
        .iter()
        .map(|i| {
            let opt = job.read_options(i).ok_or_else(|| format!("{}: 範囲に入るところがありません", i.label()))?;
            let info = source::probe(&i.path).map_err(|e| format!("{}: {e}", i.path.display()))?;
            Ok(Opened { path: i.path.clone(), info, opt })
        })
        .collect::<Result<_, String>>()?;
    let (w, h) = (r.w as usize, r.h as usize);
    let total = inputs.len();
    let done = AtomicUsize::new(0);
    let tick = |stage: Stage| {
        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
        progress(&AnimProgress { stage, done: d, total });
    };
    let begin = |stage: Stage| {
        done.store(0, Ordering::Relaxed);
        progress(&AnimProgress { stage, done: 0, total });
    };

    // 1. Alignment.
    begin(Stage::Align);
    let lo = for_inputs(&inputs, job.threads, cancel, &|_, input| {
        let mut v = Vec::new();
        read_all(input, r, |_, f| {
            v.push(cells(&f.y, w, h));
            true
        })?;
        tick(Stage::Align);
        Ok(v)
    })?;
    let al = align(&lo, job.search);
    drop(lo);
    let (on, settled) = active_span(&al).ok_or("どの録画でも同じように動くものが、枠の中に見つかりません。枠がロゴの動く範囲を囲んでいるか、どの録画にもロゴのアニメーションが入っているかを確かめてください")?;
    let off = &al.offsets;
    // Frames fitted: a few before the rise (the first frames may be faint)
    // to well after the fall, which the shared changes place only roughly;
    // the coarse fit then finds where the logo stops changing. Backgrounds:
    // just before, and after.
    let k0 = on - 3;
    let mut k1 = settled + SLACK;
    let pre = [k0 - 2, k0 - 1];
    let mut post: Vec<i64> = (k1 + 3..k1 + 7).collect();
    // The still logo is read after the animation, to the end.
    let still_from = settled + 10;

    // 2. Where the still logo is: edges that stay put after the animation.
    begin(Stage::Locate);
    let presence = for_inputs(&inputs, job.threads, cancel, &|c, input| {
        let mut count = vec![0u16; w * h];
        let mut frames = 0u32;
        let mut luma = vec![0u8; w * h];
        read_all(input, r, |f, fr| {
            let t = f as i64 - off[c];
            if t >= still_from && (t - still_from) % 4 == 0 {
                for (o, v) in luma.iter_mut().zip(&fr.y) {
                    *o = (*v as i32 * 219 / 4096 + 16).clamp(0, 255) as u8;
                }
                crate::detect::edges(&luma, w, h, &mut count);
                frames += 1;
            }
            true
        })?;
        tick(Stage::Locate);
        Ok((count, frames))
    })?;
    let mut count = vec![0u32; w * h];
    let mut frames = 0u32;
    for (c, f) in &presence {
        for (a, b) in count.iter_mut().zip(c) {
            *a += *b as u32;
        }
        frames += f;
    }
    if frames < 8 {
        return Err("ロゴの動きが止まったあとの部分が、録画にほとんど入っていません。ロゴが画面から消えるまでを切り出してください".into());
    }
    // Edges there in most frames after the animation. Unlike `detect` on a
    // whole picture, nothing is dropped for being large or near the edge of
    // the area: the box drawn round a moving logo may be little larger than
    // the logo it settles into.
    let steady: Vec<bool> = count.iter().map(|c| *c as f32 >= frames as f32 * 0.45).collect();
    let steady = drop_specks(&steady, w, h, 20);
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (i, _) in steady.iter().enumerate().filter(|(_, s)| **s) {
        x0 = x0.min(i % w);
        y0 = y0.min(i / w);
        x1 = x1.max(i % w);
        y1 = y1.max(i / w);
    }
    if x0 == usize::MAX {
        return Err("ロゴの動きが止まったあと、枠の中にロゴが見つかりません。枠がロゴの止まる位置まで囲んでいるか、録画にロゴが画面から消えるまでが入っているかを確かめてください".into());
    }
    let tight = Rect { x: x0 as u32, y: y0 as u32, w: (x1 + 1 - x0) as u32, h: (y1 + 1 - y0) as u32 };
    let still_rect = crate::detect::grow(tight, 3, r.w, r.h);

    // 3. The still logo, with the ordinary scanner.
    begin(Stage::Still);
    let params = Params {
        background: Background::Plane,
        threshold_y: 12.0 * 4096.0 / 219.0,
        threshold_c: 12.0 * 4096.0 / 224.0,
        max_frames: 8000,
        robust_passes: 3,
    };
    let scanner = Mutex::new(Scanner::new(still_rect.w as usize, still_rect.h as usize, params));
    for_inputs(&inputs, job.threads, cancel, &|c, input| {
        let mut mine = Vec::new();
        read_all(input, r, |f, fr| {
            if f as i64 - off[c] >= still_from {
                mine.push(crop(&fr, w, still_rect));
            }
            true
        })?;
        let mut s = scanner.lock().unwrap();
        for fr in mine {
            s.push(fr);
        }
        tick(Stage::Still);
        Ok(())
    })?;
    let report = scanner.into_inner().unwrap().finish(job.threads);
    if report.frames_used < 2 {
        return Err("動きが止まったあとのロゴを求められません（ロゴのまわりの背景が、どの録画でも模様や動きのある絵でした）。録画を増やしてください".into());
    }
    let still = lgd::Logo {
        x: still_rect.x as i16,
        y: still_rect.y as i16,
        w: still_rect.w as i16,
        h: still_rect.h as i16,
        pixels: report.pixels,
        ..Default::default()
    };
    // With few recordings the ring around the still logo is seldom flat,
    // and a fit on a handful of pictures can come out inverted. Then it is
    // not trusted to clear the backgrounds after the animation.
    let mut warnings = Vec::new();
    let mean_dp = still.pixels.iter().map(|p| p.dp_y as f64).sum::<f64>() / still.pixels.len().max(1) as f64;
    let still_ok = report.frames_used >= STILL_FRAMES && mean_dp > 0.0;
    if !still_ok {
        warnings.push(format!(
            "動きが止まったあとのロゴを求めるのに使えた絵が {} 枚しかなく、当てになりません。そのため終わりのほうのフレームは、アニメーション直前の絵だけから求めました。録画を増やすと良くなります（30 本ほど）",
            report.frames_used
        ));
    }
    // In area coordinates, for removing it from the area's frames.
    let area = Rect { x: 0, y: 0, w: r.w, h: r.h };

    let mut nk = (k1 - k0 + 1) as usize;
    let mut settle: Option<usize> = None;
    // 4. Coarse fit of luma at half resolution, everywhere in the area, to
    // learn which pixels each frame covers.
    let (bw, bh) = (w / 2, h / 2);
    let full_idx: Vec<u32> = (0..(w * h) as u32).collect();
    let mut slots: Vec<Slot> = Vec::new();
    // Passes: 0 coarse; 1 plain least squares; 2 counts how many
    // recordings agree with each candidate line; 3 fits the recordings that
    // agree with the best candidate; 4 drops what still stands out.
    let mut coarse_lines: Vec<Vec<Option<(f32, f32)>>> = Vec::new();
    for pass in 0..5u32 {
        let coarse = pass == 0;
        begin(if coarse { Stage::Coarse } else { Stage::Fine(pass) });
        let shared: Vec<Mutex<Shared>> = (0..nk)
            .map(|k| {
                let len = if coarse { bw * bh } else { slots[k].idx.len() };
                Mutex::new(Shared {
                    sums: if pass == 2 { Vec::new() } else { vec![Sums::default(); len] },
                    votes: if pass == 2 { vec![[0; CANDIDATES]; len] } else { Vec::new() },
                    used: 0,
                })
            })
            .collect();
        let slots_ref = &slots;
        for_inputs(&inputs, job.threads, cancel, &|c, input| {
            // The frames this recording contributes, by aligned time.
            let want = |t: i64| (pre[0]..=k1).contains(&t) || post.contains(&t);
            let mut got: Vec<(i64, Frame)> = Vec::new();
            read_all(input, r, |f, fr| {
                let t = f as i64 - off[c];
                if want(t) {
                    got.push((t, fr));
                }
                t <= *post.last().unwrap()
            })?;
            let pick = |ts: &[i64]| -> Vec<Frame> {
                got.iter().filter(|(t, _)| ts.contains(t)).map(|(_, f)| Frame { y: f.y.clone(), cb: f.cb.clone(), cr: f.cr.clone() }).collect()
            };
            let pre_frames = pick(&pre);
            let bg_pre = if pre_frames.len() == pre.len() { mean_frames(&pre_frames) } else { None };
            let mut post_frames = pick(&post);
            for f in post_frames.iter_mut() {
                crate::erase::remove(&still, f, area);
            }
            let bg_post = if still_ok && post_frames.len() == post.len() { mean_frames(&post_frames) } else { None };
            drop(pre_frames);
            drop(post_frames);
            for (t, fr) in &got {
                if *t < k0 || *t > k1 {
                    continue;
                }
                let k = (*t - k0) as usize;
                let cands: Vec<&Planes> = [&bg_pre, &bg_post].into_iter().flatten().collect();
                if coarse {
                    // Nothing is known about the logo yet: the whole frame
                    // must match.
                    let best = cands.iter().map(|b| (mismatch(fr, b), *b)).min_by(|a, b| a.0.total_cmp(&b.0));
                    let Some((m, bg)) = best else { continue };
                    if m > MATCH {
                        continue;
                    }
                    let mut sh = shared[k].lock().unwrap();
                    sh.used += 1;
                    for by in 0..bh {
                        for bx in 0..bw {
                            let mut x = 0.0;
                            let mut y = 0.0;
                            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                                let i = (by * 2 + dy) * w + bx * 2 + dx;
                                x += bg[0][i];
                                y += fr.y[i] as f32;
                            }
                            sh.sums[by * bw + bx].add(0, x / 4.0, y / 4.0);
                        }
                    }
                    continue;
                }
                // Block by block: a pixel is used where the picture around
                // the logo still matches the background.
                let slot = &slots_ref[k];
                let maps: Vec<Vec<f32>> = cands.iter().map(|b| block_mismatch(fr, b, &slot.inside, w, h)).collect();
                let gw = w.div_ceil(BLOCK);
                let mut any = false;
                let mut sh = shared[k].lock().unwrap();
                for (j, &i) in slot.idx.iter().enumerate() {
                    let i = i as usize;
                    let blk = (i / w / BLOCK) * gw + (i % w) / BLOCK;
                    let Some((m, bg)) = maps.iter().zip(&cands).map(|(m, b)| (m[blk], *b)).min_by(|a, b| a.0.total_cmp(&b.0)) else { continue };
                    if m > MATCH {
                        continue;
                    }
                    any = true;
                    let (x0, y0) = (bg[0][i], fr.y[i] as f32);
                    match pass {
                        2 => {
                            for (v, (a, b)) in sh.votes[j].iter_mut().zip(slot.cands[j]) {
                                if (y0 - (a * x0 + b)).abs() <= AGREE {
                                    *v += 1;
                                }
                            }
                            continue;
                        }
                        3 | 4 => {
                            let (a, b) = slot.gate[j];
                            if (y0 - (a * x0 + b)).abs() > AGREE {
                                continue;
                            }
                        }
                        _ => {}
                    }
                    // Last pass: each sample against the fit of the others,
                    // so that one bad recording among few cannot hide by
                    // pulling the line toward itself.
                    let others = (pass == 4).then(|| {
                        let mut o = slot.prev.as_ref().unwrap()[j];
                        for (p, b) in bg.iter().enumerate() {
                            o.remove(p, b[i], plane(fr, p)[i] as f32);
                        }
                        (o, o.lines())
                    });
                    let off_line = |p: usize, x: f32, y: f32| {
                        others.as_ref().is_some_and(|(o, l)| l[p].is_some_and(|l| !l.predicts(o, p, x, y)))
                    };
                    if off_line(0, x0, y0) {
                        continue;
                    }
                    for (p, b) in bg.iter().enumerate() {
                        let (x, y) = (b[i], plane(fr, p)[i] as f32);
                        if off_line(p, x, y) {
                            continue;
                        }
                        sh.sums[j].add(p, x, y);
                    }
                }
                if any {
                    sh.used += 1;
                }
            }
            tick(if coarse { Stage::Coarse } else { Stage::Fine(pass) });
            Ok(())
        })?;
        let mut results: Vec<Shared> = shared.into_iter().map(|m| m.into_inner().unwrap()).collect();
        if coarse {
            // Where the logo stops changing; the fine passes stop a few
            // frames after, and take the background right after that.
            let alpha: Vec<Vec<f32>> = results
                .iter()
                .map(|r| r.sums.iter().map(|s| s.line(0).filter(|l| l.shows(0.02)).map_or(0.0, |l| (1.0 - l.a) as f32)).collect())
                .collect();
            if let Some(end) = settles(&alpha) {
                settle = Some(end);
                nk = (end + 1 + TAIL).min(nk);
                results.truncate(nk);
                k1 = k0 + nk as i64 - 1;
                post = (k1 + 3..k1 + 7).collect();
            }
            coarse_lines = results.iter().map(|r| r.sums.iter().map(|s| s.line(0).map(|l| (l.a as f32, l.b as f32))).collect()).collect();
            // Which pixels each frame covers: an opacity the data show at
            // half resolution, without specks, widened by 8 pixels.
            slots = results
                .iter()
                .map(|res| {
                    // Leniently: what is let in here and shows nothing is
                    // cleared after the fine fit, what is left out is lost.
                    let raw: Vec<bool> = res.sums.iter().map(|s| s.line(0).is_some_and(|l| l.hints(0.02))).collect();
                    let clean = fill_holes(&drop_specks(&raw, bw, bh, 12), bw, bh);
                    let mut full = vec![false; w * h];
                    for y in 0..h {
                        for x in 0..w {
                            full[y * w + x] = clean[((y / 2).min(bh - 1)) * bw + (x / 2).min(bw - 1)];
                        }
                    }
                    let inside = crate::detect::dilate(&full, w, h, 8);
                    let idx: Vec<u32> = full_idx.iter().copied().filter(|i| inside[*i as usize]).collect();
                    Slot { idx, inside, prev: None, cands: Vec::new(), gate: Vec::new(), used: res.used }
                })
                .collect();
        } else if pass == 2 {
            for (slot, res) in slots.iter_mut().zip(results) {
                slot.gate = slot.cands.iter().zip(&res.votes).map(|(c, v)| c[(0..CANDIDATES).max_by_key(|i| (v[*i], CANDIDATES - i)).unwrap()]).collect();
                slot.cands = Vec::new();
            }
        } else {
            for (slot, res) in slots.iter_mut().zip(results) {
                slot.prev = Some(res.sums);
                slot.used = res.used;
            }
            if pass == 1 {
                // Candidates: this fit, the fits of the neighbouring frames
                // at the same pixel, the coarse fit, and no logo at all.
                let line_at = |k: usize, i: u32| -> (f32, f32) {
                    let sl = &slots[k];
                    match sl.idx.binary_search(&i) {
                        Ok(j) => sl.prev.as_ref().unwrap()[j].line(0).map_or((f32::NAN, f32::NAN), |l| (l.a as f32, l.b as f32)),
                        Err(_) => (1.0, 0.0),
                    }
                };
                let cands: Vec<Vec<[(f32, f32); CANDIDATES]>> = (0..nk)
                    .map(|k| {
                        slots[k]
                            .idx
                            .iter()
                            .map(|&i| {
                                let (x, y) = (i as usize % w, i as usize / w);
                                let cb = coarse_lines[k][(y / 2).min(bh - 1) * bw + (x / 2).min(bw - 1)].unwrap_or((f32::NAN, f32::NAN));
                                [
                                    line_at(k, i),
                                    if k > 0 { line_at(k - 1, i) } else { (f32::NAN, f32::NAN) },
                                    if k + 1 < nk { line_at(k + 1, i) } else { (f32::NAN, f32::NAN) },
                                    cb,
                                    (1.0, 0.0),
                                ]
                            })
                            .collect()
                    })
                    .collect();
                for (slot, c) in slots.iter_mut().zip(cands) {
                    slot.cands = c;
                }
            }
        }
    }

    // 5. The frames, each cut to the box of the pixels whose opacity the
    // data show; the rest of the area stays clear.
    let mut frames = Vec::with_capacity(nk);
    for slot in &slots {
        let lines: Vec<[Option<Line>; 3]> = slot.prev.as_ref().unwrap().iter().map(|s| s.lines()).collect();
        let mut keep = vec![false; w * h];
        for (j, &i) in slot.idx.iter().enumerate() {
            keep[i as usize] = lines[j][0].is_some_and(|l| l.shows(0.01));
        }
        let keep = drop_specks(&keep, w, h, 24);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let mut shown = 0;
        for (i, _) in keep.iter().enumerate().filter(|(_, k)| **k) {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            shown += 1;
        }
        if shown == 0 {
            frames.push(AnimFrame { rect: Rect { x: r.x, y: r.y, w: 1, h: 1 }, pixels: vec![LogoPixel::default()], samples: slot.used, shown });
            continue;
        }
        let b = crate::detect::grow(Rect { x: x0 as u32, y: y0 as u32, w: (x1 + 1 - x0) as u32, h: (y1 + 1 - y0) as u32 }, MARGIN, r.w, r.h);
        let (fw, fh) = (b.w as usize, b.h as usize);
        let mut pixels = vec![LogoPixel::default(); fw * fh];
        for (j, &i) in slot.idx.iter().enumerate() {
            let (x, y) = (i as usize % w, i as usize / w);
            if !keep[i as usize] && !near(&keep, w, h, x, y, MARGIN as usize) {
                continue;
            }
            if x < b.x as usize || y < b.y as usize || x >= (b.x + b.w) as usize || y >= (b.y + b.h) as usize {
                continue;
            }
            let v = lines[j].map(|l| l.map_or((0, 0), |l| opaque_at_most(scan::to_lgd(l.a, l.b))));
            pixels[(y - b.y as usize) * fw + x - b.x as usize] = LogoPixel { dp_y: v[0].0, y: v[0].1, dp_cb: v[1].0, cb: v[1].1, dp_cr: v[2].0, cr: v[2].1 };
        }
        frames.push(AnimFrame { rect: Rect { x: r.x + b.x, y: r.y + b.y, w: b.w, h: b.h }, pixels, samples: slot.used, shown });
    }
    let mut still = still;
    still.x += r.x as i16;
    still.y += r.y as i16;
    // The animation begins with the first frame that shows a fair part of
    // what the others do, and ends where it stops changing.
    let typical = {
        let mut v: Vec<usize> = frames.iter().map(|f| f.shown).collect();
        v.sort_unstable();
        v[v.len() / 2]
    };
    // Specks of noise come and go; a logo covers much the same pixels in
    // the next frame.
    let first = (0..frames.len())
        .find(|&k| {
            frames[k].shown >= MIN_SHOWN.max(typical / 4) && frames.get(k + 1).is_none_or(|next| overlap(&frames[k], next) >= 0.5)
        })
        .ok_or("ロゴのアニメーションを取り出せませんでした。枠と録画を確かめてください")?;
    let last = settle.unwrap_or(frames.len() - 1).clamp(first, frames.len() - 1);
    let frames: Vec<AnimFrame> = frames.into_iter().take(last + 1).skip(first).collect();
    let thin: Vec<usize> = (0..frames.len()).filter(|&k| frames[k].samples < FEW).collect();
    if let (Some(a), Some(b)) = (thin.first(), thin.last()) {
        let at = if a == b { format!("{a} フレーム目") } else { format!("{a}〜{b} フレーム目のあたりの {} フレーム", thin.len()) };
        warnings.push(format!("{at}は、使えた録画が {FEW} 本に届かず、ロゴの形が当てになりません。録画を増やすと良くなります"));
    }
    // 6. How long the still logo stays, and how it fades: per recording and
    // frame, the depth at which removing it leaves its edges flattest. A
    // still logo that cannot be trusted cannot measure that either.
    let a0 = k0 + first as i64;
    let hold = if still_ok {
        begin(Stage::Fade);
        let rest = k0 + last as i64;
        let srect = Rect { x: still.x as u32, y: still.y as u32, w: still.w as u32, h: still.h as u32 };
        let band = edge_band(&still);
        let depths = for_inputs(&inputs, job.threads, cancel, &|c, input| {
            let mut v = Vec::new();
            read_all(input, srect, |f, fr| {
                let t = f as i64 - off[c];
                if t > rest {
                    v.push((t - a0, depth(&still, &fr.y, &band)));
                }
                true
            })?;
            tick(Stage::Fade);
            Ok(v)
        })?;
        let hold = fit_fade(&depths, rest - a0);
        if hold.is_none() {
            warnings.push("録画がロゴの消える前で終わっているので、end と fadeout を測れませんでした。ロゴが画面から消えるまでを切り出すと測れます".into());
        }
        hold
    } else {
        warnings.push("動きが止まったあとのロゴが当てにならないので、end と fadeout は測っていません".into());
        None
    };
    let starts = off.iter().map(|o| o + a0).collect();
    Ok(AnimOutcome { frames, warnings, still, starts, hold })
}

/// Pixels kept around what the data show, so the edge of the logo is not
/// cut where its opacity fades below what can be told apart.
const MARGIN: u32 = 3;

/// Pixels a frame must show to count as part of the animation (and a
/// quarter of what a typical frame shows: with few recordings, specks of
/// noise make up frames of their own before the logo comes in).
const MIN_SHOWN: usize = 64;

/// Recordings a frame should be fitted on.
const FEW: usize = 5;

/// Frames the still logo must be fitted on to be trusted.
const STILL_FRAMES: usize = 150;

/// Frames fitted past where the shared changes die down, in case they do
/// so before the logo has quite settled.
const SLACK: i64 = 30;

/// Frames fitted after the one the logo settles on.
const TAIL: usize = 5;

/// Share of the pixels `a` covers that `b` covers too.
fn overlap(a: &AnimFrame, b: &AnimFrame) -> f64 {
    let covers = |f: &AnimFrame, x: u32, y: u32| {
        x >= f.rect.x
            && y >= f.rect.y
            && x < f.rect.x + f.rect.w
            && y < f.rect.y + f.rect.h
            && f.pixels[((y - f.rect.y) * f.rect.w + x - f.rect.x) as usize].dp_y != 0
    };
    let (mut both, mut all) = (0usize, 0usize);
    for y in a.rect.y..a.rect.y + a.rect.h {
        for x in a.rect.x..a.rect.x + a.rect.w {
            if covers(a, x, y) {
                all += 1;
                both += covers(b, x, y) as usize;
            }
        }
    }
    if all == 0 { 0.0 } else { both as f64 / all as f64 }
}

fn median3(v: &[f64]) -> Vec<f64> {
    (0..v.len())
        .map(|k| {
            let mut w: Vec<f64> = v[k.saturating_sub(1)..(k + 2).min(v.len())].to_vec();
            w.sort_by(|a, b| a.total_cmp(b));
            w[w.len() / 2]
        })
        .collect()
}

/// The frame the logo settles on: where the change from one opacity map to
/// the next falls to a small part of what it was while the logo moved, and
/// stays there on average (the coarse fit has the odd jump after that).
fn settles(alpha: &[Vec<f32>]) -> Option<usize> {
    let step: Vec<f64> = alpha
        .windows(2)
        .map(|p| {
            let (mut sum, mut n) = (0.0f64, 0usize);
            for (a, b) in p[0].iter().zip(&p[1]) {
                if *a != 0.0 || *b != 0.0 {
                    sum += (a - b).abs() as f64;
                    n += 1;
                }
            }
            if n == 0 { 0.0 } else { sum / n as f64 }
        })
        .collect();
    const RUN: usize = 10;
    if step.len() < 2 * RUN {
        return None;
    }
    let moving = {
        let mut v = step.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() * 3 / 4]
    };
    let low = moving * 0.15;
    let smooth = median3(&step);
    (0..step.len() - RUN).find(|&k| smooth[k] <= low && step[k..k + RUN].iter().sum::<f64>() / RUN as f64 <= low)
}

/// Neighbouring pixels across which the still logo's opacity steps by a
/// tenth or more: (index, neighbour) in the logo's own box.
fn edge_band(logo: &lgd::Logo) -> Vec<(usize, usize)> {
    let (w, h) = (logo.w as usize, logo.h as usize);
    let dp = |i: usize| logo.pixels[i].dp_y as i32;
    let mut band = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if x + 1 < w && (dp(i) - dp(i + 1)).abs() >= 100 {
                band.push((i, i + 1));
            }
            if y + 1 < h && (dp(i) - dp(i + w)).abs() >= 100 {
                band.push((i, i + w));
            }
        }
    }
    band
}

/// The share of the still logo's opacity (0 to 1.2) whose removal leaves
/// the least step across its edges in this frame's luma.
fn depth(logo: &lgd::Logo, y: &[i16], band: &[(usize, usize)]) -> f32 {
    if band.is_empty() {
        return 0.0;
    }
    let erased = |i: usize, m: f64| -> f64 {
        let p = logo.pixels[i];
        let dp = (p.dp_y as f64 * m).min(LOGO_MAX_DP as f64 - 1.0);
        (y[i] as f64 * LOGO_MAX_DP as f64 - p.y as f64 * dp) / (LOGO_MAX_DP as f64 - dp)
    };
    let energy = |m: f64| -> f64 { band.iter().map(|&(i, j)| (erased(i, m) - erased(j, m)).abs()).sum() };
    let mut best = (f64::MAX, 0.0);
    for k in 0..=24 {
        let m = k as f64 * 0.05;
        let e = energy(m);
        if e < best.0 {
            best = (e, m);
        }
    }
    // Finer around the best step.
    let c = best.1;
    for k in -5..=5 {
        let m = c + k as f64 * 0.01;
        if (0.0..=1.2).contains(&m) {
            let e = energy(m);
            if e < best.0 {
                best = (e, m);
            }
        }
    }
    best.1 as f32
}

/// delogomod's fade: full depth up to `end - fadeout`, then down step by
/// step to nothing after `end`.
fn fade_at(u: i64, end: i64, fadeout: i64) -> f64 {
    if u > end {
        0.0
    } else if u > end - fadeout {
        ((end - u) * 2 + 1) as f64 / (fadeout * 2) as f64
    } else {
        1.0
    }
}

/// The `end` and `fadeout` that best fit the depths measured after the
/// logo settled on frame `rest` (frames counted from the animation's
/// first), taking the middle value of the recordings at each frame.
fn fit_fade(depths: &[Vec<(i64, f32)>], rest: i64) -> Option<Hold> {
    let mut by: std::collections::BTreeMap<i64, Vec<f32>> = std::collections::BTreeMap::new();
    for v in depths {
        for &(u, m) in v {
            by.entry(u).or_default().push(m);
        }
    }
    let enough = (depths.len() / 4).max(3);
    let series: Vec<(i64, f64)> = by
        .into_iter()
        .filter(|(_, v)| v.len() >= enough)
        .map(|(u, mut v)| {
            v.sort_by(|a, b| a.total_cmp(b));
            (u, v[v.len() / 2] as f64)
        })
        .collect();
    if series.len() < 20 {
        return None;
    }
    // The logo must be seen to go: low at the end of what was read.
    let tail = &series[series.len() - 5..];
    if tail.iter().map(|(_, m)| m).sum::<f64>() / tail.len() as f64 > 0.25 {
        return None;
    }
    let (u0, u1) = (series[0].0, series[series.len() - 1].0);
    let mut best = (f64::MAX, Hold { end: u1, fadeout: 1 });
    for end in rest.max(u0)..=u1 {
        for fadeout in 1..=(end - rest).max(1) {
            let sse: f64 = series.iter().map(|&(u, m)| (m - fade_at(u, end, fadeout)).powi(2)).sum();
            if sse < best.0 {
                best = (sse, Hold { end, fadeout });
            }
        }
    }
    Some(best.1)
}

/// Removing a logo divides by `1000 - dp`, and delogo does not guard
/// against more than 1000; a pixel that noise puts there would turn the
/// picture inside out, so it is kept just short of opaque.
fn opaque_at_most((dp, c): (i16, i16)) -> (i16, i16) {
    (dp.min(LOGO_MAX_DP as i16 - 1), c)
}

fn near(mask: &[bool], w: usize, h: usize, x: usize, y: usize, r: usize) -> bool {
    for yy in y.saturating_sub(r)..(y + r + 1).min(h) {
        for xx in x.saturating_sub(r)..(x + r + 1).min(w) {
            if mask[yy * w + xx] {
                return true;
            }
        }
    }
    false
}

/// Sets the pixels the mask encloses: those the outside cannot reach.
fn fill_holes(mask: &[bool], w: usize, h: usize) -> Vec<bool> {
    let mut outside = vec![false; mask.len()];
    let mut stack: Vec<usize> = (0..mask.len()).filter(|&i| (i % w == 0 || i % w == w - 1 || i / w == 0 || i / w == h - 1) && !mask[i]).collect();
    for &i in &stack {
        outside[i] = true;
    }
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let mut visit = |j: usize| {
            if !mask[j] && !outside[j] {
                outside[j] = true;
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
    outside.iter().map(|o| !o).collect()
}

/// Drops connected groups of fewer than `min` pixels.
fn drop_specks(mask: &[bool], w: usize, h: usize, min: usize) -> Vec<bool> {
    let mut out = mask.to_vec();
    let mut seen = vec![false; mask.len()];
    let mut stack = Vec::new();
    let mut group = Vec::new();
    for s in 0..mask.len() {
        if !mask[s] || seen[s] {
            continue;
        }
        seen[s] = true;
        stack.push(s);
        group.clear();
        while let Some(i) = stack.pop() {
            group.push(i);
            let (x, y) = (i % w, i / w);
            let mut visit = |j: usize| {
                if mask[j] && !seen[j] {
                    seen[j] = true;
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
        if group.len() < min {
            for &i in &group {
                out[i] = false;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recordings of random pictures, each with the same moving square
    /// starting at a different frame, are lined up again.
    #[test]
    fn alignment_finds_the_shared_motion() {
        let (cw, ch) = (64usize, 20usize);
        let mut seed = 7u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let starts = [5usize, 17, 0, 29, 11, 8];
        let lo: Vec<Vec<Vec<i16>>> = starts
            .iter()
            .map(|&s| {
                let mut pic: Vec<i16> = (0..cw * ch).map(|_| (rand() % 2000) as i16).collect();
                (0..120)
                    .map(|t| {
                        // A cut now and then; the rest stays put.
                        if rand() % 25 == 0 {
                            pic.iter_mut().for_each(|v| *v = (rand() % 2000) as i16);
                        }
                        let mut f = pic.clone();
                        if t >= s && t < s + 40 {
                            // Accelerating, so that no shift looks alike.
                            let u = t - s;
                            let (x, y0) = (2 + u + u * u / 120, 4 + u / 8);
                            for y in y0..y0 + 6 {
                                for dx in 0..5 {
                                    f[y * cw + x + dx] = 3500;
                                }
                            }
                        }
                        f
                    })
                    .collect()
            })
            .collect();
        let a = align(&lo, 40);
        let base = a.offsets[0] - starts[0] as i64;
        for (o, s) in a.offsets.iter().zip(starts) {
            assert_eq!(o - s as i64, base, "{:?}", a.offsets);
        }
        let (on, _) = active_span(&a).unwrap();
        assert!((on - (starts[0] as i64 - a.offsets[0])).abs() <= 1, "{on} {:?}", a.offsets);
    }

    #[test]
    fn chroma_slope_leans_on_luma() {
        let mut s = Sums::default();
        // Luma: alpha 0.4 over a wide spread of backgrounds.
        for (i, x) in [200.0, 900.0, 1600.0, 2300.0, 3000.0, 3700.0].iter().enumerate() {
            s.add(0, *x, 0.6 * x + 0.4 * 4000.0);
            // Chroma: backgrounds barely differ, so the slope alone is
            // unknowable; one noisy sample.
            let cx = 10.0 * i as f32;
            s.add(1, cx, 0.6 * cx + if i == 2 { 30.0 } else { 0.0 });
        }
        let [y, cb, _] = s.lines();
        assert!((y.unwrap().a - 0.6).abs() < 1e-3);
        assert!((cb.unwrap().a - 0.6).abs() < 0.1, "{:?}", cb);
    }
}
