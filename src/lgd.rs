//! The .lgd file format of the AviUtl logo plugin (delogo / logoscan).
//!
//! ```text
//! file header   28 bytes  "<logo data file ver0.1>" NUL-padded (or ver0.2)
//!                4 bytes  number of logos, big-endian
//! per logo:
//!   name        32 bytes (ver0.1) / 256 bytes (ver0.2), CP932, NUL-padded
//!   x, y, h, w, fi, fo, st, ed   i16 little-endian each
//!   pixels      h * w * { dp_y, y, dp_cb, cb, dp_cr, cr }  i16 little-endian
//! ```
//!
//! Colour values are in AviUtl's PIXEL_YC scale (Y 0..4096, Cb/Cr -2048..2048)
//! and `dp` is the opacity scaled to `LOGO_MAX_DP` (1000).

use std::io::{self, Read, Write};

pub const LOGO_MAX_DP: i32 = 1000;
const HEADER_V1: &[u8] = b"<logo data file ver0.1>";
const HEADER_V2: &[u8] = b"<logo data file ver0.2>";
const HEADER_LEN: usize = 28;
const NAME_V1: usize = 32;
const NAME_V2: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogoPixel {
    pub dp_y: i16,
    pub y: i16,
    pub dp_cb: i16,
    pub cb: i16,
    pub dp_cr: i16,
    pub cr: i16,
}

#[derive(Clone, Debug, Default)]
pub struct Logo {
    /// Raw name bytes as stored (CP932 in files written by AviUtl).
    pub name: Vec<u8>,
    pub x: i16,
    pub y: i16,
    pub w: i16,
    pub h: i16,
    pub fi: i16,
    pub fo: i16,
    pub st: i16,
    pub ed: i16,
    pub pixels: Vec<LogoPixel>,
}

impl Logo {
    /// The name as text: CP932 as AviUtl writes it, or UTF-8 as early
    /// lgdscan builds did.
    pub fn name_lossy(&self) -> String {
        decode_name(&self.name)
    }
}

pub fn decode_name(bytes: &[u8]) -> String {
    // Japanese in CP932 is practically never valid UTF-8, so a valid UTF-8
    // name is taken as such.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    encoding_rs::SHIFT_JIS.decode(bytes).0.into_owned()
}

/// Characters that look the same but that Unicode keeps apart; CP932 has
/// only the second of each pair (what text typed on Linux or macOS often
/// carries versus what Windows does).
const CP932_LOOKALIKES: &[(char, char)] = &[
    ('\u{301C}', '\u{FF5E}'), // 〜 -> ～
    ('\u{2016}', '\u{2225}'), // ‖ -> ∥
    ('\u{2014}', '\u{2015}'), // — -> ―
    ('\u{00A2}', '\u{FFE0}'), // ¢ -> ￠
    ('\u{00A3}', '\u{FFE1}'), // £ -> ￡
    ('\u{00AC}', '\u{FFE2}'), // ¬ -> ￢
    ('\u{00A6}', '\u{FFE4}'), // ¦ -> ￤
];

/// The CP932 character that looks like `c`, or `c` itself.
pub fn cp932_lookalike(c: char) -> char {
    CP932_LOOKALIKES.iter().find(|(from, _)| *from == c).map_or(c, |(_, to)| *to)
}

/// Encodes a logo name in CP932, the code page AviUtl reads it in.
/// Fails with the characters CP932 cannot hold.
pub fn encode_name(name: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(name.len() * 2);
    let mut bad = String::new();
    let mut buf = [0u8; 4];
    for c in name.chars() {
        let c = cp932_lookalike(c);
        let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut buf));
        if unmappable {
            if !bad.contains(c) {
                bad.push(c);
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    if bad.is_empty() {
        Ok(out)
    } else {
        Err(bad)
    }
}

/// What [`write`] keeps of a CP932 name.
pub fn stored_name(bytes: &[u8]) -> &[u8] {
    &bytes[..cp932_prefix(bytes, NAME_MAX_V1)]
}

/// Longest name a ver0.1 file holds, in bytes (the field ends with a NUL).
pub const NAME_MAX_V1: usize = NAME_V1 - 1;

pub fn read(mut r: impl Read) -> io::Result<Vec<Logo>> {
    let mut head = [0u8; HEADER_LEN + 4];
    r.read_exact(&mut head)?;
    let name_len = if head.starts_with(HEADER_V2) {
        NAME_V2
    } else if head.starts_with(HEADER_V1) {
        NAME_V1
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a .lgd file"));
    };
    let count = u32::from_be_bytes(head[HEADER_LEN..].try_into().unwrap());
    let mut logos = Vec::new();
    for _ in 0..count {
        let mut name = vec![0u8; name_len];
        r.read_exact(&mut name)?;
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        name.truncate(end);
        let mut f = [0u8; 16];
        r.read_exact(&mut f)?;
        let v = |i: usize| i16::from_le_bytes([f[i * 2], f[i * 2 + 1]]);
        let (x, y, h, w, fi, fo, st, ed) = (v(0), v(1), v(2), v(3), v(4), v(5), v(6), v(7));
        if w <= 0 || h <= 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad logo size"));
        }
        let n = w as usize * h as usize;
        let mut raw = vec![0u8; n * 12];
        r.read_exact(&mut raw)?;
        let pixels = raw
            .as_chunks::<12>()
            .0
            .iter()
            .map(|c| {
                let g = |i: usize| i16::from_le_bytes([c[i * 2], c[i * 2 + 1]]);
                LogoPixel { dp_y: g(0), y: g(1), dp_cb: g(2), cb: g(3), dp_cr: g(4), cr: g(5) }
            })
            .collect();
        logos.push(Logo { name, x, y, w, h, fi, fo, st, ed, pixels });
    }
    Ok(logos)
}

