//! Sample AviSynth scripts to go with the logo files: how to call delogomod
//! on a moving logo's .ldp and delogo on a still logo's .lgd. They define a
//! function and show one call; the recording itself is left to the user.

use std::path::Path;

use crate::anim::Hold;

/// For a moving logo: EraseLogomod applies the logos in the .ldp one per
/// frame from `start`, then holds the last until `end`, fading it out.
pub fn moving(ldp: &str, frames: usize, hold: Option<Hold>) -> String {
    let (length, fadeout, note) = match hold {
        Some(h) => (h.end.to_string(), h.fadeout.to_string(), "end と fadeout は、局ロゴが消えていく様子を録画から測った値です。"),
        None => ("0".into(), "0".into(), "end と fadeout は測れませんでした。局ロゴが消えるまでの長さを入れてください。"),
    };
    let mut s = String::new();
    s.push_str(&format!("# {ldp} を delogomod で使うサンプル（lgdscan が書きました）\n"));
    s.push_str(&format!("# アニメーションのロゴが 1 フレームずつ {frames} 枚入っています。start には、ロゴがうっすら出始めるフレームを入れます。\n"));
    s.push_str(&format!("# {note}\n"));
    s.push_str("# 録画がアニメーションの途中から始まるときは、過ぎたフレーム数を logo_start に入れます。\n");
    s.push('\n');
    s.push_str(&format!("#EraseLogomod(logofile=\"{ldp}\", start=16, end=16+{length}, fadeout={fadeout})\n"));
    s.push_str(&format!("#EraseLogomod(logofile=\"{ldp}\", start=32392, end=32392+{length}, fadeout={fadeout})\n"));
    s.push_str(&format!("#EraseLogomod(logofile=\"{ldp}\", start=0, end={length}-20, fadeout={fadeout}, logo_start=20)\n"));
    s.replace('\n', "\r\n")
}

/// For a still logo: delogo's EraseLOGO, one call per stretch of the
/// programme so the logo is not "removed" during commercials.
pub fn still(lgd: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {lgd} を delogo で使うサンプル（lgdscan が書きました）\n"));
    s.push_str("# 本編の区間ごとに start と end を入れて並べます。CM の間は消さないでください（ロゴの形が浮き出ます）。\n");
    s.push('\n');
    s.push_str(&format!("#EraseLOGO(logofile=\"{lgd}\", start=300, end=15299, interlaced=true).EraseLOGO(logofile=\"{lgd}\", start=18000, end=32399, interlaced=true)\n"));
    s.replace('\n', "\r\n")
}

/// The sample's path: the logo file's, with .avs.
pub fn path_for(logo: &Path) -> std::path::PathBuf {
    logo.with_extension("avs")
}

/// Writes a sample in CP932, which AviSynth reads scripts in.
pub fn write(path: &Path, text: &str) -> Result<(), String> {
    let mut bad = String::new();
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut buf = [0u8; 4];
    for c in text.chars() {
        let c = crate::lgd::cp932_lookalike(c);
        let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut buf));
        if unmappable {
            if !bad.contains(c) {
                bad.push(c);
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    if !bad.is_empty() {
        return Err(format!("ファイル名に CP932 で書けない文字があります: {bad}"));
    }
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_sample_carries_the_measured_hold() {
        let t = moving("anim.ldp", 106, Some(Hold { end: 232, fadeout: 22 }));
        assert!(t.contains("#EraseLogomod(logofile=\"anim.ldp\", start=16, end=16+232, fadeout=22)\r\n"));
        assert_eq!(t.matches("\r\n").count(), t.matches('\n').count());
    }

    #[test]
    fn still_sample_names_the_file() {
        let t = still("ロゴ.lgd");
        assert!(t.contains("EraseLOGO(logofile=\"ロゴ.lgd\", start=300, end=15299, interlaced=true)"));
    }
}
