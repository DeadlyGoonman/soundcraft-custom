//! The header strip on top of every plugin editor window: platform-free layout, hit testing and
//! frame-drag helpers. The Win32 window that draws it lives in `frame`. The editor is scaled by
//! dragging the frame (or from the insert's right-click menu); the header holds Bypass and Delta.

use soundcraft_dsp::{EditorHeaderState, EditorScaleMode};

/// Header height at 96 dpi, in pixels.
pub const HEADER_DIP: i32 = 28;

/// Factors a dragged frame snaps to when it ends within 2% of one.
pub const SNAPS: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

/// A rectangle in header client pixels (`right`/`bottom` exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// A clickable part of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Bypass,
    /// "Δ Delta": wet minus dry.
    Delta,
}

/// Where the header's buttons are for a width and dpi: `… [Bypass] [Δ Delta]`, right-aligned
/// (next to the close button above them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub height: i32,
    pub bypass: Rect,
    pub delta: Rect,
}

/// `dip` device-independent pixels at `dpi` (96 when unknown), rounded.
pub fn px(dip: i32, dpi: u32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi.min(960) } as i32;
    (dip * dpi + 48) / 96
}

/// Header height at `dpi`.
pub fn height(dpi: u32) -> i32 {
    px(HEADER_DIP, dpi)
}

/// Button positions for a header `width` pixels wide at `dpi`.
pub fn layout(width: i32, dpi: u32) -> Layout {
    let h = height(dpi);
    let pad = px(4, dpi);
    let (top, bottom) = (pad, h - pad);
    let delta_w = px(68, dpi);
    let bypass_w = px(64, dpi);
    let right = width.max(pad + bypass_w + pad + delta_w + pad) - pad;
    let delta = Rect { left: right - delta_w, top, right, bottom };
    let bypass = Rect { left: delta.left - pad - bypass_w, top, right: delta.left - pad, bottom };
    Layout { height: h, bypass, delta }
}

/// The button under a header point.
pub fn hit(l: &Layout, x: i32, y: i32) -> Option<Button> {
    [(l.bypass, Button::Bypass), (l.delta, Button::Delta)].into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, b)| b)
}

/// Delta needs a dry signal: unavailable on instruments and while bypassed.
pub fn delta_enabled(s: &EditorHeaderState) -> bool {
    !s.bypass && !s.is_instrument
}

/// Whether the editor can be scaled: the plugin draws itself at another size, or its saved mode
/// is one of the upscalers. Otherwise the frame is fixed (no sizing border).
pub fn scalable(s: &EditorHeaderState) -> bool {
    s.native_scaling || matches!(s.scale.mode, EditorScaleMode::Upscale(_))
}

/// A dragged factor: snapped to 100/125/150/200% within 2%, otherwise rounded to whole percent,
/// and kept in 0.5..=4.0.
pub fn snap_factor(f: f32) -> f32 {
    if !f.is_finite() {
        return 1.0;
    }
    let f = f.clamp(0.5, 4.0);
    if let Some(s) = SNAPS.iter().find(|s| (f / **s - 1.0).abs() <= 0.02) {
        return *s;
    }
    (f * 100.0).round() / 100.0
}

/// Which frame edge a drag moves (Win32 `WMSZ_*` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    TopLeft,
    TopRight,
    Bottom,
    BottomLeft,
    BottomRight,
}

impl Edge {
    /// From a `WMSZ_*` value (1..=8).
    pub fn from_wmsz(v: usize) -> Option<Edge> {
        [Edge::Left, Edge::Right, Edge::Top, Edge::TopLeft, Edge::TopRight, Edge::Bottom, Edge::BottomLeft, Edge::BottomRight]
            .get(v.checked_sub(1)?)
            .copied()
    }
    /// Only the height is dragged: the width follows it.
    fn height_leads(self) -> bool {
        matches!(self, Edge::Top | Edge::Bottom)
    }
}

