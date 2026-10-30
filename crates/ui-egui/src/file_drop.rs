//! Audio files dragged in from outside the app onto an audio track: the lane is outlined with a
//! line at the snapped drop point (or the track header is outlined), and the files land there as
//! clips on that track; dropped on the header, they land at the edit cursor.

use crate::SoundApp;
use crate::edit_window::{drag_snap, sample_at, x_of};
use crate::sampler_window::is_audio_name;
use crate::theme::{Tokens, clip_colors};
use egui::{Id, LayerId, Order, Pos2, Rect, Stroke, StrokeKind, Ui, pos2};
use serde_json::json;
use soundcraft_model::{Session, TrackId};
use soundcraft_time::Samples;
use std::path::{Path, PathBuf};

/// Files larger than this, or longer than `MAX_PREVIEW_SECS`, get no waveform preview: the whole
/// file is read and decoded once on the UI thread, so both limits keep that to a brief pause.
const MAX_PREVIEW_BYTES: u64 = 64 << 20;
const MAX_PREVIEW_SECS: f64 = 600.0;
const PREVIEW_COLUMNS: usize = 2048;
const MAX_PREVIEW_CHANNELS: usize = 8;

/// The hovered file's overview: per channel, `PREVIEW_COLUMNS` (min, max) pairs across its length.
pub(crate) struct Preview {
    seconds: f64,
    channels: Vec<Vec<(f32, f32)>>,
}

/// The decoded preview of the most recently hovered file (`None` once decoding was refused).
#[derive(Default)]
pub(crate) struct DropPreview {
    cached: Option<(PathBuf, Option<Preview>)>,
}

impl DropPreview {
    fn get(&mut self, path: &Path) -> Option<&Preview> {
        if self.cached.as_ref().is_none_or(|(p, _)| p != path) {
            self.cached = Some((path.to_path_buf(), load_preview(path)));
        }
        self.cached.as_ref().and_then(|(_, v)| v.as_ref())
    }
}

fn load_preview(path: &Path) -> Option<Preview> {
    if !path.is_absolute() || !is_audio_name(path) || std::fs::metadata(path).ok()?.len() > MAX_PREVIEW_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let ext = path.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let info = soundcraft_audio_io::probe(&bytes, ext.as_deref()).ok()?;
    if info.frames == 0 || info.duration_secs() > MAX_PREVIEW_SECS {
        return None;
    }
    let (info, buf) = soundcraft_audio_io::decode(&bytes, ext.as_deref()).ok()?;
    let channels: Vec<_> = buf.channels.iter().take(MAX_PREVIEW_CHANNELS).map(|c| overview(c)).collect();
    (!channels.is_empty() && info.sample_rate > 0)
        .then(|| Preview { seconds: buf.channels.first().map_or(0, Vec::len) as f64 / f64::from(info.sample_rate), channels })
}

fn overview(samples: &[f32]) -> Vec<(f32, f32)> {
    (0..PREVIEW_COLUMNS)
        .map(|i| {
            let a = samples.len() * i / PREVIEW_COLUMNS;
            let b = (samples.len() * (i + 1) / PREVIEW_COLUMNS).max(a + 1);
            samples
                .get(a..b.min(samples.len()))
                .unwrap_or_default()
                .iter()
                .filter(|v| v.is_finite())
                .fold((0.0f32, 0.0f32), |(lo, hi), &v| (lo.min(v), hi.max(v)))
        })
        .collect()
}

/// The bars of one channel's (min, max) columns stretched over `x0..x1` inside `band`, cut to `clip`.
fn wave_rects(cols: &[(f32, f32)], band: Rect, x0: f32, x1: f32, clip: Rect) -> Vec<Rect> {
    let n = cols.len().max(1) as f32;
    let (cy, half) = (band.center().y, band.height() * 0.5);
    cols.iter()
        .enumerate()
        .filter_map(|(i, &(lo, hi))| {
            let xa = x0 + (x1 - x0) * i as f32 / n;
            let xb = x0 + (x1 - x0) * (i + 1) as f32 / n;
            if xb < clip.min.x || xa > clip.max.x {
                return None;
            }
            let top = cy - hi.clamp(-1.0, 1.0) * half;
            let bottom = (cy - lo.clamp(-1.0, 1.0) * half).max(top + 1.0);
            let r = Rect::from_min_max(pos2(xa, top), pos2(xb.max(xa + 1.0), bottom)).intersect(clip);
            (r.width() > 0.0 && r.height() > 0.0).then_some(r)
        })
        .collect()
}

