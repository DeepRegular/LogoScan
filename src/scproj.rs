//! Recordings from a SmartCut project (.scproj): every range a clip keeps
//! becomes one input, so the stretches cut out for a logo analysis need not
//! be written out first.
//!
//! A clip's `edit.cuts` are the ranges removed, in seconds from the start of
//! the file (the container's start time, which is also what ffmpeg's -ss
//! counts from). What is kept runs from the first key frame to the end of
//! the file, less the cuts. A clip never opened has no cuts: all of it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::source::{self, Input};

/// Shorter stretches are not read. SmartCut ends a recording where its
/// pictures end; the container (sound included) can run a fraction of a
/// second longer, and a cut "to the end" would leave that much behind.
const LEAST: f64 = 1.0;

pub fn is_project(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("scproj"))
}

/// The files given, with every project replaced by the ranges it keeps.
pub fn expand(paths: &[PathBuf]) -> Result<Vec<Input>, String> {
    let mut out = Vec::new();
    for p in paths {
        if is_project(p) {
            out.extend(load(p)?);
        } else {
            out.push(Input::whole(p.clone()));
        }
    }
    Ok(out)
}

pub fn load(project: &Path) -> Result<Vec<Input>, String> {
    let text = std::fs::read_to_string(project).map_err(|e| format!("{}: {e}", project.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", project.display()))?;
    let clips = json
        .get("clips")
        .and_then(|c| c.as_array())
        .ok_or_else(|| format!("{}: SmartCut のプロジェクトではありません（clips がありません）", project.display()))?;
    // The same recording often appears once per stretch; look at each once.
    let mut facts: HashMap<PathBuf, (f64, f64)> = HashMap::new();
    let mut out = Vec::new();
    for clip in clips {
        let Some(path) = clip.get("path").and_then(|p| p.as_str()) else { continue };
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(format!("プロジェクトの録画が見つかりません: {}", path.display()));
        }
        let (first, duration) = match facts.get(&path) {
            Some(f) => *f,
            None => {
                let info = source::probe(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let f = (source::first_point(&path), info.duration);
                facts.insert(path.clone(), f);
                f
            }
        };
        let end = if duration > 0.0 { duration } else { f64::INFINITY };
        let cuts = clip.get("edit").and_then(|e| e.get("cuts")).and_then(|c| c.as_array());
        let cuts: Vec<(f64, f64)> = cuts
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c.get("a")?.as_f64()?, c.get("b")?.as_f64()?)))
            .collect();
        out.extend(
            kept(first, end, cuts)
                .into_iter()
                .filter(|(a, b)| b - a >= LEAST)
                .map(|span| Input { path: path.clone(), span: Some(span) }),
        );
    }
    if out.is_empty() {
        return Err(format!("{}: 残す範囲がひとつもありません", project.display()));
    }
    Ok(out)
}

/// What is left of `from..to` once the cuts are taken out, as SmartCut
/// works it out: cuts sorted, overlapping ones merged, empty ones dropped.
fn kept(from: f64, to: f64, mut cuts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    cuts.retain(|(a, b)| b > a);
    cuts.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out = Vec::new();
    let mut pos = from;
    for (a, b) in cuts {
        let a = a.min(to);
        if a > pos {
            out.push((pos, a));
        }
        pos = pos.max(b);
    }
    if to > pos {
        out.push((pos, to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_the_cuts_leave() {
        // As cut for a moving logo: a lead-in before the first key frame,
        // then all but eight seconds.
        let k = kept(0.6655, 1805.04, vec![(0.6655, 2.9011), (10.9091, 1805.035)]);
        assert_eq!(k.len(), 2);
        assert_eq!(k[0], (2.9011, 10.9091));
        assert!((k[1].0 - 1805.035).abs() < 1e-9);
        assert_eq!(kept(1.0, 10.0, vec![(5.0, 7.0), (2.0, 3.0), (6.0, 8.0)]), vec![(1.0, 2.0), (3.0, 5.0), (8.0, 10.0)]);
        assert_eq!(kept(0.0, f64::INFINITY, Vec::new()), vec![(0.0, f64::INFINITY)]);
    }

    #[test]
    fn window_narrows_the_stretch() {
        let i = Input { path: PathBuf::from("a.ts"), span: Some((10.0, 20.0)) };
        assert_eq!(i.window(None, None), Some((Some(10.0), Some(20.0))));
        assert_eq!(i.window(Some(12.0), Some(30.0)), Some((Some(12.0), Some(20.0))));
        assert_eq!(i.window(Some(25.0), None), None);
        let open = Input { path: PathBuf::from("a.ts"), span: Some((10.0, f64::INFINITY)) };
        assert_eq!(open.window(None, None), Some((Some(10.0), None)));
    }
}
