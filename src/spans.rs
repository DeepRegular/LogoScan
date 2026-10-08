//! Where a station logo is on screen in a recording, and how it fades in
//! and out: the start, end, fadein and fadeout of delogo's EraseLOGO for
//! each stretch of the programme.
//!
//! Each frame is measured for how much of the logo it carries: the share of
//! the logo's opacity whose removal leaves the least step across the edges
//! the logo draws. Those edges are taken in all three planes, from steps in
//! the opacity and in what the logo adds to the picture; a logo drawn in
//! colours on an evenly translucent plate has its letters only in the
//! latter, and on a white background its plate vanishes into the picture.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::lgd::{Logo, LOGO_MAX_DP};
use crate::source::{self, Frame, ReadOptions, Reader, Rect, VideoInfo};

/// What the logo adds to the picture must step by this much between
/// neighbours to count as an edge it draws: three 8-bit levels.
const EDGE_ADDS: f64 = 48.0;
/// Or its opacity by a tenth.
const EDGE_DP: f64 = 100.0;
/// A picture on which the logo would show less than this, in 8-bit
/// levels, cannot tell whether it is there.
const SHOWING_MIN: f32 = 2.0;
/// Fades longer than this are not looked for.
const FADE_MAX_SEC: f64 = 4.0;
/// A gap in the logo shorter than this is a scene it cannot be told apart
/// in, not a break.
const GAP_SEC: f64 = 5.0;
/// Stretches shorter than this are left out.
const SPAN_MIN_SEC: f64 = 3.0;

/// One stretch the logo is on screen: frames `start..=end`, counted from
/// the first frame read, fading in over `fadein` frames and out over
/// `fadeout`, as EraseLOGO counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
    pub fadein: u64,
    pub fadeout: u64,
}

/// Measures how much of a logo each frame carries.
pub struct Meter {
    /// (plane, pixel, neighbour) across which the logo draws an edge.
    band: Vec<(u8, u32, u32)>,
    /// Per plane and pixel: opacity and colour.
    coef: [Vec<(f64, f64)>; 3],
}

impl Meter {
    pub fn new(logo: &Logo) -> Result<Meter, String> {
        let (w, h) = (logo.w.max(0) as usize, logo.h.max(0) as usize);
        let coef = [0, 1, 2].map(|p| {
            logo.pixels
                .iter()
                .map(|q| match p {
                    0 => (q.dp_y as f64, q.y as f64),
                    1 => (q.dp_cb as f64, q.cb as f64),
                    _ => (q.dp_cr as f64, q.cr as f64),
                })
                .collect::<Vec<_>>()
        });
        let mut band = Vec::new();
        for (p, c) in coef.iter().enumerate() {
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let right = (x + 1 < w).then_some(i + 1);
                    let below = (y + 1 < h).then_some(i + w);
                    for j in [right, below].into_iter().flatten() {
                        let ((a, l), (b, m)) = (c[i], c[j]);
                        if (a - b).abs() >= EDGE_DP || (a * l - b * m).abs() / LOGO_MAX_DP as f64 >= EDGE_ADDS {
                            band.push((p as u8, i as u32, j as u32));
                        }
                    }
                }
            }
        }
        if band.is_empty() {
            return Err("ロゴの縁が見つからないので、出ている区間を測れません".into());
        }
        Ok(Meter { band, coef })
    }

    /// The share of the logo's opacity (0 to 1.2) whose removal leaves the
    /// least step across its edges in `f`, which covers the logo's box; or
    /// NaN when the picture cannot tell whether the logo is there (a white
    /// logo on white).
    pub fn depth(&self, f: &Frame) -> f32 {
        if self.showing(f) < SHOWING_MIN {
            return f32::NAN;
        }
        let k = LOGO_MAX_DP as f64;
        let planes = [&f.y, &f.cb, &f.cr];
        let energy = |m: f64| -> f64 {
            let erased = |p: usize, i: usize| {
                let (dp, l) = self.coef[p][i];
                let d = (dp * m).min(k - 1.0);
                (planes[p][i] as f64 * k - l * d) / (k - d)
            };
            self.band.iter().map(|&(p, i, j)| (erased(p as usize, i as usize) - erased(p as usize, j as usize)).abs()).sum()
        };
        let mut best = (f64::MAX, 0.0);
        for s in 0..=24 {
            let m = s as f64 * 0.05;
            let e = energy(m);
            if e < best.0 {
                best = (e, m);
            }
        }
        let c = best.1;
        for s in -4..=4 {
            let m = c + s as f64 * 0.01;
            if s != 0 && (0.0..=1.2).contains(&m) {
                let e = energy(m);
                if e < best.0 {
                    best = (e, m);
                }
            }
        }
        best.1 as f32
    }

    /// How much the logo would show on this picture, in 8-bit levels:
    /// the mean over its edges of opacity times how far its colour lies
    /// from the picture's. A white logo on white shows nothing, whether
    /// it is there or not.
    pub fn showing(&self, f: &Frame) -> f32 {
        let planes = [&f.y, &f.cb, &f.cr];
        let k = LOGO_MAX_DP as f64;
        let mut sum = 0.0;
        for &(p, i, j) in &self.band {
            for i in [i as usize, j as usize] {
                let (dp, l) = self.coef[p as usize][i];
                sum += dp / k * (l - planes[p as usize][i] as f64).abs();
            }
        }
        (sum / (2 * self.band.len()) as f64 / 16.0) as f32
    }
}

