use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use lgdscan::detect::{self, DetectOptions};
use lgdscan::job::{self, Job};
use lgdscan::scan::Background;
use lgdscan::source::{self, ReadOptions, Reader, Rect, Scan};
use lgdscan::{erase, lgd};

const USAGE: &str = "\
lgdscan - logo analysis compatible with the AviUtl logo plugin

usage:
  lgdscan scan INPUT... --rect X,Y,W,H [options]
      -o, --output FILE      .lgd to write (default: <name>.lgd)
      -n, --name NAME        logo name (default: output file stem)
      --start SEC            start of the range to read, per input
      --end SEC              end of the range to read, per input
      --step N               read every Nth frame (default 1)
      --threshold T          largest spread on the background ring, in
                             8-bit levels (default 12; chroma uses the same)
      --background plane|flat  plane: gradient fitted to the ring (default);
                             flat: one colour, as logoscan does
      --scan auto|progressive|interlaced
                             how 4:2:0 chroma is interpolated vertically
                             (default auto: from the stream's field order)
      --max-frames N         frames kept for the fit (default 8000)
      --passes N             least-squares passes, outliers dropped after
                             the first (default 3; 1 = plain least squares)
      --threads N            (default: all cores)
  lgdscan detect INPUT [--samples N] [--share S] [--margin M] [--start SEC] [--end SEC]
                             find logo positions; prints --rect candidates
      --samples N            keyframes spread over the input (default 120)
      --share S              share of frames with an edge (default 0.45)
      --margin M             pixels added around the logo (default 3)
  lgdscan info FILE.lgd
  lgdscan compare A.lgd B.lgd
  lgdscan render FILE.lgd OUT.pgm   dp_y as a greyscale picture
  lgdscan erase FILE.lgd VIDEO SEC OUT.ppm
                             the frame at SEC around the logo, before and
                             after removing it, side by side
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("scan") => cmd_scan(&args[1..]),
        Some("detect") => cmd_detect(&args[1..]),
        Some("info") if args.len() == 2 => cmd_info(Path::new(&args[1])),
        Some("compare") if args.len() == 3 => cmd_compare(Path::new(&args[1]), Path::new(&args[2])),
        Some("render") if args.len() == 3 => cmd_render(Path::new(&args[1]), Path::new(&args[2])),
        Some("erase") if args.len() == 5 => cmd_erase(Path::new(&args[1]), Path::new(&args[2]), &args[3], Path::new(&args[4])),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lgdscan: {e}");
            ExitCode::FAILURE
        }
    }
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn cmd_scan(args: &[String]) -> Res {
    let mut inputs = Vec::new();
    let mut rect = None;
    let mut output: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let (mut start, mut end) = (None, None);
    let mut step = 1u32;
    let mut threshold = 12.0f64;
    let mut background = Background::Plane;
    let mut max_frames = 8000usize;
    let mut passes = 3u32;
    let mut scan_mode = Scan::Auto;
    let mut threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--rect" => {
                let v: Vec<u32> = val()?.split(',').map(|s| s.trim().parse()).collect::<Result<_, _>>()?;
                let [x, y, w, h] = v[..] else { return Err("--rect takes X,Y,W,H".into()) };
                rect = Some(Rect { x, y, w, h });
            }
            "-o" | "--output" => output = Some(PathBuf::from(val()?)),
            "-n" | "--name" => name = Some(val()?.clone()),
            "--start" => start = Some(val()?.parse::<f64>()?),
            "--end" => end = Some(val()?.parse::<f64>()?),
            "--step" => step = val()?.parse::<u32>()?.max(1),
            "--threshold" => threshold = val()?.parse()?,
            "--background" => {
                background = match val()?.as_str() {
                    "flat" => Background::Flat,
                    "plane" => Background::Plane,
                    v => return Err(format!("unknown background {v}").into()),
                }
            }
            "--scan" => {
                scan_mode = match val()?.as_str() {
                    "auto" => Scan::Auto,
                    "progressive" => Scan::Progressive,
                    "interlaced" => Scan::Interlaced,
                    v => return Err(format!("unknown scan {v}").into()),
                }
            }
            "--max-frames" => max_frames = val()?.parse()?,
            "--passes" => passes = val()?.parse()?,
            "--threads" => threads = val()?.parse::<usize>()?.max(1),
            s if s.starts_with('-') => return Err(format!("unknown option {s}").into()),
            s => inputs.push(PathBuf::from(s)),
        }
    }
    let rect = rect.ok_or("--rect is required")?;
    let output = output.unwrap_or_else(|| PathBuf::from(format!("{}.lgd", name.as_deref().unwrap_or("logo"))));
    let name = name.unwrap_or_else(|| output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    lgd::encode_name(&name).map_err(|bad| format!("the name has characters CP932 cannot hold: {bad}"))?;
    let mut job = Job::new(inputs, rect);
    job.start = start;
    job.end = end;
    job.step = step;
    job.threshold = threshold;
    job.background = background;
    job.scan = scan_mode;
    job.max_frames = max_frames;
    job.passes = passes;
    job.threads = threads;
    let t0 = Instant::now();
    let mut last = usize::MAX;
    let out = job::run(
        &job,
        &mut |p| {
            if p.input != last && last != usize::MAX {
                eprintln!();
            }
            last = p.input;
            if p.fitting {
                eprint!("\r  {} frames read, {} usable; fitting...          ", p.seen, p.accepted);
            } else {
                eprint!("\r  input {}/{}: {:3.0}%  {} frames read, {} usable", p.input + 1, p.inputs, p.fraction * 100.0, p.seen, p.accepted);
            }
        },
        &AtomicBool::new(false),
    )?;
    eprintln!();
    if out.peak < 50 {
        eprintln!(
            "warning: no logo found (largest dp_y {}); the usable frames may all lack the logo or show it on its own colour",
            out.peak
        );
    }
    eprintln!(
        "fitted {} frames ({} more set aside as showing no logo) in {:.1}s",
        out.frames_used,
        out.frames_without_logo,
        t0.elapsed().as_secs_f64()
    );
    let bytes = lgd::encode_name(&name).map_err(|bad| format!("the name has characters CP932 cannot hold: {bad}"))?;
    if bytes.len() > lgd::NAME_MAX_V1 {
        eprintln!("warning: the name is cut to \"{}\" ({} bytes at most)", lgd::decode_name(lgd::stored_name(&bytes)), lgd::NAME_MAX_V1);
    }
    let logo = out.logo(bytes, rect);
    let mut f = BufWriter::new(File::create(&output)?);
    lgd::write(&mut f, &[logo])?;
    f.flush()?;
    eprintln!("wrote {}", output.display());
    Ok(())
}

