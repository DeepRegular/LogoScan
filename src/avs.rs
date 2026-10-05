//! Sample AviSynth scripts to go with the logo files: how to call delogomod
//! on a moving logo's .ldp and delogo on a still logo's .lgd. They define a
//! function and show one call; the recording itself is left to the user.

use std::path::Path;

use crate::anim::Hold;

/// For a moving logo: EraseLogomod applies the logos in the .ldp one per
/// frame from `start`, then holds the last until `end`, fading it out.
pub fn moving(ldp: &str, frames: usize, hold: Option<Hold>) -> String {
    let (length, fadeout, note) = match hold {
        Some(h) => (h.end.to_string(), h.fadeout.to_string(), "end と fadeout は、止まったロゴが消えていく様子を録画から測った値です。"),
        None => ("0".into(), "0".into(), "end と fadeout は測れませんでした。止まったロゴが消えるまでの長さを入れてください。"),
    };
    let mut s = String::new();
    s.push_str(&format!("# {ldp} を delogomod で使うサンプル（lgdscan が書きました）\n"));
    s.push_str("#\n");
    s.push_str(&format!("# {ldp} には、アニメーションのロゴが 1 フレームずつ {frames} 枚入っています。\n"));
    s.push_str("# EraseLogomod はこれを start から上から順に当て、使い切ったあとは最後の 1 枚（止まったロゴ）を\n");
    s.push_str("# end まで当て続け、最後の fadeout フレームで薄くしていきます。\n");
    s.push_str(&format!("# {note}\n"));
    s.push_str("# start には、録画でアニメーションの最初のフレーム（ロゴがうっすら出始めるところ）を入れます。\n");
    s.push('\n');
    s.push_str("#LoadPlugin(\"delogomod.dll\")\n");
    s.push('\n');
    s.push_str("function EraseMovingLogo(clip c, int \"start\", int \"length\", int \"fadeout\")\n");
    s.push_str("{\n");
    s.push_str("  start = default(start, 0)\n");
    s.push_str(&format!("  length = default(length, {length})\n"));
    s.push_str(&format!("  fadeout = default(fadeout, {fadeout})\n"));
    s.push_str(&format!("  return c.EraseLogomod(logofile=\"{ldp}\", start=start, end=start+length, fadeout=fadeout)\n"));
    s.push_str("}\n");
    s.push('\n');
    s.push_str("# 例: 16 フレーム目からアニメーションが始まるとき\n");
    s.push_str("#EraseMovingLogo(16)\n");
    s.push_str("# 1 本の録画に何回も出てくるときは、そのぶん並べます\n");
    s.push_str("#EraseMovingLogo(16).EraseMovingLogo(32400)\n");
    s.replace('\n', "\r\n")
}

/// For a still logo: delogo's EraseLOGO over a range of frames.
pub fn still(lgd: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {lgd} を delogo で使うサンプル（lgdscan が書きました）\n"));
    s.push_str("#\n");
    s.push_str("# start から end までロゴを消します。end を省くと最後まで消します。\n");
    s.push_str("# CM などロゴの出ていないところを含めるときは、範囲を分けて並べてください。\n");
    s.push_str("# interlaced は、インターレースの録画（放送の 1080i など）なら true にします。\n");
    s.push('\n');
    s.push_str("#LoadPlugin(\"delogo.dll\")\n");
    s.push('\n');
    s.push_str("function EraseStillLogo(clip c, int \"start\", int \"end\", int \"fadein\", int \"fadeout\")\n");
    s.push_str("{\n");
    s.push_str(&format!(
        "  return c.EraseLOGO(logofile=\"{lgd}\", start=default(start, 0), end=default(end, -1), fadein=default(fadein, 0), fadeout=default(fadeout, 0), interlaced=true)\n"
    ));
    s.push_str("}\n");
    s.push('\n');
    s.push_str("# 例: 録画全体から消す\n");
    s.push_str("#EraseStillLogo()\n");
    s.push_str("# 例: 300〜25000 フレーム目だけ、前後 15 フレームでフェードさせて消す\n");
    s.push_str("#EraseStillLogo(300, 25000, 15, 15)\n");
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
        assert!(t.contains("c.EraseLogomod(logofile=\"anim.ldp\", start=start, end=start+length, fadeout=fadeout)\r\n"));
        assert!(t.contains("length = default(length, 232)\r\n"));
        assert!(t.contains("fadeout = default(fadeout, 22)\r\n"));
        assert_eq!(t.matches("\r\n").count(), t.matches('\n').count());
    }

    #[test]
    fn still_sample_names_the_file() {
        let t = still("ロゴ.lgd");
        assert!(t.contains("EraseLOGO(logofile=\"ロゴ.lgd\""));
    }
}
