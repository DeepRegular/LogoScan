//! The logo analysis itself.
//!
//! A logo is alpha-blended over the picture: `obs = bg * (1 - a) + logo * a`.
//! For every pixel and plane we collect (background, observed) pairs from the
//! frames whose background is known, fit `obs = A * bg + B`, and recover
//! `a = 1 - A`, `logo = B / (1 - A)` -- the same quantities logoscan writes.
//!
//! The background of a frame is read from the one-pixel ring around the
//! rectangle: a frame is used only when that ring is flat (or, with
//! `Background::Plane`, close to a linear gradient).

use crate::lgd::{LogoPixel, LOGO_MAX_DP};
use crate::source::Frame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// One colour for the whole rectangle, the mean of the ring (logoscan).
    Flat,
    /// A linear gradient fitted to the ring.
    Plane,
}

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub background: Background,
    /// Largest spread allowed on the ring, in PIXEL_YC units.
    pub threshold_y: f64,
    pub threshold_c: f64,
    /// Frames kept for the fit; beyond this a uniform random subset is kept.
    pub max_frames: usize,
    /// Least-squares passes; each one after the first drops outliers.
    pub robust_passes: u32,
}

/// Background model of one plane of one frame: bg(u, v) = a + b*u + c*v.
#[derive(Clone, Copy, Debug, Default)]
struct Bg {
    a: f32,
    b: f32,
    c: f32,
}

struct Sample {
    frame: Frame,
    bg: [Bg; 3],
}

pub struct Scanner {
    w: usize,
    h: usize,
    p: Params,
    ring: Vec<usize>,
    samples: Vec<Sample>,
    pub seen: u64,
    pub accepted: u64,
    rng: u64,
}

pub struct Report {
    pub pixels: Vec<LogoPixel>,
    pub frames_used: usize,
    pub frames_without_logo: usize,
}