fn cmd_detect(args: &[String]) -> Res {
    let mut input = None;
    let mut samples = 120u32;
    let mut share = 0.45f32;
    let mut margin = 3u32;
    let (mut start, mut end) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--samples" => samples = val()?.parse()?,
            "--share" => share = val()?.parse()?,
            "--margin" => margin = val()?.parse()?,
            "--start" => start = Some(val()?.parse::<f64>()?),
            "--end" => end = Some(val()?.parse::<f64>()?),
            s if s.starts_with('-') => return Err(format!("unknown option {s}").into()),
            s => input = Some(PathBuf::from(s)),
        }
    }
    let input = input.ok_or("no input")?;
    let info = source::probe(&input)?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let t0 = Instant::now();
    let d = detect::measure(
        &input,
        &info,
        &DetectOptions { samples, start, end, threads },
        &|f| eprint!("\r  {:3.0}%", f * 100.0),
        &AtomicBool::new(false),
    )?;
    eprintln!("\r{} keyframes in {:.1}s", d.frames, t0.elapsed().as_secs_f64());
    for c in d.candidates(share, margin) {
        let r = c.rect;
        println!("--rect {},{},{},{}   ({} edge pixels, score {:.0})", r.x, r.y, r.w, r.h, c.pixels, c.score);
    }
    Ok(())
}

fn load(path: &Path) -> Result<Vec<lgd::Logo>, Box<dyn std::error::Error>> {
    Ok(lgd::read(BufReader::new(File::open(path).map_err(|e| format!("{}: {e}", path.display()))?))?)
}

fn cmd_info(path: &Path) -> Res {
    for (i, l) in load(path)?.iter().enumerate() {
        let strong = l.pixels.iter().filter(|p| p.dp_y > 100).count();
        let max = l.pixels.iter().map(|p| p.dp_y).max().unwrap_or(0);
        println!(
            "#{i} \"{}\" x={} y={} w={} h={} fi={} fo={} st={} ed={}  max dp_y={max}  pixels with dp_y>100: {strong}",
            l.name_lossy(), l.x, l.y, l.w, l.h, l.fi, l.fo, l.st, l.ed
        );
    }
    Ok(())
}

