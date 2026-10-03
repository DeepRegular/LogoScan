<div align="center">

# LogoScan

**AviUtl's logo analysis, without AviUtl.**

Build the `.lgd` that the transparent-logo filter removes, straight from a recording.

[![License](https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square)](LICENSE)
[![Core](https://img.shields.io/badge/core-Rust-dea584?style=flat-square)](src/)

English ・ [日本語](README.ja.md)

</div>

LogoScan writes the same `.lgd` files as the logo analysis plugin for AviUtl (logoscan),
so the transparent-logo filter (delogo) and every other tool that reads `.lgd` can use them as they are.
It comes as a window (`lgdscan-gui`, labelled in Japanese) and as a command (`lgdscan`) that share the same engine.

Video is read through the `ffmpeg` and `ffprobe` commands.

## Download

From [Releases](https://github.com/DeepRegular/LogoScan/releases):

- **Windows** — `LogoScan-<version>-windows-x86_64.zip`. Unpack it anywhere and run `lgdscan-gui.exe`.
  FFmpeg comes with it, in the `ffmpeg` folder.
- **Linux** — `LogoScan-<version>-linux-x86_64.tar.gz`, for Ubuntu 22.04 or later and the like
  (glibc 2.35). Install FFmpeg from your distribution (`sudo apt install ffmpeg`).

`ffmpeg` and `ffprobe` are looked for next to the program, then in an `ffmpeg` folder beside it,
then on `PATH`.

## The window

```
lgdscan-gui recording.ts [logo.lgd]
```

Recordings can also be dropped on the window (hold Shift to add to the inputs instead of replacing them).

1. **Find the logo.** 120 keyframes spread over the recording are read and the box is placed on the most
   logo-like spot. Other candidates are listed; moving *edge share* or *margin* recomputes them on the spot.
2. **Adjust the box on the picture.**
   - Drag outside the box to draw a new one, inside to move it, on an edge or corner to resize it.
   - Arrow keys move it by one pixel; Shift+arrows change its width and height.
   - The wheel zooms, a right or middle drag pans, a right double-click or *Fit* shows the whole picture.
     Past 2× the pixels are shown as they are.
3. **Analyse.** When it finishes, the opacity of the logo appears as a greyscale image and the picture
   switches to the logo removed. Move the slider to see how it holds up on other scenes.
4. Name the logo and **Save**.

## The command

```
lgdscan scan recording.ts --rect 1700,34,157,45 -o logo.lgd -n MyLogo
```

`--rect` is X,Y,width,height. Leave a little space around the logo: the outermost one-pixel ring of the box
is read as background, so the logo must not touch it. Any number of inputs may be given; more of them give
the background a wider range of colours and a steadier result.

| Option | Default | |
|---|---|---|
| `--start` / `--end` | whole input | range to read, in seconds |
| `--step N` | 1 | read one frame in N |
| `--threshold T` | 12 | how far ring pixels may stray from the background model, in 8-bit levels |
| `--background plane\|flat` | plane | `plane` fits a gradient to the ring; `flat` takes its mean, as logoscan does |
| `--scan auto\|progressive\|interlaced` | auto | how 4:2:0 chroma is interpolated vertically |
| `--max-frames N` | 8000 | frames kept for the fit; beyond that a random subset |
| `--passes N` | 3 | least-squares passes; outliers are dropped from the second on |

To find the logo only:

```
lgdscan detect recording.ts
```

prints candidates as `--rect X,Y,W,H`, most likely first. `--share` (edge share, 0.45), `--margin` (3)
and `--samples` (keyframes, 120) can be changed.

Also:

- `lgdscan info logo.lgd` prints the header
- `lgdscan compare A.lgd B.lgd` compares two logos pixel by pixel
- `lgdscan render logo.lgd out.pgm` writes the luma opacity as a greyscale picture
- `lgdscan erase logo.lgd recording.ts SECONDS out.ppm` shows that frame before and after removal, side by side

## How it works

A logo sits on the picture as `observed = background × (1 − α) + logo × α`.
From the frames whose background is known, LogoScan fits `observed = A × background + B` for every pixel
and every one of Y, Cb and Cr, and stores `dp = (1 − A) × 1000` and `colour = B / (1 − A)` — the same
formulas and the same rounding as logoscan.

Three things differ from logoscan:

- **Background.** A ring that is a smooth gradient is accepted, not just a flat one. Flat frames alone are
  almost all white or black: on white the logo cannot be seen, and on black (scene changes) it is usually
  not shown at all, so neither tells anything.
- **Frames without the logo are set aside.** After a first fit, each frame is checked on the pixels where
  the logo is strongest: if "no logo" explains it better than the fitted blend, it is dropped and the fit
  is run again. Commercials and fades in the range do no harm, so the range need not be picked by hand.
- **Chroma is shaped as AviUtl sees it.** Horizontally the samples sit on the even pixels and the odd ones
  take the mean of their neighbours; vertically, interlaced 4:2:0 is interpolated within each field at
  MPEG-2 positions. Without this, the chroma opacity does not match AviUtl's.

## Finding the logo

The pictures change; the logo does not. Keyframes are sampled across the recording and, for every pixel,
the share of frames with an edge there is counted. Pixels with an edge in most frames are grouped — the gaps
between letters closed — into candidates. A logo that disappears during commercials only gets a lower share.
Pixels close to the border of the picture, and groups longer than half of it (letterbox lines and the like),
are left out.

## Logo names

Names are written in CP932 (Windows-31J), as AviUtl does. Characters that are easy to type on Linux or macOS
but missing from CP932 — 〜 (U+301C), ‖, —, ¢, £, ¬ — are replaced with their CP932 lookalikes
(～ ∥ ― ￠ ￡ ￢). Anything still impossible (é, emoji) is reported before saving.
A name holds 31 bytes; a longer one is cut without splitting a character. Files are always written as ver0.1.
Names are read as CP932 (or as UTF-8, when the bytes are valid UTF-8).

## Accuracy

A whole broadcast recording (MPEG-2 1080i, 30 minutes, a translucent white logo), compared with the `.lgd`
AviUtl made of the same logo:

| | Opacity correlation | Mean difference (/1000) |
|---|---|---|
| Y | 0.9996 | 2.1 |
| Cb | 0.9944 | 5.9 |
| Cr | 0.9929 | 6.4 |

Two logos made from different episodes of the same programme differ by about as much (chroma correlation
0.994); what remains depends on which frames went in.

## Not yet

- Fade in / fade out and the other fields (fi/fo/st/ed) are left at 0.
- One logo per file.

## Building

```
cargo build --release
```

This builds `lgdscan` (the command) and `lgdscan-gui` (the window, made with eframe/egui).
For Japanese text the window uses a system font: Noto Sans CJK, IPA Gothic, Yu Gothic, Meiryo and so on.

## License

[GPL-3.0](LICENSE).

The Windows package includes an FFmpeg build from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds)
(GPL; its licence is in `ffmpeg/LICENSE.txt`, and the sources are at [ffmpeg.org](https://ffmpeg.org/download.html)).