/// The depth of every frame of `path` in the logo's box, from `opt.start`,
/// and the number of the first of them, counted from the recording's first
/// picture. `progress` gets the share of the input read.
pub fn measure(
    path: &Path,
    info: &VideoInfo,
    logo: &Logo,
    opt: &ReadOptions,
    progress: &dyn Fn(f64),
    cancel: &AtomicBool,
) -> Result<(Vec<f32>, u64), String> {
    opt.check()?;
    let meter = Meter::new(logo)?;
    if logo.x < 0 || logo.y < 0 {
        return Err("ロゴの位置が画面の外にあります".into());
    }
    let rect = Rect { x: logo.x as u32, y: logo.y as u32, w: logo.w as u32, h: logo.h as u32 };
    let offset = match opt.start {
        None => 0,
        // Counted from the first picture that decodes, as AviSynth does
        // (by pictures this is only as near as the clock gets).
        Some(s) => source::clock_frame(info, source::first_picture(path).map_err(|e| e.to_string())?, s),
    };
    let mut reader = Reader::open(path, info, rect, opt).map_err(|e| e.to_string())?;
    let threads = (opt.threads as usize).max(1);
    let total = opt.duration.unwrap_or(info.duration - opt.start.unwrap_or(0.0)) * info.frame_rate;
    let mut depths = Vec::new();
    let mut batch = Vec::with_capacity(threads * 32);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("中止しました".into());
        }
        let frame = reader.next_frame().map_err(|e| e.to_string())?;
        let done = frame.is_none();
        if let Some(f) = frame {
            batch.push(f);
        }
        if batch.len() == batch.capacity() || (done && !batch.is_empty()) {
            let size = batch.len().div_ceil(threads);
            let parts: Vec<Vec<f32>> = std::thread::scope(|s| {
                let hs: Vec<_> = batch.chunks(size).map(|c| s.spawn(|| c.iter().map(|f| meter.depth(f)).collect())).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            depths.extend(parts.into_iter().flatten());
            batch.clear();
            if total > 0.0 {
                progress((depths.len() as f64 / total).min(1.0));
            }
        }
        if done {
            break;
        }
    }
    Ok((depths, offset))
}

/// The middle of the values that are not NaN, if any.
fn median(v: &[f32]) -> Option<f32> {
    let mut s: Vec<f32> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    s.sort_by(|a, b| a.total_cmp(b));
    s.get(s.len() / 2).copied()
}

/// EraseLOGO's fade at frame `u` of a stretch starting at `start` and
/// fading in over `fadein`.
fn fade_in_at(u: i64, start: i64, fadein: i64) -> f64 {
    if u < start {
        0.0
    } else if u < start + fadein {
        ((u - start) * 2 + 1) as f64 / (fadein * 2) as f64
    } else {
        1.0
    }
}