impl Scanner {
    pub fn new(w: usize, h: usize, p: Params) -> Scanner {
        let mut ring = Vec::new();
        for x in 0..w {
            ring.push(x);
            if h > 1 {
                ring.push((h - 1) * w + x);
            }
        }
        for y in 1..h.saturating_sub(1) {
            ring.push(y * w);
            if w > 1 {
                ring.push(y * w + w - 1);
            }
        }
        Scanner { w, h, p, ring, samples: Vec::new(), seen: 0, accepted: 0, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Fits the background of one plane on the ring, or None if it is not
    /// smooth enough to stand in for the hidden pixels.
    fn background(&self, plane: &[i16], threshold: f64) -> Option<Bg> {
        let n = self.ring.len() as f64;
        let (cu, cv) = ((self.w as f64 - 1.0) / 2.0, (self.h as f64 - 1.0) / 2.0);
        let bg = match self.p.background {
            Background::Flat => {
                let sum: f64 = self.ring.iter().map(|&i| plane[i] as f64).sum();
                Bg { a: (sum / n) as f32, b: 0.0, c: 0.0 }
            }
            Background::Plane => {
                // Centred coordinates make the normal equations nearly diagonal.
                let (mut s1, mut su, mut sv, mut suu, mut svv, mut suv) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                let (mut sz, mut szu, mut szv) = (0.0, 0.0, 0.0);
                for &i in &self.ring {
                    let u = (i % self.w) as f64 - cu;
                    let v = (i / self.w) as f64 - cv;
                    let z = plane[i] as f64;
                    s1 += 1.0;
                    su += u;
                    sv += v;
                    suu += u * u;
                    svv += v * v;
                    suv += u * v;
                    sz += z;
                    szu += z * u;
                    szv += z * v;
                }
                let m = [[s1, su, sv], [su, suu, suv], [sv, suv, svv]];
                let [a, b, c] = solve3(m, [sz, szu, szv])?;
                // Store relative to the top-left corner.
                Bg { a: (a - b * cu - c * cv) as f32, b: b as f32, c: c as f32 }
            }
        };
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for &i in &self.ring {
            let u = (i % self.w) as f64;
            let v = (i / self.w) as f64;
            let r = plane[i] as f64 - bg.at(u, v);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        (hi - lo <= threshold).then_some(bg)
    }

    pub fn push(&mut self, frame: Frame) {
        self.seen += 1;
        let Some(by) = self.background(&frame.y, self.p.threshold_y) else { return };
        let Some(bcb) = self.background(&frame.cb, self.p.threshold_c) else { return };
        let Some(bcr) = self.background(&frame.cr, self.p.threshold_c) else { return };
        self.accepted += 1;
        let s = Sample { frame, bg: [by, bcb, bcr] };
        if self.samples.len() < self.p.max_frames {
            self.samples.push(s);
        } else {
            // Reservoir sampling keeps a uniform subset of all accepted frames.
            let j = (self.rand() % self.accepted) as usize;
            if j < self.samples.len() {
                self.samples[j] = s;
            }
        }
    }

    /// Writes "bg obs" pairs of every plane of one pixel, for inspection.
    pub fn dump_pixel(&self, x: usize, y: usize, out: &mut impl std::io::Write) -> std::io::Result<()> {
        let i = y * self.w + x;
        for s in &self.samples {
            let (u, v) = (x as f64, y as f64);
            writeln!(
                out,
                "{:.1} {} {:.1} {} {:.1} {}",
                s.bg[0].at(u, v),
                s.frame.y[i],
                s.bg[1].at(u, v),
                s.frame.cb[i],
                s.bg[2].at(u, v),
                s.frame.cr[i]
            )?;
        }
        Ok(())
    }

    /// Fits every pixel and plane on the frames marked in `use_frame`;
    /// `None` where the data cannot determine a line.
    fn fit_all(&self, use_frame: &[bool], threads: usize) -> Vec<[Option<(f64, f64)>; 3]> {
        let n = self.w * self.h;
        let mut lines = vec![[None; 3]; n];
        let chunk = n.div_ceil(threads.max(1));
        std::thread::scope(|s| {
            for (ci, out) in lines.chunks_mut(chunk).enumerate() {
                s.spawn(move || {
                    let mut xs = Vec::new();
                    let mut ys = Vec::new();
                    let mut keep = Vec::new();
                    for (k, px) in out.iter_mut().enumerate() {
                        let i = ci * chunk + k;
                        let (u, v) = ((i % self.w) as f64, (i / self.w) as f64);
                        for (pl, r) in px.iter_mut().enumerate() {
                            xs.clear();
                            ys.clear();
                            for smp in self.samples.iter().zip(use_frame).filter(|(_, u)| **u).map(|(s, _)| s) {
                                xs.push(smp.bg[pl].at(u, v));
                                ys.push(smp.frame.plane(pl)[i] as f64);
                            }
                            *r = fit(&xs, &ys, self.p.robust_passes, &mut keep);
                        }
                    }
                });
            }
        });
        lines
    }

    /// Marks the frames in which the logo is shown: on the pixels where the
    /// logo is strongest, the fitted blend must explain the frame better
    /// than the bare background does.
    fn frames_with_logo(&self, lines: &[[Option<(f64, f64)>; 3]]) -> Option<Vec<bool>> {
        let alpha: Vec<f64> = lines.iter().map(|l| l[0].map_or(0.0, |(a, _)| 1.0 - a)).collect();
        let mut sorted: Vec<f64> = alpha.iter().copied().filter(|a| a.is_finite()).collect();
        sorted.sort_by(|p, q| p.total_cmp(q));
        let top = *sorted.get(sorted.len() * 99 / 100)?;
        if top < 0.02 {
            return None;
        }
        let mask: Vec<usize> = (0..alpha.len()).filter(|&i| alpha[i] >= top * 0.5).collect();
        let keep = self
            .samples
            .iter()
            .map(|s| {
                let (mut with, mut without) = (0.0, 0.0);
                for &i in &mask {
                    let (a, b) = lines[i][0].unwrap();
                    let x = s.bg[0].at((i % self.w) as f64, (i / self.w) as f64);
                    let y = s.frame.y[i] as f64;
                    with += (y - (a * x + b)).powi(2);
                    without += (y - x).powi(2);
                }
                with <= without
            })
            .collect();
        Some(keep)
    }

    pub fn finish(&self, threads: usize) -> Report {
        let n = self.w * self.h;
        let mut use_frame = vec![true; self.samples.len()];
        let mut lines = self.fit_all(&use_frame, threads);
        // Frames without the logo (CMs, fades, black between scenes) pull the
        // fit toward "no logo"; drop them and fit again until it settles.
        for _ in 0..4 {
            let Some(keep) = self.frames_with_logo(&lines) else { break };
            if keep == use_frame || keep.iter().filter(|k| **k).count() < 2 {
                break;
            }
            use_frame = keep;
            lines = self.fit_all(&use_frame, threads);
        }
        let mut pixels = vec![LogoPixel::default(); n];
        for (px, l) in pixels.iter_mut().zip(&lines) {
            let r = l.map(|f| f.map_or((0, 0), |(a, b)| to_lgd(a, b)));
            *px = LogoPixel { dp_y: r[0].0, y: r[0].1, dp_cb: r[1].0, cb: r[1].1, dp_cr: r[2].0, cr: r[2].1 };
        }
        Report { pixels, frames_used: use_frame.iter().filter(|k| **k).count(), frames_without_logo: use_frame.iter().filter(|k| !**k).count() }
    }
}

impl Frame {
    fn plane(&self, p: usize) -> &[i16] {
        match p {
            0 => &self.y,
            1 => &self.cb,
            _ => &self.cr,
        }
    }
}

impl Bg {
    fn at(&self, u: f64, v: f64) -> f64 {
        self.a as f64 + self.b as f64 * u + self.c as f64 * v
    }
}

fn solve3(m: [[f64; 3]; 3], r: [f64; 3]) -> Option<[f64; 3]> {
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-9 {
        return None;
    }
    let mut out = [0.0; 3];
    for (k, o) in out.iter_mut().enumerate() {
        let mut mk = m;
        for row in 0..3 {
            mk[row][k] = r[row];
        }
        *o = det(mk) / d;
    }
    Some(out)
}

/// Least squares `y = A x + B`; later passes refit on the samples within
/// three robust standard deviations of the previous line.
fn fit(xs: &[f64], ys: &[f64], passes: u32, keep: &mut Vec<bool>) -> Option<(f64, f64)> {
    keep.clear();
    keep.resize(xs.len(), true);
    let mut line: Option<(f64, f64)> = None;
    let mut res: Vec<f64> = Vec::with_capacity(xs.len());
    for pass in 0..passes.max(1) {
        if let Some((a, b)) = line {
            res.clear();
            res.extend(xs.iter().zip(ys).map(|(x, y)| (y - (a * x + b)).abs()));
            let mut sorted = res.clone();
            let mid = sorted.len() / 2;
            let (_, med, _) = sorted.select_nth_unstable_by(mid, |p, q| p.total_cmp(q));
            // Floor the scale so clean pixels do not shed honest samples.
            let sigma = (1.4826 * *med).max(16.0);
            for (k, r) in keep.iter_mut().zip(&res) {
                *k = *r <= 3.0 * sigma;
            }
        }
        let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for ((x, y), k) in xs.iter().zip(ys).zip(keep.iter()) {
            if *k {
                n += 1.0;
                sx += x;
                sy += y;
                sxx += x * x;
                sxy += x * y;
            }
        }
        let den = n * sxx - sx * sx;
        if n < 2.0 || den.abs() < 1e-6 * n * n {
            return line;
        }
        let a = (n * sxy - sx * sy) / den;
        let b = (sxx * sy - sx * sxy) / den;
        line = Some((a, b));
        if pass == 0 && passes <= 1 {
            break;
        }
    }
    line
}

/// logoscan's conversion: dp = (1-A)*1000, colour = B/(1-A); a pixel whose
/// dp rounds to zero or whose colour overflows is stored as (0, 0).
pub(crate) fn to_lgd(a: f64, b: f64) -> (i16, i16) {
    let alpha = 1.0 - a;
    if alpha == 0.0 {
        return (0, 0);
    }
    // logoscan rounds with (short)(v + 0.5), which truncates toward zero.
    let colour = (b / alpha + 0.5).trunc();
    if colour.abs() >= 0x7FFF as f64 {
        return (0, 0);
    }
    let dp = (alpha * LOGO_MAX_DP as f64 + 0.5).trunc();
    if dp.abs() > 0x3FFF as f64 || dp == 0.0 {
        return (0, 0);
    }
    (dp as i16, colour as i16)
}
