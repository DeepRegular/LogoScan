//! Reads a rectangle of every frame through the ffmpeg command, converted to
//! AviUtl's PIXEL_YC scale.
//!
//! ffmpeg only crops; chroma is upsampled here the way AviUtl sees it, since
//! the chroma of the logo is only as good as that interpolation:
//! horizontally the samples sit on the even pixels and the odd pixels take the
//! mean of their neighbours (AviUtl's YUY2 -> YC48), vertically 4:2:0 is
//! interpolated within each field for interlaced material.

use std::io::{self, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

/// A command for `ffmpeg` or `ffprobe`: a copy next to our own executable or
/// in an `ffmpeg` folder beside it (as the Windows package ships it), else
/// whatever PATH finds. On Windows it never opens a console window.
pub fn tool(name: &str) -> Command {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let beside = dir.and_then(|d| [d.join(&exe), d.join("ffmpeg").join(&exe)].into_iter().find(|p| p.is_file()));
    #[allow(unused_mut)]
    let mut cmd = Command::new(beside.unwrap_or_else(|| exe.into()));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    /// Seconds; 0 when the container does not say.
    pub duration: f64,
    pub pix_fmt: String,
    pub interlaced: bool,
}

pub fn probe(path: &Path) -> io::Result<VideoInfo> {
    let out = tool("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate,r_frame_rate,pix_fmt,field_order:format=duration",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| io::Error::other(format!("cannot run ffprobe (put ffmpeg and ffprobe on PATH or next to this program): {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut info = VideoInfo { width: 0, height: 0, frame_rate: 0.0, duration: 0.0, pix_fmt: String::new(), interlaced: false };
    let rate = |s: &str| -> f64 {
        let mut it = s.split('/');
        let n: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let d: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
        if d > 0.0 { n / d } else { 0.0 }
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "width" => info.width = v.parse().unwrap_or(0),
            "height" => info.height = v.parse().unwrap_or(0),
            "r_frame_rate" if info.frame_rate == 0.0 => info.frame_rate = rate(v),
            "avg_frame_rate" if rate(v) > 0.0 => info.frame_rate = rate(v),
            "pix_fmt" => info.pix_fmt = v.to_string(),
            "duration" => info.duration = v.parse().unwrap_or(0.0),
            "field_order" => info.interlaced = matches!(v, "tt" | "bb" | "tb" | "bt"),
            _ => {}
        }
    }
    if info.width == 0 || info.height == 0 {
        return Err(io::Error::other(format!("ffprobe found no video stream in {}", path.display())));
    }
    Ok(info)
}

/// Seconds decoded and thrown away before a seek target, so the frame there
/// has its references (a seek into MPEG-2 can land past the I picture).
const PRE_ROLL: f64 = 2.0;

fn seek_args(at: f64) -> Vec<String> {
    let at = at.max(0.0);
    vec!["-ss".into(), format!("{:.3}", (at - PRE_ROLL).max(0.0))]
}

/// The matrix AviUtl would pick for this picture: BT.709 from 720 lines up.
pub fn is_hd(info: &VideoInfo) -> bool {
    info.height >= 720
}

/// One whole frame at `at` seconds as packed RGB, for display.
pub fn grab_rgb(path: &Path, info: &VideoInfo, at: f64) -> io::Result<Vec<u8>> {
    let matrix = if is_hd(info) { "bt709" } else { "bt601" };
    let out = tool("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(seek_args(at))
        .arg("-i")
        .arg(path)
        .args(["-ss", &format!("{:.3}", at.clamp(0.0, PRE_ROLL))])
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-sn", "-dn"])
        .args(["-vf", &format!("scale=in_color_matrix={matrix}:in_range=tv:out_range=pc:flags=bilinear,format=rgb24")])
        .args(["-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| io::Error::other(format!("cannot run ffmpeg: {e}")))?;
    let want = (info.width * info.height * 3) as usize;
    if out.stdout.len() < want {
        return Err(io::Error::other(format!("no frame at {at:.3}s")));
    }
    let mut v = out.stdout;
    v.truncate(want);
    Ok(v)
}

/// Frame `n` (counted from `start`, or from the beginning) as packed RGB,
/// for display: picked by number, as the moving-logo analysis counts them.
pub fn grab_rgb_frame(path: &Path, info: &VideoInfo, start: Option<f64>, n: u64) -> io::Result<Vec<u8>> {
    let matrix = if is_hd(info) { "bt709" } else { "bt601" };
    let mut cmd = tool("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if let Some(s) = start {
        cmd.args(seek_args(s));
    }
    cmd.arg("-i").arg(path);
    if let Some(s) = start {
        cmd.args(["-ss", &format!("{:.3}", s.clamp(0.0, PRE_ROLL))]);
    }
    let vf = format!("select='eq(n\\,{n})',scale=in_color_matrix={matrix}:in_range=tv:out_range=pc:flags=bilinear,format=rgb24");
    let out = cmd
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", &vf, "-fps_mode", "passthrough", "-frames:v", "1"])
        .args(["-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| io::Error::other(format!("cannot run ffmpeg: {e}")))?;
    let want = (info.width * info.height * 3) as usize;
    if out.stdout.len() < want {
        return Err(io::Error::other(format!("no frame {n}")));
    }
    let mut v = out.stdout;
    v.truncate(want);
    Ok(v)
}

/// One frame of the rectangle, planar Y/Cb/Cr in PIXEL_YC units.
pub struct Frame {
    pub y: Vec<i16>,
    pub cb: Vec<i16>,
    pub cr: Vec<i16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    Auto,
    Progressive,
    Interlaced,
}

pub struct ReadOptions {
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub step: u32,
    pub threads: u32,
    pub scan: Scan,
}

pub struct Reader {
    child: Child,
    stdout: BufReader<ChildStdout>,
    rect: Rect,
    /// The area ffmpeg hands over: the rectangle plus a margin for the
    /// chroma interpolation, aligned so chroma rows keep their parity.
    area: Rect,
    /// Chroma subsampling shifts.
    sx: u32,
    sy: u32,
    /// 2 bytes per sample instead of 1.
    wide: bool,
    /// Value of one 8-bit level in the samples ffmpeg writes.
    unit: f64,
    interlaced: bool,
    buf: Vec<u8>,
}

impl Reader {
    pub fn open(path: &Path, info: &VideoInfo, rect: Rect, opt: &ReadOptions) -> io::Result<Reader> {
        if rect.w == 0 || rect.h == 0 || rect.x + rect.w > info.width || rect.y + rect.h > info.height {
            return Err(io::Error::other(format!(
                "rectangle {}x{}+{}+{} lies outside the {}x{} picture",
                rect.w, rect.h, rect.x, rect.y, info.width, info.height
            )));
        }
        let pf = info.pix_fmt.as_str();
        let (sx, sy) = if pf.starts_with("yuv420") || pf.starts_with("yuvj420") || pf.starts_with("nv12") || pf.starts_with("p010") {
            (1, 1)
        } else if pf.starts_with("yuv422") || pf.starts_with("yuvj422") {
            (1, 0)
        } else {
            (0, 0)
        };
        let high = pf.contains("10") || pf.contains("12") || pf.contains("16") || pf.starts_with("p0");
        let out_fmt = match (sx, sy, high) {
            (1, 1, false) => "yuv420p",
            (1, 0, false) => "yuv422p",
            (_, _, false) => "yuv444p",
            (1, 1, true) => "yuv420p16le",
            (1, 0, true) => "yuv422p16le",
            (_, _, true) => "yuv444p16le",
        };
        let (wide, unit) = if high { (true, 256.0) } else { (false, 1.0) };
        let interlaced = match opt.scan {
            Scan::Auto => info.interlaced && sy == 1,
            Scan::Progressive => false,
            Scan::Interlaced => sy == 1,
        };
        // Margin of two luma pixels each way; rows aligned to 4 so the field
        // parity of chroma rows survives the crop.
        let ax = (rect.x.saturating_sub(2)) & !1;
        let ay = (rect.y.saturating_sub(4)) & !3;
        let ar = (rect.x + rect.w + 2).min(info.width);
        let ab = (rect.y + rect.h + 4).min(info.height);
        let aw = ((ar - ax + 1) & !1).min(info.width - ax);
        let ah = ((ab - ay + 3) & !3).min(info.height - ay);
        let area = Rect { x: ax, y: ay, w: aw, h: ah };
        let mut vf = format!("crop={aw}:{ah}:{ax}:{ay}:exact=1,format={out_fmt}");
        if opt.step > 1 {
            vf = format!("select='not(mod(n\\,{}))',{vf}", opt.step);
        }
        let mut cmd = tool("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        cmd.args(["-threads", &opt.threads.to_string()]);
        if let Some(s) = opt.start {
            cmd.args(seek_args(s));
        }
        cmd.arg("-i").arg(path);
        if let Some(s) = opt.start {
            cmd.args(["-ss", &format!("{:.3}", s.clamp(0.0, PRE_ROLL))]);
        }
        if let Some(d) = opt.duration {
            cmd.args(["-t", &format!("{d:.3}")]);
        }
        cmd.args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", &vf, "-fps_mode", "passthrough"]);
        cmd.args(["-f", "rawvideo", "pipe:1"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| io::Error::other(format!("cannot run ffmpeg: {e}")))?;
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        let (cw, ch) = (aw.div_ceil(1 << sx), ah.div_ceil(1 << sy));
        let bytes = (aw * ah + 2 * cw * ch) as usize * if wide { 2 } else { 1 };
        Ok(Reader { child, stdout, rect, area, sx, sy, wide, unit, interlaced, buf: vec![0; bytes] })
    }

    pub fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        match self.stdout.read_exact(&mut self.buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let (aw, ah) = (self.area.w as usize, self.area.h as usize);
        let (cw, ch) = (aw.div_ceil(1 << self.sx), ah.div_ceil(1 << self.sy));
        let get = |i: usize| -> f64 {
            if self.wide {
                u16::from_le_bytes([self.buf[i * 2], self.buf[i * 2 + 1]]) as f64 / self.unit
            } else {
                self.buf[i] as f64
            }
        };
        let (ox, oy) = ((self.rect.x - self.area.x) as usize, (self.rect.y - self.area.y) as usize);
        let (w, h) = (self.rect.w as usize, self.rect.h as usize);
        // AviUtl's YUY2 -> PIXEL_YC: y = (Y-16)*4096/219, c = (C-128)*4096/224.
        let luma = |v: f64| ((v - 16.0) * (4096.0 / 219.0)).round() as i16;
        let chroma = |v: f64| ((v - 128.0) * (4096.0 / 224.0)).round() as i16;
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            for c in 0..w {
                y.push(luma(get((oy + r) * aw + ox + c)));
            }
        }
        let mut planes = [Vec::with_capacity(w * h), Vec::with_capacity(w * h)];
        for (p, out) in planes.iter_mut().enumerate() {
            let base = aw * ah + p * cw * ch;
            let at = |cx: usize, cy: usize| get(base + cy.min(ch - 1) * cw + cx.min(cw - 1));
            for r in 0..h {
                let ly = oy + r;
                // Vertical position as two chroma rows and a weight.
                let (r0, r1, t) = if self.sy == 0 {
                    (ly, ly, 0.0)
                } else if self.interlaced {
                    // Field f holds chroma rows 2k+f, sited a quarter (top)
                    // or three quarters (bottom) between its luma rows 2k, 2k+1.
                    let f = ly & 1;
                    let fl = (ly >> 1) as f64;
                    let pos = (fl - if f == 0 { 0.25 } else { 0.75 }) / 2.0;
                    let k0 = pos.floor().max(0.0);
                    let t = (pos - k0).clamp(0.0, 1.0);
                    let k0 = k0 as usize;
                    (2 * k0 + f, 2 * (k0 + 1) + f, t)
                } else {
                    // MPEG-2 progressive: chroma between luma rows 2k, 2k+1.
                    let pos = (ly as f64 - 0.5) / 2.0;
                    let k0 = pos.floor().max(0.0);
                    let t = (pos - k0).clamp(0.0, 1.0);
                    (k0 as usize, k0 as usize + 1, t)
                };
                let row = |cx: usize| at(cx, r0) * (1.0 - t) + at(cx, r1) * t;
                for c in 0..w {
                    let lx = ox + c;
                    let v = if self.sx == 0 {
                        row(lx)
                    } else if lx & 1 == 0 {
                        row(lx >> 1)
                    } else {
                        (row(lx >> 1) + row((lx >> 1) + 1)) / 2.0
                    };
                    out.push(chroma(v));
                }
            }
        }
        let [cb, cr] = planes;
        Ok(Some(Frame { y, cb, cr }))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