/// For each fade length up to `longest`: the start that best fits `d`
/// (depths divided by the level the logo holds at, the first at frame
/// `lo`), and how far off it is.
fn fit_in(d: &[f64], lo: i64, longest: i64) -> Vec<(f64, i64)> {
    let n = d.len() as i64;
    (0..=longest)
        .map(|fi| {
            let mut best = (f64::MAX, lo);
            for s in 0..n {
                let sse: f64 = d
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| !v.is_nan())
                    .map(|(k, &v)| (v - fade_in_at(k as i64, s, fi)).powi(2))
                    .sum();
                if sse < best.0 {
                    best = (sse, lo + s);
                }
            }
            best
        })
        .collect()
}

/// The fade length that fits all the fades together: a station fades its
/// logo the same way each time, and one fade measured alone is thrown by
/// a picture that looks like the logo.
fn common_length(fits: &[Vec<(f64, i64)>]) -> usize {
    let Some(first) = fits.first() else { return 0 };
    (0..first.len())
        .min_by(|&a, &b| {
            let sum = |fi: usize| fits.iter().map(|f| f[fi].0).sum::<f64>();
            sum(a).total_cmp(&sum(b))
        })
        .unwrap_or(0)
}

/// The stretches the logo is on screen in, from the depths of each frame.
pub fn find(depths: &[f32], frame_rate: f64) -> Vec<Span> {
    let n = depths.len();
    if n == 0 {
        return Vec::new();
    }
    let fps = if frame_rate > 0.0 { frame_rate } else { 30000.0 / 1001.0 };
    let frames = |sec: f64| (sec * fps).round() as usize;
    // Shown where the middle value of about a second says so. Where the
    // pictures cannot tell, as it was before.
    let half = frames(0.5);
    let said: Vec<Option<bool>> = (0..n)
        .map(|i| {
            let w = &depths[i.saturating_sub(half)..(i + half + 1).min(n)];
            let known = w.iter().filter(|v| !v.is_nan()).count();
            if known * 2 < w.len() {
                return None;
            }
            median(w).map(|m| m >= 0.5)
        })
        .collect();
    let Some(first) = said.iter().flatten().next().copied() else { return Vec::new() };
    let on: Vec<bool> = said
        .iter()
        .scan(first, |last, s| {
            *last = s.unwrap_or(*last);
            Some(*last)
        })
        .collect();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        let j = (i..n).find(|&j| on[j] != on[i]).unwrap_or(n);
        if on[i] {
            match runs.last_mut() {
                Some(last) if i - last.1 < frames(GAP_SEC) => last.1 = j,
                _ => runs.push((i, j)),
            }
        }
        i = j;
    }
    runs.retain(|&(a, b)| b - a >= frames(SPAN_MIN_SEC));

    let reach = frames(FADE_MAX_SEC);
    // Each fade in and out, fitted for every length.
    let mut ins = Vec::new();
    let mut outs = Vec::new();
    for (k, &(a, b)) in runs.iter().enumerate() {
        let level = median(&depths[a..b]).unwrap_or(1.0).max(0.5) as f64;
        let scaled = |r: std::ops::Range<usize>| depths[r].iter().map(|&v| v as f64 / level).collect::<Vec<f64>>();
        // A fade the pictures cannot see at all (white behind a white
        // logo) fits any start equally: none is fitted, and the stretch
        // starts or ends where it was seen.
        let seen = |d: &[f64]| d.iter().any(|v| !v.is_nan());
        // The fade in: between the stretch before and the middle of this one.
        ins.push((a > 0).then(|| {
            let floor = if k == 0 { 0 } else { runs[k - 1].1 };
            let lo = a.saturating_sub(reach).max(floor);
            let hi = (a + reach).min((a + b) / 2);
            let d = scaled(lo..hi);
            seen(&d).then(|| fit_in(&d, lo as i64, reach as i64))
        }));
        // The fade out: the same, read backwards from the end.
        outs.push((b < n).then(|| {
            let ceil = runs.get(k + 1).map_or(n, |r| r.0);
            let lo = b.saturating_sub(reach).max((a + b) / 2);
            let hi = (b + reach).min(ceil);
            let mut d = scaled(lo..hi);
            d.reverse();
            seen(&d).then(|| fit_in(&d, 0, reach as i64).into_iter().map(|(e, s)| (e, hi as i64 - 1 - s)).collect::<Vec<_>>())
        }));
    }
    let fadein = common_length(&ins.iter().flatten().flatten().cloned().collect::<Vec<_>>());
    let fadeout = common_length(&outs.iter().flatten().flatten().cloned().collect::<Vec<_>>());
    let mut spans: Vec<Span> = Vec::new();
    for (k, &(a, b)) in runs.iter().enumerate() {
        let (start, fi) = match &ins[k] {
            None => (0, 0),
            Some(None) => (a as u64, 0),
            Some(Some(f)) => (f[fadein].1 as u64, fadein as u64),
        };
        let (end, fo) = match &outs[k] {
            None => (n as u64 - 1, 0),
            Some(None) => (b as u64 - 1, 0),
            Some(Some(f)) => (f[fadeout].1 as u64, fadeout as u64),
        };
        if end <= start {
            continue;
        }
        let mut span = Span { start, end, fadein: fi, fadeout: fo };
        // Fades fitted from both sides of a short gap can cross: the frames
        // between are split, so none is erased twice.
        if let Some(last) = spans.last_mut() {
            if last.end >= span.start {
                let mid = (last.end + span.start) / 2;
                last.end = mid.max(last.start);
                span.start = (mid + 1).max(last.end + 1);
                if span.end <= span.start {
                    continue;
                }
            }
        }
        spans.push(span);
    }
    spans
}