/// Writes ver0.1, which every reader of .lgd understands; a name longer
/// than [`NAME_MAX_V1`] bytes is cut on a character boundary.
pub fn write(mut out: impl Write, logos: &[Logo]) -> io::Result<()> {
    let (tag, name_len) = (HEADER_V1, NAME_V1);
    let mut head = [0u8; HEADER_LEN + 4];
    head[..tag.len()].copy_from_slice(tag);
    head[HEADER_LEN..].copy_from_slice(&(logos.len() as u32).to_be_bytes());
    out.write_all(&head)?;
    for l in logos {
        let mut name = vec![0u8; name_len];
        let n = cp932_prefix(&l.name, name_len - 1);
        name[..n].copy_from_slice(&l.name[..n]);
        out.write_all(&name)?;
        for v in [l.x, l.y, l.h, l.w, l.fi, l.fo, l.st, l.ed] {
            out.write_all(&v.to_le_bytes())?;
        }
        let mut raw = Vec::with_capacity(l.pixels.len() * 12);
        for p in &l.pixels {
            for v in [p.dp_y, p.y, p.dp_cb, p.cb, p.dp_cr, p.cr] {
                raw.extend_from_slice(&v.to_le_bytes());
            }
        }
        out.write_all(&raw)?;
    }
    Ok(())
}

/// Length of the longest prefix of CP932 `bytes` within `max` bytes that
/// does not cut a two-byte character in half.
fn cp932_prefix(bytes: &[u8], max: usize) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let len = if (0x81..=0x9F).contains(&b) || (0xE0..=0xFC).contains(&b) { 2 } else { 1 };
        if i + len > max {
            break;
        }
        i += len;
    }
    i.min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_in_cp932() {
        let b = encode_name("ロゴ名テスト").unwrap();
        assert_eq!(b[..4], [0x83, 0x8D, 0x83, 0x53]);
        assert_eq!(decode_name(&b), "ロゴ名テスト");
        assert_eq!(decode_name(b"Logo_202607"), "Logo_202607");
        assert_eq!(decode_name("ロゴ１１".as_bytes()), "ロゴ１１");
    }

    #[test]
    fn lookalikes_and_unmappable() {
        assert_eq!(encode_name("A〜B").unwrap(), [b'A', 0x81, 0x60, b'B']);
        assert_eq!(encode_name("é🎵é"), Err("é🎵".to_string()));
    }

    #[test]
    fn truncation_keeps_characters_whole() {
        let b = encode_name("あいうえおかきくけこさしすせそた").unwrap();
        assert_eq!(b.len(), 32);
        assert_eq!(cp932_prefix(&b, 31), 30);
        assert_eq!(cp932_prefix(b"abc", 31), 3);
    }

    #[test]
    fn write_then_read() {
        let logo = Logo { name: encode_name("ロゴ").unwrap(), x: 1, y: 2, w: 2, h: 1, pixels: vec![LogoPixel { dp_y: 5, ..Default::default() }; 2], ..Default::default() };
        let mut buf = Vec::new();
        write(&mut buf, std::slice::from_ref(&logo)).unwrap();
        assert_eq!(buf.len(), 32 + 48 + 24);
        assert!(buf.starts_with(b"<logo data file ver0.1>"));
        let back = read(&buf[..]).unwrap();
        assert_eq!(back[0].name_lossy(), "ロゴ");
        assert_eq!(back[0].pixels, logo.pixels);
        assert_eq!((back[0].x, back[0].y, back[0].w, back[0].h), (1, 2, 2, 1));
    }
}
