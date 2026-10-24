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
    /// "Keys": send SoundCraft's shortcut keys to the app while the editor has focus.
    Keys,
    Bypass,
    /// "Δ Delta": wet minus dry.
    Delta,
}

/// Where the header's buttons are for a width and dpi: `… [Keys] [Bypass] [Δ Delta]`,
/// right-aligned (next to the close button above them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub height: i32,
    pub keys: Rect,
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
    let keys_w = px(52, dpi);
    let right = width.max(pad + keys_w + pad + bypass_w + pad + delta_w + pad) - pad;
    let delta = Rect { left: right - delta_w, top, right, bottom };
    let bypass = Rect { left: delta.left - pad - bypass_w, top, right: delta.left - pad, bottom };
    let keys = Rect { left: bypass.left - pad - keys_w, top, right: bypass.left - pad, bottom };
    Layout { height: h, keys, bypass, delta }
}

/// The button under a header point.
pub fn hit(l: &Layout, x: i32, y: i32) -> Option<Button> {
    [(l.keys, Button::Keys), (l.bypass, Button::Bypass), (l.delta, Button::Delta)].into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, b)| b)
}

/// The Keys button's tooltip.
pub const KEYS_TIP: &str = "Send keys to SoundCraft (Space = play/stop)";

/// Which key presses an editor window sends to SoundCraft while Keys is on, as the egui key name
/// the app's shortcut handling knows (`None`: the plugin gets the key). `vk` is a Windows
/// virtual-key code.
///
/// Plain keys: only the transport and navigation keys a plugin rarely needs (Space, Enter, Home,
/// End, F1-F12, the numeric keypad), so typing into a plugin's text fields still works. With Ctrl
/// or Alt held: also letters, the number row, arrows and the rest (SoundCraft's menu shortcuts
/// such as Ctrl+S, Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y). Escape and Tab always stay with the plugin.
pub fn forward_key(vk: u32, ctrl: bool, alt: bool) -> Option<&'static str> {
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    const LETTERS: [&str; 26] =
        ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z"];
    const FKEYS: [&str; 12] = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"];
    let i = |base: u32| (vk - base) as usize;
    let always = match vk {
        0x20 => Some("Space"),
        0x0D => Some("Enter"),
        0x24 => Some("Home"),
        0x23 => Some("End"),
        0x60..=0x69 => DIGITS.get(i(0x60)).copied(), // numeric keypad 0-9
        0x6B => Some("Plus"),                        // keypad +
        0x6D => Some("Minus"),                       // keypad -
        0x6E => Some("Period"),                      // keypad .
        0x70..=0x7B => FKEYS.get(i(0x70)).copied(),
        _ => None,
    };
    if always.is_some() {
        return always;
    }
    if !(ctrl || alt) {
        return None;
    }
    match vk {
        0x30..=0x39 => DIGITS.get(i(0x30)).copied(),
        0x41..=0x5A => LETTERS.get(i(0x41)).copied(),
        0x25 => Some("ArrowLeft"),
        0x26 => Some("ArrowUp"),
        0x27 => Some("ArrowRight"),
        0x28 => Some("ArrowDown"),
        0x2E => Some("Delete"),
        0x08 => Some("Backspace"),
        0xBB => Some("Equals"),
        0xBD => Some("Minus"),
        0xBC => Some("Comma"),
        0xBE => Some("Period"),
        0xDB => Some("OpenBracket"),
        0xDD => Some("CloseBracket"),
        _ => None,
    }
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
    fn keys_sits_left_of_bypass() {
        for dpi in [96, 144, 192] {
            let l = layout(800, dpi);
            assert!(l.keys.left >= 0 && l.keys.right < l.bypass.left);
            assert_eq!(l.keys.top, l.bypass.top);
        }
        let l = layout(10, 96);
        assert!(l.keys.left >= 0 && l.keys.right < l.bypass.left, "too narrow: still in order");
    }

    #[test]
    fn which_keys_go_to_soundcraft() {
        // Transport and navigation keys, plain or with modifiers.
        assert_eq!(forward_key(0x20, false, false), Some("Space"));
        assert_eq!(forward_key(0x0D, false, false), Some("Enter"));
        assert_eq!(forward_key(0x24, false, false), Some("Home"));
        assert_eq!(forward_key(0x23, false, false), Some("End"));
        assert_eq!(forward_key(0x61, false, false), Some("1"), "keypad digits");
        assert_eq!(forward_key(0x6B, false, false), Some("Plus"));
        assert_eq!(forward_key(0x71, false, false), Some("F2"));
        // Typing stays with the plugin.
        assert_eq!(forward_key(0x41, false, false), None, "a plain letter");
        assert_eq!(forward_key(0x31, false, false), None, "a plain number-row digit");
        assert_eq!(forward_key(0x25, false, false), None, "a plain arrow");
        assert_eq!(forward_key(0x08, false, false), None, "plain backspace");
        // SoundCraft's menu shortcuts.
        assert_eq!(forward_key(0x53, true, false), Some("S"), "Ctrl+S");
        assert_eq!(forward_key(0x5A, true, false), Some("Z"), "Ctrl+Z (and Ctrl+Shift+Z)");
        assert_eq!(forward_key(0x59, true, false), Some("Y"));
        assert_eq!(forward_key(0x31, true, false), Some("1"));
        assert_eq!(forward_key(0xBB, true, false), Some("Equals"));
        // Never: Escape, Tab, modifiers on their own.
        for vk in [0x1B, 0x09, 0x10, 0x11, 0x12] {
            assert_eq!(forward_key(vk, true, false), None, "{vk:#x}");
            assert_eq!(forward_key(vk, false, false), None, "{vk:#x}");
        }
    }

    #[test]
    fn hits_find_each_button() {
        for dpi in [96, 144, 192] {
            let l = layout(640, dpi);
            let mid = |r: Rect| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            for (r, b) in [(l.keys, Button::Keys), (l.bypass, Button::Bypass), (l.delta, Button::Delta)] {
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
