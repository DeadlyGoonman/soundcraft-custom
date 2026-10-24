//! The header strip on top of every plugin editor window: platform-free layout, hit testing, the
//! scale menu and frame-drag helpers. The Win32 window that draws it lives in `frame`.

use soundcraft_dsp::{EditorHeaderState, EditorScale, EditorScaleMode, UpscaleFilter};

/// Header height at 96 dpi, in pixels.
pub const HEADER_DIP: i32 = 28;

/// Scale presets offered in the header menu, in percent.
pub const PRESETS: [u32; 10] = [50, 75, 100, 125, 150, 175, 200, 250, 300, 400];

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
    /// "150% ▾": opens the scale menu.
    Scale,
    Bypass,
    /// "Δ Delta": wet minus dry.
    Delta,
}

/// Where the header's buttons are for a width and dpi: `[Scale ▾] … [Bypass] [Δ Delta]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub height: i32,
    pub scale: Rect,
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
    let scale = Rect { left: pad, top, right: pad + px(76, dpi), bottom };
    let delta_w = px(68, dpi);
    let bypass_w = px(64, dpi);
    let right = width.max(scale.right + pad + bypass_w + pad + delta_w + pad) - pad;
    let delta = Rect { left: right - delta_w, top, right, bottom };
    let bypass = Rect { left: delta.left - pad - bypass_w, top, right: delta.left - pad, bottom };
    Layout { height: h, scale, bypass, delta }
}

/// The button under a header point.
pub fn hit(l: &Layout, x: i32, y: i32) -> Option<Button> {
    [(l.scale, Button::Scale), (l.bypass, Button::Bypass), (l.delta, Button::Delta)].into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, b)| b)
}

/// Delta needs a dry signal: unavailable on instruments and while bypassed.
pub fn delta_enabled(s: &EditorHeaderState) -> bool {
    !s.bypass && !s.is_instrument
}

/// The scale button's text, e.g. "150% ▾".
pub fn scale_label(scale: EditorScale) -> String {
    format!("{}% \u{25BE}", (scale.clamped_factor() * 100.0).round() as u32)
}

/// One choice of the scale menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuChoice {
    Percent(u32),
    Mode(EditorScaleMode),
}

/// The modes offered (NIS is not: it falls back to FSR 1).
pub const MODES: [(EditorScaleMode, &str); 4] = [
    (EditorScaleMode::Auto, "Auto (native, else FSR 1)"),
    (EditorScaleMode::Native, "Native (plugin draws)"),
    (EditorScaleMode::Upscale(UpscaleFilter::Fsr1), "Upscale: FSR 1"),
    (EditorScaleMode::Upscale(UpscaleFilter::Bilinear), "Upscale: Bilinear"),
];

const MODE_BASE: u32 = 1;
const PERCENT_BASE: u32 = 1000;

/// The Win32 menu command id of a choice (never 0).
pub fn menu_id(c: MenuChoice) -> u32 {
    match c {
        MenuChoice::Percent(p) => PERCENT_BASE + p,
        MenuChoice::Mode(m) => MODE_BASE + MODES.iter().position(|(x, _)| *x == shown_mode(m)).unwrap_or(0) as u32,
    }
}

/// The choice behind a menu command id.
pub fn menu_choice(id: u32) -> Option<MenuChoice> {
    if id > PERCENT_BASE {
        let p = id - PERCENT_BASE;
        return PRESETS.contains(&p).then_some(MenuChoice::Percent(p));
    }
    let i = id.checked_sub(MODE_BASE)? as usize;
    MODES.get(i).map(|(m, _)| MenuChoice::Mode(*m))
}

/// NIS shows as FSR 1 (what it falls back to).
fn shown_mode(m: EditorScaleMode) -> EditorScaleMode {
    match m {
        EditorScaleMode::Upscale(UpscaleFilter::Nis) => EditorScaleMode::Upscale(UpscaleFilter::Fsr1),
        m => m,
    }
}

/// One entry of the scale menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
    /// Draw a separator above this entry.
    pub separator: bool,
}

