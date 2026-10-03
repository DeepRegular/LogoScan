//! Applies a logo the way delogo removes it, and turns PIXEL_YC into RGB for
//! looking at the result.

use crate::lgd::{Logo, LOGO_MAX_DP};
use crate::source::{Frame, Rect};

/// Removes `logo` from `frame`, whose pixels cover `area` of the picture.
pub fn remove(logo: &Logo, frame: &mut Frame, area: Rect) {
    let (w, h) = (area.w as i64, area.h as i64);
    for r in 0..logo.h as i64 {
        for c in 0..logo.w as i64 {
            let (fx, fy) = (logo.x as i64 + c - area.x as i64, logo.y as i64 + r - area.y as i64);
            if fx < 0 || fy < 0 || fx >= w || fy >= h {
                continue;
            }
            let i = (fy * w + fx) as usize;
            let p = logo.pixels[(r * logo.w as i64 + c) as usize];
            for (plane, dp, v) in [(&mut frame.y, p.dp_y, p.y), (&mut frame.cb, p.dp_cb, p.cb), (&mut frame.cr, p.dp_cr, p.cr)] {
                let dp = (dp as i32).clamp(0, LOGO_MAX_DP - 1);
                // original = (observed - logo * a) / (1 - a)
                let o = (plane[i] as i32 * LOGO_MAX_DP - v as i32 * dp) / (LOGO_MAX_DP - dp);
                plane[i] = o.clamp(-32768, 32767) as i16;
            }
        }
    }
}

/// PIXEL_YC to full-range RGB with BT.709 (`hd`) or BT.601 coefficients.
pub fn yc_to_rgb(y: i16, cb: i16, cr: i16, hd: bool) -> [u8; 3] {
    let y = y as f64 / 4096.0;
    let (cb, cr) = (cb as f64 / 4096.0, cr as f64 / 4096.0);
    let (r, g, b) = if hd {
        (y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb)
    } else {
        (y + 1.402 * cr, y - 0.3441 * cb - 0.7141 * cr, y + 1.772 * cb)
    };
    [r, g, b].map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
}

pub fn frame_to_rgb(frame: &Frame, hd: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.y.len() * 3);
    for i in 0..frame.y.len() {
        out.extend(yc_to_rgb(frame.y[i], frame.cb[i], frame.cr[i], hd));
    }
    out
}