/// Registers `lane` (a track's lane, or its header: anything left of the timeline `tl`) as where
/// dropped audio files land on track `id`, and outlines it while files hover over it.
pub(crate) fn audio_lane(app: &mut SoundApp, ui: &Ui, lane: Rect, tl: Rect, id: TrackId) {
    app.audio_drop_lanes.push((lane, tl, id));
    let ctx = ui.ctx();
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let Some(p) = ctx.input(|i| i.pointer.latest_pos()) else { return };
    if !lane.contains(p) || ctx.layer_id_at(p) != Some(ui.layer_id()) {
        return;
    }
    let accent = Tokens::current().accent;
    let header = is_header(lane, tl);
    let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique(("audio_drop", id.0)))).with_clip_rect(if header { lane } else { tl });
    painter.rect_stroke(lane, 0.0, Stroke::new(2.0, accent), StrokeKind::Inside);
    if !header {
        let s = app.engine.session();
        let flip = ctx.input(|i| i.modifiers.command);
        let x = x_of(s, tl, drop_at(s, lane, tl, p, flip)).clamp(lane.min.x, lane.max.x);
        painter.vline(x, lane.y_range(), Stroke::new(2.0, accent));
        let path = ctx.input(|i| i.raw.hovered_files.iter().find_map(|f| f.path.clone()));
        let Some(track) = s.track(id) else { return };
        if let Some(pv) = path.and_then(|p| app.drop_preview.get(&p)) {
            let (body, _, wave) = clip_colors(track.color, false);
            let x1 = x_of(s, tl, drop_at(s, lane, tl, p, flip).saturating_add(soundcraft_time::to_samples(pv.seconds * s.sample_rate.as_f64())));
            let r = Rect::from_min_max(pos2(x, lane.min.y + 1.0), pos2(x1.max(x + 1.0), lane.max.y - 1.0));
            painter.rect_filled(r.intersect(tl), 2.0, body.gamma_multiply(0.12));
            let n = track.channels().min(pv.channels.len()).max(1);
            let h = r.height() / n as f32;
            for (k, cols) in pv.channels.iter().take(n).enumerate() {
                let band = Rect::from_min_size(pos2(r.min.x, r.min.y + h * k as f32), egui::vec2(r.width(), h));
                for seg in wave_rects(cols, band, r.min.x, r.max.x, r.intersect(tl)) {
                    painter.rect_filled(seg, 0.0, wave.gamma_multiply(0.32));
                }
            }
        }
    }
}

fn is_header(lane: Rect, tl: Rect) -> bool {
    lane.max.x <= tl.min.x
}

/// Where a drop at `p` lands: the snapped point under it on a lane, the edit cursor on a header.
fn drop_at(s: &Session, lane: Rect, tl: Rect, p: Pos2, flip: bool) -> Samples {
    if is_header(lane, tl) { s.edit.selection.start } else { drag_snap(s, sample_at(s, tl, p.x).max(0), flip) }
}

pub(crate) fn lane_drop(app: &mut SoundApp, files: Vec<egui::DroppedFileHandle>, pos: Option<Pos2>, flip: bool) -> Vec<egui::DroppedFileHandle> {
    let Some(p) = pos else { return files };
    let Some(&(lane, tl, track)) = app.audio_drop_lanes.iter().rev().find(|(lane, ..)| lane.contains(p)) else { return files };
    let mut at = drop_at(app.engine.session(), lane, tl, p, flip);
    let mut rest = Vec::new();
    for f in files {
        if !f.path().is_absolute() || !is_audio_name(f.path()) {
            rest.push(f);
            continue;
        }
        let path = f.path().to_string_lossy().into_owned();
        match app.run("file.import_audio", json!({"path": path, "track": track.0, "new_tracks": false, "at": at})) {
            Ok(v) => {
                let frames = v.pointer("/imported/0/frames").and_then(serde_json::Value::as_i64).unwrap_or(0);
                at = at.saturating_add(frames.max(0));
                let name = f.path().file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string();
                app.ui.status = format!("{name} placed on the track");
            }
            Err(e) => app.ui.status = format!("{path}: {e}"),
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects_scale_to_band_and_clip() {
        let band = Rect::from_min_max(pos2(100.0, 0.0), pos2(200.0, 40.0));
        let cols = [(-1.0, 1.0), (-0.5, 0.5), (0.0, 0.0), (f32::NAN, 9.0)];
        let all = wave_rects(&cols, band, 100.0, 200.0, band);
        assert_eq!(all.len(), 4);
        assert_eq!((all[0].min.y, all[0].max.y), (0.0, 40.0));
        assert_eq!((all[1].min.y, all[1].max.y), (10.0, 30.0));
        assert!(all[2].height() >= 1.0);
        let cut = wave_rects(&cols, band, 100.0, 200.0, Rect::from_min_max(pos2(150.0, 0.0), pos2(180.0, 40.0)));
        assert!(cut.iter().all(|r| r.min.x >= 150.0 && r.max.x <= 180.0) && cut.len() < 4);
    }

    #[test]
    fn overview_spans_whole_file() {
        let v: Vec<f32> = (0..10_000).map(|i| if i < 5000 { 0.5 } else { -0.25 }).collect();
        let o = overview(&v);
        assert_eq!(o.len(), PREVIEW_COLUMNS);
        assert_eq!(o[0], (0.0, 0.5));
        assert_eq!(o[PREVIEW_COLUMNS - 1], (-0.25, 0.0));
        assert!(overview(&[]).iter().all(|&c| c == (0.0, 0.0)));
    }
}