fn cmd_compare(a: &Path, b: &Path) -> Res {
    let la = load(a)?;
    let lb = load(b)?;
    let (Some(a), Some(b)) = (la.first(), lb.first()) else { return Err("empty file".into()) };
    if (a.x, a.y, a.w, a.h) != (b.x, b.y, b.w, b.h) {
        println!("rectangles differ: {}x{}+{}+{} vs {}x{}+{}+{}", a.w, a.h, a.x, a.y, b.w, b.h, b.x, b.y);
        return Ok(());
    }
    type Get = fn(&lgd::LogoPixel) -> (i16, i16);
    let planes: [(&str, Get); 3] = [
        ("Y ", |p| (p.dp_y, p.y)),
        ("Cb", |p| (p.dp_cb, p.cb)),
        ("Cr", |p| (p.dp_cr, p.cr)),
    ];
    for (label, get) in planes {
        let pairs: Vec<((i16, i16), (i16, i16))> = a.pixels.iter().zip(&b.pixels).map(|(p, q)| (get(p), get(q))).collect();
        let dps: Vec<(f64, f64)> = pairs.iter().map(|(p, q)| (p.0 as f64, q.0 as f64)).collect();
        let n = dps.len() as f64;
        let (ma, mb) = (dps.iter().map(|d| d.0).sum::<f64>() / n, dps.iter().map(|d| d.1).sum::<f64>() / n);
        let (mut sab, mut saa, mut sbb, mut mad) = (0.0, 0.0, 0.0, 0.0f64);
        for (x, y) in &dps {
            sab += (x - ma) * (y - mb);
            saa += (x - ma) * (x - ma);
            sbb += (y - mb) * (y - mb);
            mad += (x - y).abs();
        }
        let corr = sab / (saa * sbb).sqrt();
        // Colour only means something where the logo is reasonably opaque.
        let strong: Vec<f64> = pairs
            .iter()
            .filter(|(p, q)| p.0 > 100 && q.0 > 100)
            .map(|(p, q)| (p.1 as f64 - q.1 as f64).abs())
            .collect();
        let cdiff = if strong.is_empty() { f64::NAN } else { strong.iter().sum::<f64>() / strong.len() as f64 };
        println!(
            "{label}: dp corr {corr:.4}  mean|ddp| {:.2}  mean dp {ma:.1} vs {mb:.1}  mean|dcolour| (dp>100, {} px) {cdiff:.1}",
            mad / n,
            strong.len()
        );
    }
    Ok(())
}

fn cmd_render(path: &Path, out: &Path) -> Res {
    let l = load(path)?.into_iter().next().ok_or("empty file")?;
    let mut f = BufWriter::new(File::create(out)?);
    write!(f, "P5\n{} {}\n255\n", l.w, l.h)?;
    let max = l.pixels.iter().map(|p| p.dp_y).max().unwrap_or(1).max(1) as f64;
    let bytes: Vec<u8> = l.pixels.iter().map(|p| (p.dp_y.max(0) as f64 / max * 255.0).round() as u8).collect();
    f.write_all(&bytes)?;
    Ok(())
}

fn cmd_erase(lgd_path: &Path, video: &Path, at: &str, out: &Path) -> Res {
    let l = load(lgd_path)?.into_iter().next().ok_or("empty file")?;
    let info = source::probe(video)?;
    let m = 16u32;
    let (x0, y0) = ((l.x as u32).saturating_sub(m), (l.y as u32).saturating_sub(m));
    let x1 = (l.x as u32 + l.w as u32 + m).min(info.width);
    let y1 = (l.y as u32 + l.h as u32 + m).min(info.height);
    let rect = Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
    let opt = ReadOptions { start: Some(at.parse()?), duration: None, step: 1, threads: 4, scan: Scan::Auto };
    let frame = Reader::open(video, &info, rect, &opt)?.next_frame()?.ok_or("no frame at that time")?;
    let (w, h) = (rect.w as usize, rect.h as usize);
    let hd = source::is_hd(&info);
    let before = erase::frame_to_rgb(&frame, hd);
    let mut frame = frame;
    erase::remove(&l, &mut frame, rect);
    let after = erase::frame_to_rgb(&frame, hd);
    let mut f = BufWriter::new(File::create(out)?);
    write!(f, "P6\n{} {}\n255\n", w * 2, h)?;
    for r in 0..h {
        f.write_all(&before[r * w * 3..(r + 1) * w * 3])?;
        f.write_all(&after[r * w * 3..(r + 1) * w * 3])?;
    }
    f.flush()?;
    Ok(())
}
