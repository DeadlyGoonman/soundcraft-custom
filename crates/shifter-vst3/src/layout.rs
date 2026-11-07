//! The editor's layout: platform-free rectangles, hit testing, knob geometry and hover texts.
//! Everything is designed on a 640×330 base and scaled uniformly (centred) to the client size.
//! The Win32 window that draws it lives in `editor`.

use crate::params::{self, BYPASS, PHASE_SWAP};

pub const BASE_W: i32 = 640;
pub const BASE_H: i32 = 330;
/// Editor size limits and default, in pixels.
pub const DEFAULT_W: i32 = BASE_W;
pub const DEFAULT_H: i32 = BASE_H;
pub const MIN_W: i32 = 480;
pub const MIN_H: i32 = 248;
pub const MAX_W: i32 = 1920;
pub const MAX_H: i32 = 990;

/// Knob sweep: 270 degrees, from 225° (bottom left, value 0) clockwise to -45° (bottom right).
pub const SWEEP_START_DEG: f64 = 225.0;
pub const SWEEP_DEG: f64 = 270.0;
/// Pixels of vertical mouse travel (at scale 1) for the full knob range.
pub const DRAG_SPAN: f64 = 220.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn center(&self) -> (i32, i32) {
        ((self.left + self.right) / 2, (self.top + self.bottom) / 2)
    }
}

/// One knob: caption above, dial, end labels under it, value readout below.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knob {
    pub id: u32,
    pub caption: &'static str,
    pub cx: i32,
    pub cy: i32,
    pub r: i32,
    pub label: Rect,
    pub ends: (&'static str, &'static str),
    pub end_left: Rect,
    pub end_right: Rect,
    pub value: Rect,
    /// Where a click grabs the knob.
    pub hit: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    /// The preset bar across the top (full width).
    pub bar: Rect,
    pub prev: Rect,
    pub next: Rect,
    pub name: Rect,
    pub save: Rect,
    pub reset: Rect,
    pub bypass: Rect,
    /// The wordmark and the accent line through the title row.
    pub wordmark: Rect,
    pub line_y: i32,
    pub line_left: (i32, i32),
    pub line_right: (i32, i32),
    /// Mix, Anchor, Detune, Delay (large), Width, Output (small).
    pub knobs: [Knob; 6],
    pub modes: [Rect; 3],
    pub romans: [Rect; 3],
    pub mode_caption: Rect,
    pub mode_name: Rect,
    pub phase: Rect,
    pub phase_label: Rect,
    pub phase_value: Rect,
    pub grip: Rect,
}

/// What is under the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    PrevPreset,
    NextPreset,
    PresetName,
    Save,
    Reset,
    Bypass,
    ModeButton(usize),
    Phase,
    Knob(u32),
    Grip,
}

pub fn clamp_size(w: i32, h: i32) -> (i32, i32) {
    (w.clamp(MIN_W, MAX_W), h.clamp(MIN_H, MAX_H))
}

/// (id, caption, left end, right end, centre x, centre y, radius) on the base canvas.
const KNOBS: [(u32, &str, &str, &str, i32, i32, i32); 6] = [
    (5, "MIX", "DRY", "WET", 80, 150, 30),
    (3, "ANCHOR", "20 HZ", "1 KHZ", 200, 150, 30),
    (2, "DETUNE", "MIN", "MAX", 440, 150, 30),
    (1, "DELAY", "TIGHT", "LOOSE", 560, 150, 30),
    (4, "WIDTH", "MONO", "WIDE", 140, 270, 17),
    (6, "OUTPUT", "-24", "+12", 500, 270, 17),
];