/// The body size for a drag that wants `want`, locked to `native`'s aspect ratio and kept to
/// factors `min..=max` of it.
pub fn locked_body(native: (f64, f64), want: (f64, f64), edge: Edge, min: f64, max: f64) -> (f64, f64) {
    let (nw, nh) = (native.0.max(1.0), native.1.max(1.0));
    let f = if edge.height_leads() { want.1 / nh } else { want.0 / nw };
    let f = if f.is_finite() { f.clamp(min, max.max(min)) } else { 1.0 };
    ((nw * f).round(), (nh * f).round())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::{EditorScale, UpscaleFilter};

    #[test]
    fn layout_scales_with_dpi_and_stays_inside() {
        for (dpi, h) in [(96, 28), (144, 42), (192, 56)] {
            let l = layout(800, dpi);
            assert_eq!(l.height, h);
            assert_eq!(l.delta.right, 800 - px(4, dpi), "right-aligned");
            assert!(l.bypass.right < l.delta.left);
            for r in [l.bypass, l.delta] {
                assert!(r.top >= 0 && r.bottom <= h && r.left < r.right && r.left >= 0);
            }
        }
        // Too narrow: the buttons still don't overlap or leave the left edge.
        let l = layout(50, 96);
        assert!(l.bypass.left >= 0 && l.bypass.right < l.delta.left);
    }

    #[test]
    fn hits_find_each_button() {
        for dpi in [96, 144, 192] {
            let l = layout(640, dpi);
            let mid = |r: Rect| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            for (r, b) in [(l.bypass, Button::Bypass), (l.delta, Button::Delta)] {
                let (x, y) = mid(r);
                assert_eq!(hit(&l, x, y), Some(b), "{dpi}");
            }
            assert_eq!(hit(&l, 20, l.height / 2), None, "the left side is empty");
            assert_eq!(hit(&l, l.bypass.left, 0), None, "above the buttons");
        }
    }

    #[test]
    fn fixed_size_plugins_scale_only_in_an_upscale_mode() {
        assert!(!scalable(&EditorHeaderState::default()), "default: native only");
        assert!(scalable(&EditorHeaderState { native_scaling: true, ..Default::default() }));
        let upscale = EditorScale { factor: 1.0, mode: EditorScaleMode::Upscale(UpscaleFilter::Bilinear) };
        assert!(scalable(&EditorHeaderState { scale: upscale, ..Default::default() }));
    }

    #[test]
    fn delta_needs_a_dry_signal() {
        assert!(delta_enabled(&EditorHeaderState::default()));
        assert!(!delta_enabled(&EditorHeaderState { bypass: true, ..Default::default() }));
        assert!(!delta_enabled(&EditorHeaderState { is_instrument: true, ..Default::default() }));
    }

    #[test]
    fn drags_snap_and_lock_the_aspect() {
        assert_eq!(snap_factor(1.49), 1.5);
        assert_eq!(snap_factor(1.02), 1.0);
        assert_eq!(snap_factor(1.37), 1.37);
        assert_eq!(snap_factor(9.0), 4.0);
        assert_eq!(snap_factor(f32::NAN), 1.0);
        assert_eq!(locked_body((400.0, 300.0), (800.0, 10.0), Edge::Right, 0.5, 4.0), (800.0, 600.0));
        assert_eq!(locked_body((400.0, 300.0), (10.0, 450.0), Edge::Bottom, 0.5, 4.0), (600.0, 450.0));
        assert_eq!(locked_body((400.0, 300.0), (100.0, 100.0), Edge::BottomRight, 0.5, 4.0), (200.0, 150.0));
        assert_eq!(locked_body((400.0, 300.0), (4000.0, 100.0), Edge::Left, 0.5, 2.0), (800.0, 600.0));
        assert_eq!(Edge::from_wmsz(8), Some(Edge::BottomRight));
        assert_eq!(Edge::from_wmsz(0), None);
    }
}