/// The scale menu for the header state: the presets (current one checked), then the modes.
pub fn menu_items(s: &EditorHeaderState) -> Vec<MenuItem> {
    let cur = (s.scale.clamped_factor() * 100.0).round() as u32;
    let mut out: Vec<MenuItem> = PRESETS
        .iter()
        .map(|&p| MenuItem { id: menu_id(MenuChoice::Percent(p)), label: format!("{p}%"), checked: p == cur, enabled: true, separator: false })
        .collect();
    let mode = shown_mode(s.scale.mode);
    for (i, (m, label)) in MODES.iter().enumerate() {
        out.push(MenuItem {
            id: menu_id(MenuChoice::Mode(*m)),
            label: (*label).to_string(),
            checked: *m == mode,
            enabled: *m != EditorScaleMode::Native || s.native_scaling,
            separator: i == 0,
        });
    }
    out
}

/// The scale after picking `c` with `cur` in effect.
pub fn apply_choice(cur: EditorScale, c: MenuChoice) -> EditorScale {
    match c {
        MenuChoice::Percent(p) => EditorScale { factor: p as f32 / 100.0, mode: cur.mode },
        MenuChoice::Mode(mode) => EditorScale { factor: cur.clamped_factor(), mode },
    }
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

    #[test]
    fn layout_scales_with_dpi_and_stays_inside() {
        for (dpi, h) in [(96, 28), (144, 42), (192, 56)] {
            let l = layout(800, dpi);
            assert_eq!(l.height, h);
            assert_eq!(l.scale.left, px(4, dpi));
            assert_eq!(l.delta.right, 800 - px(4, dpi));
            assert!(l.scale.right < l.bypass.left && l.bypass.right < l.delta.left);
            for r in [l.scale, l.bypass, l.delta] {
                assert!(r.top >= 0 && r.bottom <= h && r.left < r.right);
            }
        }
        // Too narrow: the buttons still don't overlap (the header just runs past the window).
        let l = layout(50, 96);
        assert!(l.scale.right < l.bypass.left);
    }

    #[test]
    fn hits_find_each_button() {
        for dpi in [96, 144, 192] {
            let l = layout(640, dpi);
            let mid = |r: Rect| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            for (r, b) in [(l.scale, Button::Scale), (l.bypass, Button::Bypass), (l.delta, Button::Delta)] {
                let (x, y) = mid(r);
                assert_eq!(hit(&l, x, y), Some(b), "{dpi}");
            }
            assert_eq!(hit(&l, 320, l.height / 2), None, "the gap");
            assert_eq!(hit(&l, l.scale.left, 0), None, "above the buttons");
        }
    }

    #[test]
    fn menu_ids_round_trip() {
        for p in PRESETS {
            assert_eq!(menu_choice(menu_id(MenuChoice::Percent(p))), Some(MenuChoice::Percent(p)));
        }
        for (m, _) in MODES {
            assert_eq!(menu_choice(menu_id(MenuChoice::Mode(m))), Some(MenuChoice::Mode(m)));
        }
        assert_eq!(menu_choice(0), None);
        assert_eq!(menu_choice(PERCENT_BASE + 333), None);
        let ids: std::collections::HashSet<u32> = menu_items(&EditorHeaderState::default()).iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), PRESETS.len() + MODES.len());
    }

    #[test]
    fn menu_checks_the_current_scale_and_hides_nis() {
        let s = EditorHeaderState { scale: EditorScale { factor: 1.5, mode: EditorScaleMode::Upscale(UpscaleFilter::Nis) }, ..Default::default() };
        let items = menu_items(&s);
        let checked: Vec<&str> = items.iter().filter(|i| i.checked).map(|i| i.label.as_str()).collect();
        assert_eq!(checked, ["150%", "Upscale: FSR 1"]);
        assert!(!items.iter().any(|i| i.label.contains("NIS")));
        let native = items.iter().find(|i| i.label.starts_with("Native")).map(|i| i.enabled);
        assert_eq!(native, Some(false), "no native scaling: Native is greyed");
        assert_eq!(scale_label(s.scale), "150% \u{25BE}");
    }

    #[test]
    fn choices_change_factor_or_mode() {
        let cur = EditorScale { factor: 1.25, mode: EditorScaleMode::Native };
        assert_eq!(apply_choice(cur, MenuChoice::Percent(200)), EditorScale { factor: 2.0, mode: EditorScaleMode::Native });
        assert_eq!(apply_choice(cur, MenuChoice::Mode(EditorScaleMode::Auto)), EditorScale { factor: 1.25, mode: EditorScaleMode::Auto });
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