pub fn layout(w: i32, h: i32) -> Layout {
    let (w, h) = (w.max(1), h.max(1));
    let s = (f64::from(w) / f64::from(BASE_W)).min(f64::from(h) / f64::from(BASE_H));
    let ox = (f64::from(w) - f64::from(BASE_W) * s) / 2.0;
    let oy = (f64::from(h) - f64::from(BASE_H) * s) / 2.0;
    let x = |v: i32| (ox + f64::from(v) * s).round() as i32;
    let y = |v: i32| (oy + f64::from(v) * s).round() as i32;
    let d = |v: i32| (f64::from(v) * s).round() as i32;
    let r = |l: i32, t: i32, rr: i32, b: i32| Rect::new(x(l), y(t), x(rr), y(b));
    let knobs = KNOBS.map(|(id, caption, a, b, cx, cy, rad)| {
        let big = rad > 20;
        let (label, ends_y, value) = if big {
            (r(cx - 60, cy - 58, cx + 60, cy - 40), cy + 34, r(cx - 60, cy + 50, cx + 60, cy + 66))
        } else {
            (r(cx - 90, cy - 9, cx - rad - 10, cy + 9), cy + rad + 1, r(cx + rad + 10, cy - 9, cx + 90, cy + 9))
        };
        let span = rad + 22;
        Knob {
            id,
            caption,
            cx: x(cx),
            cy: y(cy),
            r: d(rad),
            label,
            ends: (a, b),
            end_left: r(cx - span, ends_y, cx, ends_y + 14),
            end_right: r(cx, ends_y, cx + span, ends_y + 14),
            value,
            hit: r(cx - rad - 8, cy - rad - 8, cx + rad + 8, cy + rad + 8),
        }
    });
    let mode_x = [270, 320, 370];
    Layout {
        width: w,
        height: h,
        scale: s,
        bar: Rect::new(0, 0, w, y(32)),
        prev: r(10, 6, 30, 26),
        next: r(34, 6, 54, 26),
        name: r(60, 5, 300, 27),
        save: r(306, 6, 326, 26),
        reset: r(330, 6, 350, 26),
        bypass: r(560, 6, 630, 26),
        wordmark: r(240, 38, 400, 70),
        line_y: y(54),
        line_left: (x(20), x(236)),
        line_right: (x(404), x(620)),
        knobs,
        modes: mode_x.map(|cx| r(cx - 15, 135, cx + 15, 165)),
        romans: mode_x.map(|cx| r(cx - 20, 115, cx + 20, 132)),
        mode_caption: r(262, 170, 378, 184),
        mode_name: r(262, 186, 378, 202),
        phase: r(309, 258, 331, 280),
        phase_label: r(250, 238, 390, 254),
        phase_value: r(250, 284, 390, 298),
        grip: Rect::new(w - 18, h - 18, w, h),
    }
}

pub fn hit(l: &Layout, x: i32, y: i32) -> Option<Hit> {
    let plain = [
        (l.grip, Hit::Grip),
        (l.prev, Hit::PrevPreset),
        (l.next, Hit::NextPreset),
        (l.name, Hit::PresetName),
        (l.save, Hit::Save),
        (l.reset, Hit::Reset),
        (l.bypass, Hit::Bypass),
        (l.phase, Hit::Phase),
    ];
    if let Some((_, h)) = plain.iter().find(|(r, _)| r.contains(x, y)) {
        return Some(*h);
    }
    if let Some(i) = l.modes.iter().position(|r| r.contains(x, y)) {
        return Some(Hit::ModeButton(i));
    }
    l.knobs.iter().find(|k| k.hit.contains(x, y)).map(|k| Hit::Knob(k.id))
}

/// Angle in degrees (maths convention: 0 = right, counter-clockwise) for a normalized value.
pub fn knob_angle(n: f64) -> f64 {
    let n = if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.0 };
    SWEEP_START_DEG - SWEEP_DEG * n
}

/// A point on a knob's circle of `radius` at `deg` (screen coordinates, y down).
pub fn polar(cx: i32, cy: i32, radius: f64, deg: f64) -> (i32, i32) {
    let a = deg.to_radians();
    ((f64::from(cx) + radius * a.cos()).round() as i32, (f64::from(cy) - radius * a.sin()).round() as i32)
}

/// Points of an arc from value `from` to `to` (normalized), for a polyline.
pub fn arc_points(cx: i32, cy: i32, radius: f64, from: f64, to: f64) -> Vec<(i32, i32)> {
    let (a, b) = (knob_angle(from), knob_angle(to));
    let steps = (((a - b).abs() / 4.0).ceil() as usize).max(1);
    (0..=steps).map(|i| polar(cx, cy, radius, a + (b - a) * i as f64 / steps as f64)).collect()
}

/// New normalized value after a vertical drag of `dy` pixels (up = positive) from `start`;
/// `fine` (Shift) moves five times slower.
pub fn drag_value(start: f64, dy: i32, scale: f64, fine: bool) -> f64 {
    let span = DRAG_SPAN * scale.max(0.1) * if fine { 5.0 } else { 1.0 };
    (start + f64::from(dy) / span).clamp(0.0, 1.0)
}