/// One EraseLOGO call per stretch, chained, with the frames moved on by
/// `offset` (the first frame read, counted from the recording's first).
pub fn erase_call(lgd: &str, spans: &[Span], offset: u64, interlaced: bool) -> String {
    spans
        .iter()
        .map(|s| {
            let mut c = format!("EraseLOGO(logofile=\"{lgd}\", start={}, end={}", s.start + offset, s.end + offset);
            if s.fadein > 0 {
                c.push_str(&format!(", fadein={}", s.fadein));
            }
            if s.fadeout > 0 {
                c.push_str(&format!(", fadeout={}", s.fadeout));
            }
            c.push_str(&format!(", interlaced={interlaced})"));
            c
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Depths that follow EraseLOGO's fades, with a scene the logo cannot
    /// be seen in partway through.
    fn trace(n: usize, spans: &[Span], level: f32) -> Vec<f32> {
        (0..n as i64)
            .map(|u| {
                let v = spans.iter().map(|s| {
                    let (st, en) = (s.start as i64, s.end as i64);
                    if u < st || u > en {
                        0.0
                    } else {
                        fade_in_at(u, st, s.fadein as i64).min(fade_in_at(en - u, 0, s.fadeout as i64))
                    }
                });
                v.fold(0.0, f64::max) as f32 * level
            })
            .collect()
    }

    #[test]
    fn finds_fades_and_bridges_a_short_gap() {
        let want = [Span { start: 600, end: 23000, fadein: 21, fadeout: 28 }, Span { start: 25600, end: 42800, fadein: 21, fadeout: 28 }];
        let mut d = trace(45000, &want, 0.95);
        for v in &mut d[10000..10060] {
            *v = 0.0;
        }
        assert_eq!(find(&d, 30000.0 / 1001.0), want);
    }

    #[test]
    fn a_white_scene_does_not_break_the_logo() {
        let want = [Span { start: 300, end: 8999, fadein: 0, fadeout: 0 }];
        let mut d = trace(9000, &want, 1.0);
        for v in &mut d[2000..2600] {
            *v = f32::NAN;
        }
        // The cuts into and out of it read as no logo.
        d[1999] = 0.0;
        d[2600] = 0.0;
        assert_eq!(find(&d, 29.97), want);
    }

    #[test]
    fn a_logo_from_the_first_frame_has_no_fade_in() {
        let want = [Span { start: 0, end: 8999, fadein: 0, fadeout: 0 }];
        let d = trace(9000, &want, 1.0);
        assert_eq!(find(&d, 29.97), want);
    }

    #[test]
    fn calls_are_chained_and_moved_on() {
        let s = [Span { start: 0, end: 99, fadein: 0, fadeout: 28 }, Span { start: 200, end: 299, fadein: 21, fadeout: 0 }];
        assert_eq!(
            erase_call("a.lgd", &s, 10, true),
            "EraseLOGO(logofile=\"a.lgd\", start=10, end=109, fadeout=28, interlaced=true).\
             EraseLOGO(logofile=\"a.lgd\", start=210, end=309, fadein=21, interlaced=true)"
        );
    }
}
