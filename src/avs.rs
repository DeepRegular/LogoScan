//! Sample AviSynth scripts to go with the logo files: how to call delogomod
//! on a moving logo's .ldp and delogo on a still logo's .lgd. They define a
//! function and show one call; the recording itself is left to the user.

use std::path::Path;

use crate::anim::Hold;
use crate::spans::Fades;

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
/// programme so the logo is not "removed" during commercials. The frame
/// numbers are examples; `fades` are the station's, when measured.
pub fn still(lgd: &str, fades: Fades) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {lgd} を delogo で使うサンプル（lgdscan が書きました）\n"));
    s.push_str("# 本編の区間ごとに start と end を入れて並べます。CM の間は消さないでください（ロゴの形が浮き出ます）。\n");
    let mut fade = String::new();
    match (fades.fadein, fades.fadeout) {
        (None, None) => s.push_str("# ロゴがフェードする局では、fadein と fadeout にフェードのフレーム数を入れます。\n"),
        (Some(0) | None, Some(0) | None) => s.push_str("# 録画から測ったところ、ロゴはフェードせずに出入りします。\n"),
        (fadein, fadeout) => {
            s.push_str("# fadein と fadeout は、ロゴが出るとき・消えるときのフェードを録画から測った値です。\n");
            if let Some(f @ 1..) = fadein {
                fade.push_str(&format!(", fadein={f}"));
            }
            if let Some(f @ 1..) = fadeout {
                fade.push_str(&format!(", fadeout={f}"));
            }
        }
    }
    for (what, v) in [("出る", fades.fadein), ("消える", fades.fadeout)] {
        if v.is_none() && (fades.fadein.is_some() || fades.fadeout.is_some()) {
            s.push_str(&format!("# 録画にロゴが{what}ところが無く、そちらのフェードは測れていません。\n"));
        }
    }
    s.push('\n');
    s.push_str(&format!(
        "#EraseLOGO(logofile=\"{lgd}\", start=300, end=15299{fade}, interlaced=true).EraseLOGO(logofile=\"{lgd}\", start=18000, end=32399{fade}, interlaced=true)\n"
    ));
    s.replace('\n', "\r\n")
}

/// For a still logo in one recording: the chain of calls found in it.
pub fn recording(lgd: &str, video: &str, call: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {video} で {lgd} を使う呼び出し（lgdscan が書きました）\n"));
    s.push_str("# ロゴの出ている区間ごとに、録画から測った start・end・fadein・fadeout で並べています。\n");
    s.push_str("# フレーム番号は lgdscan が読んだ最初のフレームを 0 として数えています。\n");
    s.push('\n');
    s.push_str(&format!("{call}\n"));
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
        let t = still("ロゴ.lgd", Fades::default());
        assert!(t.contains("EraseLOGO(logofile=\"ロゴ.lgd\", start=300, end=15299, interlaced=true)"));
    }

    #[test]
    fn still_sample_carries_the_measured_fades() {
        let t = still("a.lgd", Fades { fadein: Some(21), fadeout: Some(28) });
        assert!(t.contains("#EraseLOGO(logofile=\"a.lgd\", start=300, end=15299, fadein=21, fadeout=28, interlaced=true).EraseLOGO(logofile=\"a.lgd\", start=18000, end=32399, fadein=21, fadeout=28, interlaced=true)\r\n"));
        let t = still("a.lgd", Fades { fadein: Some(0), fadeout: Some(0) });
        assert!(t.contains("start=300, end=15299, interlaced=true)"));
        assert!(t.contains("フェードせずに"));
        let t = still("a.lgd", Fades { fadein: Some(21), fadeout: None });
        assert!(t.contains("end=15299, fadein=21, interlaced=true)"));
        assert!(t.contains("消えるところが無く"));
    }
}