/// Hover text for what is under the mouse.
pub fn tooltip(h: Hit) -> &'static str {
    match h {
        Hit::PrevPreset => "Previous preset.",
        Hit::NextPreset => "Next preset.",
        Hit::PresetName => params::PRESETS_TIP,
        Hit::Save => "Save: keep the current settings as the User preset (for this session).",
        Hit::Reset => "Reset: back to the default settings.",
        Hit::Bypass => params::tip(BYPASS),
        Hit::Phase => params::tip(PHASE_SWAP),
        Hit::ModeButton(i) => params::MODE_TIPS.get(i).copied().unwrap_or(""),
        Hit::Knob(id) => params::tip(id),
        Hit::Grip => "Drag to resize the editor.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disjoint(a: &Rect, b: &Rect) -> bool {
        a.right <= b.left || b.right <= a.left || a.bottom <= b.top || b.bottom <= a.top
    }

    #[test]
    fn nothing_overlaps_and_everything_fits() {
        for (w, h) in [(DEFAULT_W, DEFAULT_H), (MIN_W, MIN_H), (1200, 500), (700, 600)] {
            let l = layout(w, h);
            let mut rects = vec![l.prev, l.next, l.name, l.save, l.reset, l.bypass, l.phase];
            rects.extend(l.modes);
            rects.extend(l.knobs.iter().map(|k| k.hit));
            for (i, a) in rects.iter().enumerate() {
                assert!(a.left >= 0 && a.top >= 0 && a.right <= w && a.bottom <= h, "{w}x{h} {a:?}");
                for b in rects.iter().skip(i + 1) {
                    assert!(disjoint(a, b), "{w}x{h} {a:?} {b:?}");
                }
            }
            assert!(l.line_left.1 < l.wordmark.left && l.wordmark.right < l.line_right.0);
        }
    }

    #[test]
    fn hit_testing_knobs_and_tips() {
        let l = layout(DEFAULT_W, DEFAULT_H);
        let at = |r: &Rect| {
            let (x, y) = r.center();
            hit(&l, x, y)
        };
        assert_eq!(at(&l.bypass), Some(Hit::Bypass));
        assert_eq!(at(&l.phase), Some(Hit::Phase));
        assert_eq!(at(&l.name), Some(Hit::PresetName));
        assert_eq!(at(&l.prev), Some(Hit::PrevPreset));
        assert_eq!(at(&l.modes[2]), Some(Hit::ModeButton(2)));
        let ids: Vec<u32> = l.knobs.iter().map(|k| k.id).collect();
        assert_eq!(ids, [5, 3, 2, 1, 4, 6]);
        for k in &l.knobs {
            assert_eq!(hit(&l, k.cx, k.cy), Some(Hit::Knob(k.id)));
        }
        assert_eq!(hit(&l, l.width - 3, l.height - 3), Some(Hit::Grip));
        for h in [Hit::Bypass, Hit::Phase, Hit::Save, Hit::Reset, Hit::PresetName, Hit::ModeButton(0), Hit::Knob(3), Hit::Grip] {
            assert!(!tooltip(h).is_empty(), "{h:?}");
        }
        assert!(tooltip(Hit::ModeButton(2)).contains("mono"));
    }

    #[test]
    fn knob_geometry_and_drag() {
        assert_eq!(knob_angle(0.0), 225.0);
        assert_eq!(knob_angle(1.0), -45.0);
        assert_eq!(knob_angle(0.5), 90.0, "middle points straight up");
        assert_eq!(polar(100, 100, 10.0, 90.0), (100, 90));
        let pts = arc_points(0, 0, 10.0, 0.0, 1.0);
        assert!(pts.len() > 10 && pts.first() == Some(&polar(0, 0, 10.0, 225.0)));
        assert_eq!(drag_value(0.5, 0, 1.0, false), 0.5);
        assert!((drag_value(0.0, 110, 1.0, false) - 0.5).abs() < 1e-9);
        assert!((drag_value(0.0, 110, 1.0, true) - 0.1).abs() < 1e-9);
        assert_eq!(drag_value(0.9, 500, 1.0, false), 1.0);
        assert_eq!(drag_value(0.1, -500, 2.0, false), 0.0);
    }
}
