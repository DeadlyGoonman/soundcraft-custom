//! Design tokens. Colours were chosen by eye to give a dark, studio-style look; they are ours.

use egui::{Color32, FontFamily, FontId};
use std::sync::{
    PoisonError, RwLock,
    atomic::{AtomicU32, Ordering},
};

/// The colour palette in use. Dark is the default (the incumbent is dark-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Dark,
    Light,
    /// Dark with pure-black surfaces, for OLED panels.
    Oled,
    /// Surfaces shaded from a user colour (see [`custom_tokens`]).
    Custom([u8; 3]),
}

impl Palette {
    /// Code in the low byte, the custom colour (R, G, B) in the three bytes above it.
    const fn pack(self) -> u32 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
            Self::Oled => 2,
            Self::Custom([r, g, b]) => 3 | ((r as u32) << 8) | ((g as u32) << 16) | ((b as u32) << 24),
        }
    }

    const fn unpack(value: u32) -> Self {
        let [code, r, g, b] = value.to_le_bytes();
        match code {
            1 => Self::Light,
            2 => Self::Oled,
            3 => Self::Custom([r, g, b]),
            _ => Self::Dark,
        }
    }
}

/// The active [`Palette`], as its [`Palette::pack`].
static ACTIVE: AtomicU32 = AtomicU32::new(0);

/// The tokens of the last custom colour applied, so frames do not recompute them.
static CUSTOM_CACHE: RwLock<Option<([u8; 3], Tokens)>> = RwLock::new(None);

/// Unit tests that change or read the process-wide palette must own it until their assertions
/// finish. The lock is re-entrant per thread, so a test may hold it and still build apps
/// (`SoundApp::new` takes it too) without deadlocking itself.
#[cfg(test)]
pub(crate) fn lock_test_palette() -> TestPaletteGuard {
    TestPaletteGuard::acquire()
}

/// RAII handle for the re-entrant, per-thread test palette lock; see [`lock_test_palette`].
#[cfg(test)]
pub(crate) struct TestPaletteGuard;

#[cfg(test)]
impl TestPaletteGuard {
    fn acquire() -> Self {
        palette_lock::acquire();
        Self
    }
}

#[cfg(test)]
impl Drop for TestPaletteGuard {
    fn drop(&mut self) {
        palette_lock::release();
    }
}

#[cfg(test)]
mod palette_lock {
    use std::cell::{Cell, RefCell};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    static PALETTE: Mutex<()> = Mutex::new(());

    thread_local! {
        /// How many guards this thread holds; the OS lock is taken on 0→1 and released on 1→0.
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        static GUARD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }

    pub(super) fn acquire() {
        DEPTH.with(|depth| {
            if depth.get() == 0 {
                GUARD.with(|guard| *guard.borrow_mut() = Some(PALETTE.lock().unwrap_or_else(PoisonError::into_inner)));
            }
            depth.set(depth.get() + 1);
        });
    }

    pub(super) fn release() {
        DEPTH.with(|depth| {
            let left = depth.get().saturating_sub(1);
            depth.set(left);
            if left == 0 {
                GUARD.with(|guard| *guard.borrow_mut() = None);
            }
        });
    }
}

/// The Appearance setting (`ui.theme {mode}`). Dark by default; OLED, System and Light are opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    System,
    Light,
    #[default]
    Dark,
    Oled,
    /// Surfaces shaded from the custom colour saved in the UI prefs (`custom_bg`).
    Custom,
}

impl ThemeMode {
    pub const ALL: [Self; 5] = [Self::Dark, Self::Oled, Self::Light, Self::System, Self::Custom];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Oled => "OLED Black",
            Self::Custom => "Custom",
        }
    }

    /// The id used in `ui.theme {"mode": …}` and in the saved UI prefs.
    pub const fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Oled => "oled",
            Self::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id().eq_ignore_ascii_case(s.trim()))
    }
}

/// Soft Pink, the default custom colour.
pub const SOFT_PINK: [u8; 3] = [0xDD, 0xB5, 0xC7];
/// Dark Pink, a preset.
pub const DARK_PINK: [u8; 3] = [0xBE, 0x86, 0xA3];
/// The Custom popup's presets: name and colour. "Grey (Dark)" is Dark's `panel_bg`.
pub const CUSTOM_PRESETS: [(&str, [u8; 3]); 4] =
    [("Soft Pink", SOFT_PINK), ("Dark Pink", DARK_PINK), ("Grey (Dark)", [37, 37, 38]), ("OLED Black", [0, 0, 0])];

/// Parse `#RRGGBB`, `RRGGBB`, `#RGB` or `RGB` (hex digits only, either case). Anything else is `None`.
pub fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let s = text.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    // Hex digits are ASCII, so every byte offset below is a char boundary; this also rejects signs
    // such as `+f00` that `from_str_radix` would accept, and empty or non-ASCII input.
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let nibble = |i: usize| s.get(i..i.checked_add(1)?).and_then(|h| u8::from_str_radix(h, 16).ok());
    let byte = |i: usize| s.get(i..i.checked_add(2)?).and_then(|h| u8::from_str_radix(h, 16).ok());
    match s.len() {
        3 => Some([nibble(0)?.saturating_mul(17), nibble(1)?.saturating_mul(17), nibble(2)?.saturating_mul(17)]),
        6 => Some([byte(0)?, byte(2)?, byte(4)?]),
        _ => None,
    }
}

/// `#RRGGBB`, upper case.
pub fn format_hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub window_bg: Color32,
    pub panel_bg: Color32,
    pub panel_bg2: Color32,
    pub toolbar_bg: Color32,
    pub toolbar_group: Color32,
    pub border: Color32,
    pub border_light: Color32,
    /// The lit top/left edge of raised panels: track headers and Mix strips.
    pub edge_light: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_dark: Color32,
    pub header_text: Color32,
    pub counter_bg: Color32,
    pub counter_text: Color32,
    pub counter_label: Color32,
    pub accent: Color32,
    pub accent_dark: Color32,
    pub mode_on: Color32,
    pub mode_on_text: Color32,
    pub mode_off_text: Color32,
    pub button: Color32,
    pub button_hi: Color32,
    pub button_border: Color32,
    pub name_field: Color32,
    pub name_field_sel: Color32,
    pub ruler_bg: Color32,
    pub ruler_label_bg: Color32,
    pub ruler_text: Color32,
    pub ruler_tick: Color32,
    pub tempo_ruler: Color32,
    pub meter_ruler: Color32,
    pub marker_ruler: Color32,
    pub playlist_bg: Color32,
    pub playlist_alt: Color32,
    pub grid_line: Color32,
    pub bar_line: Color32,
    pub selection: Color32,
    pub playhead: Color32,
    pub insertion: Color32,
    pub solo: Color32,
    pub mute: Color32,
    pub rec: Color32,
    pub input: Color32,
    pub auto_read: Color32,
    pub auto_write: Color32,
    pub auto_touch: Color32,
    pub auto_latch: Color32,
    pub meter_green: Color32,
    pub meter_yellow: Color32,
    pub meter_red: Color32,
    pub fader_track: Color32,
    pub strip_bg: Color32,
    pub strip_section: Color32,
    pub slot_bg: Color32,
    pub automation_line: Color32,
    /// Menu bar background.
    pub menu_bar: Color32,
    /// Outline of toolbar groups.
    pub group_border: Color32,
    /// Edit-mode cell, not selected.
    pub mode_off: Color32,
    /// Counter-panel toggle (Grid, Count Off), off.
    pub counter_off: Color32,
    /// Outline of the counter panels.
    pub counter_border: Color32,
    /// Divider inside the counter panels.
    pub counter_rule: Color32,
    /// Smart-tool bracket, off.
    pub tool_bracket_off: Color32,
    /// Selected row in the Tracks, Groups and Clips lists.
    pub row_selected: Color32,
    /// Track-visible dot in the Tracks list.
    pub visible_dot: Color32,
    /// Big Counter background.
    pub big_counter_bg: Color32,
    /// Edit-window track header, selected.
    pub header_selected: Color32,
    /// Mix-window strip, selected.
    pub strip_selected: Color32,
    /// Marker names on the marker ruler.
    pub marker_text: Color32,
    /// Alternate-playlist lanes.
    pub playlist_lane: Color32,
    /// Universe overview background.
    pub universe_bg: Color32,
    /// Universe visible-area outline.
    pub universe_view: Color32,
    /// Meter background.
    pub meter_bg: Color32,
    /// Meter clip indicator, not clipped.
    pub meter_clip_off: Color32,
    /// Pan knob body.
    pub knob_bg: Color32,
    /// Pan knob outline.
    pub knob_ring: Color32,
    /// Pan knob pointer.
    pub knob_pointer: Color32,
    /// Mix-window section captions (INSERTS A-E, I/O…).
    pub section_label: Color32,
    /// Plugin slots, as Pro Tools draws them: the well they sit in, an empty slot, a used slot's
    /// face (top, bottom), a bypassed one's, and the dots, letters and arrows on them.
    pub slot_well: Color32,
    pub slot_empty: Color32,
    pub slot_face: [Color32; 2],
    pub slot_bypassed: [Color32; 2],
    pub slot_mark: Color32,
}

impl Tokens {
    pub const DARK: Tokens = Tokens {
        window_bg: Color32::from_rgb(30, 30, 30),
        panel_bg: Color32::from_rgb(37, 37, 38),
        panel_bg2: Color32::from_rgb(44, 44, 45),
        toolbar_bg: Color32::from_rgb(30, 30, 31),
        toolbar_group: Color32::from_rgb(24, 24, 25),
        border: Color32::from_rgb(14, 14, 14),
        border_light: Color32::from_rgb(64, 64, 66),
        edge_light: Color32::from_rgba_premultiplied(20, 20, 20, 20),
        text: Color32::from_rgb(214, 214, 214),
        text_dim: Color32::from_rgb(150, 150, 152),
        text_dark: Color32::from_rgb(16, 16, 16),
        header_text: Color32::from_rgb(196, 196, 198),
        counter_bg: Color32::from_rgb(4, 4, 4),
        counter_text: Color32::from_rgb(104, 220, 120),
        counter_label: Color32::from_rgb(200, 200, 200),
        accent: Color32::from_rgb(64, 132, 196),
        accent_dark: Color32::from_rgb(40, 92, 146),
        mode_on: Color32::from_rgb(92, 196, 108),
        mode_on_text: Color32::from_rgb(10, 30, 12),
        mode_off_text: Color32::from_rgb(98, 196, 112),
        button: Color32::from_rgb(58, 58, 60),
        button_hi: Color32::from_rgb(78, 78, 80),
        button_border: Color32::from_rgb(20, 20, 20),
        name_field: Color32::from_rgb(208, 208, 208),
        name_field_sel: Color32::from_rgb(232, 232, 232),
        ruler_bg: Color32::from_rgb(40, 40, 41),
        ruler_label_bg: Color32::from_rgb(40, 40, 41),
        ruler_text: Color32::from_rgb(178, 178, 180),
        ruler_tick: Color32::from_rgb(96, 96, 98),
        tempo_ruler: Color32::from_rgb(52, 128, 88),
        meter_ruler: Color32::from_rgb(44, 108, 140),
        marker_ruler: Color32::from_rgb(46, 46, 48),
        playlist_bg: Color32::from_rgb(36, 36, 37),
        playlist_alt: Color32::from_rgb(40, 40, 41),
        grid_line: Color32::from_rgb(52, 52, 54),
        bar_line: Color32::from_rgb(66, 66, 70),
        selection: Color32::from_rgba_premultiplied(40, 70, 110, 110),
        playhead: Color32::from_rgb(232, 60, 50),
        insertion: Color32::from_rgb(230, 230, 230),
        solo: Color32::from_rgb(222, 196, 52),
        mute: Color32::from_rgb(232, 148, 40),
        rec: Color32::from_rgb(214, 52, 46),
        input: Color32::from_rgb(70, 170, 90),
        auto_read: Color32::from_rgb(96, 200, 110),
        auto_write: Color32::from_rgb(220, 70, 60),
        auto_touch: Color32::from_rgb(230, 170, 50),
        auto_latch: Color32::from_rgb(160, 120, 220),
        meter_green: Color32::from_rgb(60, 200, 80),
        meter_yellow: Color32::from_rgb(230, 210, 60),
        meter_red: Color32::from_rgb(230, 50, 40),
        fader_track: Color32::from_rgb(12, 12, 12),
        strip_bg: Color32::from_rgb(46, 46, 48),
        strip_section: Color32::from_rgb(36, 36, 38),
        slot_bg: Color32::from_rgb(28, 28, 29),
        automation_line: Color32::from_rgb(240, 240, 240),
        menu_bar: Color32::from_rgb(22, 22, 23),
        group_border: Color32::from_rgb(8, 8, 8),
        mode_off: Color32::from_rgb(20, 36, 22),
        counter_off: Color32::from_rgb(30, 50, 32),
        counter_border: Color32::from_rgb(50, 50, 50),
        counter_rule: Color32::from_rgb(40, 40, 40),
        tool_bracket_off: Color32::from_rgb(64, 64, 66),
        row_selected: Color32::from_rgb(52, 70, 96),
        visible_dot: Color32::from_rgb(200, 200, 200),
        big_counter_bg: Color32::from_rgb(0, 0, 0),
        header_selected: Color32::from_rgb(52, 58, 66),
        strip_selected: Color32::from_rgb(54, 58, 64),
        marker_text: Color32::from_rgb(236, 236, 236),
        playlist_lane: Color32::from_rgb(30, 30, 31),
        universe_bg: Color32::from_rgb(18, 18, 19),
        universe_view: Color32::from_rgb(255, 255, 255),
        meter_bg: Color32::from_rgb(8, 8, 8),
        meter_clip_off: Color32::from_rgb(60, 20, 20),
        knob_bg: Color32::from_rgb(26, 26, 28),
        knob_ring: Color32::from_rgb(150, 150, 154),
        knob_pointer: Color32::from_rgb(230, 230, 230),
        section_label: Color32::from_rgb(200, 200, 200),
        slot_well: Color32::from_rgb(43, 43, 43),
        slot_empty: Color32::from_rgb(39, 39, 39),
        slot_face: [Color32::from_rgb(92, 92, 92), Color32::from_rgb(75, 75, 75)],
        slot_bypassed: [Color32::from_rgb(66, 96, 146), Color32::from_rgb(50, 76, 120)],
        slot_mark: Color32::from_rgb(159, 159, 159),
    };

    /// Dark with pure-black surfaces. Only background-ish tokens change: main panels and the
    /// playlist are `#000000`, secondary surfaces are a near-black grey so hierarchy and widget
    /// edges stay visible, and borders are lifted so they still read against black. Text, accent,
    /// meters, selection and track colours are Dark's.
    pub const OLED: Tokens = Tokens {
        window_bg: Color32::from_rgb(0, 0, 0),
        panel_bg: Color32::from_rgb(0, 0, 0),
        panel_bg2: Color32::from_rgb(10, 10, 10),
        toolbar_bg: Color32::from_rgb(0, 0, 0),
        toolbar_group: Color32::from_rgb(10, 10, 10),
        border: Color32::from_rgb(28, 28, 28),
        group_border: Color32::from_rgb(30, 30, 30),
        counter_bg: Color32::from_rgb(0, 0, 0),
        button: Color32::from_rgb(22, 22, 22),
        button_hi: Color32::from_rgb(36, 36, 36),
        button_border: Color32::from_rgb(80, 80, 80),
        ruler_bg: Color32::from_rgb(10, 10, 10),
        ruler_label_bg: Color32::from_rgb(10, 10, 10),
        marker_ruler: Color32::from_rgb(10, 10, 10),
        playlist_bg: Color32::from_rgb(0, 0, 0),
        playlist_alt: Color32::from_rgb(10, 10, 10),
        playlist_lane: Color32::from_rgb(10, 10, 10),
        strip_bg: Color32::from_rgb(20, 20, 20),
        strip_section: Color32::from_rgb(10, 10, 10),
        slot_bg: Color32::from_rgb(10, 10, 10),
        slot_well: Color32::from_rgb(20, 20, 20),
        slot_empty: Color32::from_rgb(17, 17, 17),
        menu_bar: Color32::from_rgb(0, 0, 0),
        universe_bg: Color32::from_rgb(0, 0, 0),
        meter_bg: Color32::from_rgb(0, 0, 0),
        knob_bg: Color32::from_rgb(20, 20, 20),
        ..Self::DARK
    };

    pub const LIGHT: Tokens = Tokens {
        window_bg: Color32::from_rgb(184, 186, 190),
        panel_bg: Color32::from_rgb(204, 206, 210),
        panel_bg2: Color32::from_rgb(216, 218, 222),
        toolbar_bg: Color32::from_rgb(194, 196, 200),
        toolbar_group: Color32::from_rgb(208, 210, 214),
        border: Color32::from_rgb(100, 103, 109),
        border_light: Color32::from_rgb(150, 153, 159),
        edge_light: Color32::from_rgba_premultiplied(150, 150, 150, 150),
        text: Color32::from_rgb(24, 25, 28),
        text_dim: Color32::from_rgb(68, 70, 76),
        text_dark: Color32::from_rgb(16, 17, 20),
        header_text: Color32::from_rgb(30, 32, 36),
        counter_bg: Color32::from_rgb(224, 226, 228),
        counter_text: Color32::from_rgb(18, 108, 40),
        counter_label: Color32::from_rgb(54, 57, 63),
        accent: Color32::from_rgb(62, 116, 180),
        accent_dark: Color32::from_rgb(43, 88, 145),
        mode_on: Color32::from_rgb(75, 158, 88),
        mode_on_text: Color32::from_rgb(255, 255, 255),
        mode_off_text: Color32::from_rgb(38, 106, 52),
        button: Color32::from_rgb(214, 216, 220),
        button_hi: Color32::from_rgb(200, 203, 208),
        button_border: Color32::from_rgb(108, 111, 117),
        name_field: Color32::from_rgb(236, 237, 239),
        name_field_sel: Color32::from_rgb(224, 232, 244),
        ruler_bg: Color32::from_rgb(198, 200, 204),
        ruler_label_bg: Color32::from_rgb(198, 200, 204),
        ruler_text: Color32::from_rgb(48, 50, 56),
        ruler_tick: Color32::from_rgb(108, 111, 118),
        tempo_ruler: Color32::from_rgb(92, 156, 112),
        meter_ruler: Color32::from_rgb(80, 144, 172),
        marker_ruler: Color32::from_rgb(186, 189, 194),
        playlist_bg: Color32::from_rgb(210, 212, 216),
        playlist_alt: Color32::from_rgb(202, 204, 208),
        grid_line: Color32::from_rgb(188, 191, 196),
        bar_line: Color32::from_rgb(162, 166, 172),
        selection: Color32::from_rgba_premultiplied(100, 145, 205, 90),
        playhead: Color32::from_rgb(210, 48, 42),
        insertion: Color32::from_rgb(35, 36, 40),
        solo: Color32::from_rgb(178, 145, 20),
        mute: Color32::from_rgb(194, 106, 22),
        rec: Color32::from_rgb(190, 42, 38),
        input: Color32::from_rgb(45, 126, 64),
        auto_read: Color32::from_rgb(52, 142, 68),
        auto_write: Color32::from_rgb(190, 50, 45),
        auto_touch: Color32::from_rgb(180, 122, 24),
        auto_latch: Color32::from_rgb(118, 80, 170),
        meter_green: Color32::from_rgb(36, 160, 58),
        meter_yellow: Color32::from_rgb(190, 162, 20),
        meter_red: Color32::from_rgb(195, 42, 35),
        fader_track: Color32::from_rgb(120, 123, 129),
        strip_bg: Color32::from_rgb(198, 200, 204),
        strip_section: Color32::from_rgb(210, 212, 216),
        slot_bg: Color32::from_rgb(194, 196, 201),
        automation_line: Color32::from_rgb(30, 32, 38),
        menu_bar: Color32::from_rgb(204, 206, 210),
        group_border: Color32::from_rgb(118, 121, 127),
        mode_off: Color32::from_rgb(202, 212, 204),
        counter_off: Color32::from_rgb(192, 212, 198),
        counter_border: Color32::from_rgb(118, 121, 127),
        counter_rule: Color32::from_rgb(188, 191, 196),
        tool_bracket_off: Color32::from_rgb(172, 175, 181),
        row_selected: Color32::from_rgb(174, 196, 226),
        visible_dot: Color32::from_rgb(86, 89, 96),
        big_counter_bg: Color32::from_rgb(224, 226, 228),
        header_selected: Color32::from_rgb(184, 200, 224),
        strip_selected: Color32::from_rgb(184, 200, 224),
        marker_text: Color32::from_rgb(24, 25, 28),
        playlist_lane: Color32::from_rgb(200, 202, 206),
        universe_bg: Color32::from_rgb(198, 200, 204),
        universe_view: Color32::from_rgb(40, 42, 48),
        meter_bg: Color32::from_rgb(52, 54, 58),
        meter_clip_off: Color32::from_rgb(92, 54, 54),
        knob_bg: Color32::from_rgb(232, 233, 236),
        knob_ring: Color32::from_rgb(116, 120, 128),
        knob_pointer: Color32::from_rgb(30, 32, 38),
        section_label: Color32::from_rgb(56, 58, 64),
        slot_well: Color32::from_rgb(212, 214, 218),
        slot_empty: Color32::from_rgb(198, 200, 205),
        slot_face: [Color32::from_rgb(244, 245, 247), Color32::from_rgb(214, 216, 220)],
        slot_bypassed: [Color32::from_rgb(190, 210, 240), Color32::from_rgb(160, 186, 228)],
        slot_mark: Color32::from_rgb(92, 95, 102),
    };

    pub fn current() -> Tokens {
        match active_palette() {
            Palette::Light => Self::LIGHT,
            Palette::Oled => Self::OLED,
            Palette::Dark => Self::DARK,
            Palette::Custom(bg) => custom_cached(bg),
        }
    }
}

/// The palette that is currently applied.
pub fn active_palette() -> Palette {
    Palette::unpack(ACTIVE.load(Ordering::Relaxed))
}

/// Whether the palette is a light one (Light, or a custom colour that is light).
pub fn is_light() -> bool {
    palette_is_light(active_palette())
}

fn palette_is_light(palette: Palette) -> bool {
    match palette {
        Palette::Light => true,
        Palette::Custom(bg) => custom_is_light(rgb(bg)),
        Palette::Dark | Palette::Oled => false,
    }
}

/// The palette a mode resolves to this frame. System follows the OS appearance; unknown uses Dark.
/// `custom` is the saved custom colour, used by [`ThemeMode::Custom`].
pub fn palette_for(ctx: &egui::Context, mode: ThemeMode, custom: [u8; 3]) -> Palette {
    match mode {
        ThemeMode::Light => Palette::Light,
        ThemeMode::Dark => Palette::Dark,
        ThemeMode::Oled => Palette::Oled,
        ThemeMode::Custom => Palette::Custom(custom),
        ThemeMode::System if ctx.system_theme() == Some(egui::Theme::Light) => Palette::Light,
        ThemeMode::System => Palette::Dark,
    }
}

/// The text colours a custom palette may use: pure white and pure black.
const INK_LIGHT: Color32 = Color32::WHITE;
const INK_DARK: Color32 = Color32::BLACK;
/// The contrast every primary text colour keeps on the surfaces it sits on (WCAG AA).
const TEXT_MIN_CONTRAST: f64 = 4.5;
/// The contrast weaker text keeps on the surfaces it sits on.
const WEAK_TEXT_MIN_CONTRAST: f64 = 3.0;

/// WCAG relative luminance of an sRGB colour (0.0 to 1.0).
fn relative_luminance(c: Color32) -> f64 {
    let channel = |v: u8| {
        let s = f64::from(v) / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// WCAG contrast ratio between two colours (1.0 to 21.0).
pub fn contrast(a: Color32, b: Color32) -> f64 {
    let (x, y) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if x >= y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// A custom colour is light when white text would have less contrast on it than black text.
pub fn custom_is_light(bg: Color32) -> bool {
    contrast(INK_LIGHT, bg) < contrast(INK_DARK, bg)
}

/// `from` moved `amount` (0..1) toward `toward`.
fn toward(from: Color32, toward: Color32, amount: f32) -> Color32 {
    from.lerp_to_gamma(toward, amount.clamp(0.0, 1.0))
}

/// A surface `amount` of the way from `bg` toward the text colour `ink`. If the text would lose
/// its contrast on it, the step is backed off toward `bg` (which always keeps it).
fn surface_toward(bg: Color32, ink: Color32, amount: f32) -> Color32 {
    const STEPS: u32 = 40;
    (0..=STEPS).rev().map(|i| toward(bg, ink, amount * i as f32 / STEPS as f32)).find(|c| contrast(ink, *c) >= TEXT_MIN_CONTRAST).unwrap_or(bg)
}

/// The text colour `ink` pulled toward `anchor` by the most that still keeps `WEAK_TEXT_MIN_CONTRAST`
/// on every surface in `surfaces`; `ink` itself when nothing weaker passes.
fn weak_text(ink: Color32, anchor: Color32, surfaces: &[Color32]) -> Color32 {
    (0..=4)
        .rev()
        .map(|i| toward(ink, anchor, 0.1 * i as f32))
        .find(|c| surfaces.iter().all(|s| contrast(*c, *s) >= WEAK_TEXT_MIN_CONTRAST))
        .unwrap_or(ink)
}

/// Tokens for a custom background colour. The base set (Light or Dark, by the colour's luminance)
/// keeps its accent, meter, selection and track colours. The background family is shades of `bg`:
/// panels are `bg`, secondary surfaces step toward the text colour, and borders step further. The
/// text colour is white or black, whichever contrasts more with `bg`, and every surface keeps
/// primary text at 4.5:1 or better.
pub fn custom_tokens(bg: Color32) -> Tokens {
    let light = custom_is_light(bg);
    let base = if light { Tokens::LIGHT } else { Tokens::DARK };
    let ink = if light { INK_DARK } else { INK_LIGHT };
    let surface = |k: f32| surface_toward(bg, ink, k);
    let line = |k: f32| toward(bg, ink, k);

    let panel2 = surface(0.06);
    let toolbar_group = surface(0.08);
    let button = surface(0.12);
    let button_hi = surface(0.2);
    let ruler = surface(0.06);
    let counter = surface(0.04);
    let strip_section = surface(0.05);
    let text_dim = weak_text(ink, bg, &[bg, panel2, button]);
    let counter_label = weak_text(ink, bg, &[counter]);
    let section_label = weak_text(ink, bg, &[strip_section]);
    let ruler_text = weak_text(ink, bg, &[ruler]);
    let visible_dot = weak_text(ink, bg, &[panel2]);

    Tokens {
        window_bg: bg,
        panel_bg: bg,
        playlist_bg: bg,
        panel_bg2: panel2,
        toolbar_bg: surface(0.03),
        toolbar_group,
        menu_bar: surface(0.02),
        border: line(0.22),
        border_light: line(0.16),
        edge_light: line(0.12),
        group_border: line(0.2),
        text: ink,
        text_dim,
        header_text: ink,
        marker_text: ink,
        automation_line: ink,
        insertion: ink,
        counter_bg: counter,
        counter_off: surface(0.1),
        counter_label,
        counter_border: line(0.2),
        counter_rule: line(0.12),
        big_counter_bg: surface(0.02),
        button,
        button_hi,
        button_border: line(0.3),
        name_field: surface(0.1),
        name_field_sel: surface(0.16),
        ruler_bg: ruler,
        ruler_label_bg: ruler,
        marker_ruler: ruler,
        ruler_text,
        ruler_tick: line(0.3),
        grid_line: line(0.08),
        bar_line: line(0.16),
        playlist_alt: surface(0.03),
        playlist_lane: surface(0.03),
        strip_bg: surface(0.08),
        strip_section,
        section_label,
        slot_bg: surface(0.04),
        slot_well: surface(0.08),
        slot_empty: surface(0.05),
        slot_face: [surface(0.14), surface(0.1)],
        fader_track: line(0.2),
        tool_bracket_off: line(0.2),
        visible_dot,
        universe_bg: surface(0.04),
        knob_bg: surface(0.1),
        ..base
    }
}

/// [`custom_tokens`] for `bg`, reusing the last result while the colour is unchanged.
fn custom_cached(bg: [u8; 3]) -> Tokens {
    let cached = *CUSTOM_CACHE.read().unwrap_or_else(PoisonError::into_inner);
    match cached {
        Some((colour, tokens)) if colour == bg => tokens,
        _ => custom_tokens(rgb(bg)),
    }
}

/// A track's header and lane backgrounds in the Edit window, tinted with its colour by `tint` (0..1).
pub fn track_backgrounds(color: [u8; 3], selected: bool, tint: f32) -> (Color32, Color32) {
    let t = Tokens::current();
    let k = tint.clamp(0.0, 1.0);
    if k <= 0.0 {
        return if selected { (t.header_selected, t.playlist_alt) } else { (t.panel_bg2, t.playlist_bg) };
    }
    let c = rgb(color);
    let header = t.panel_bg2.lerp_to_gamma(c, k * 0.55);
    let header = if selected { crate::widgets::shade(header, 0.1) } else { header };
    let lane = if selected { t.playlist_alt } else { t.playlist_bg }.lerp_to_gamma(c, k * 0.2);
    (header, lane)
}

/// Track colours tinted for clip bodies.
pub fn clip_colors(rgb: [u8; 3], selected: bool) -> (Color32, Color32, Color32) {
    let [r, g, b] = rgb;
    let shade = |k: f32| Color32::from_rgb((f32::from(r) * k) as u8, (f32::from(g) * k) as u8, (f32::from(b) * k) as u8);
    let tint = |k: f32| {
        let t = |c: u8| (f32::from(c) + (255.0 - f32::from(c)) * k) as u8;
        Color32::from_rgb(t(r), t(g), t(b))
    };
    let bar = shade(0.9);
    if is_light() {
        // A paler body and a waveform in a deep shade of the clip's own colour.
        return if selected { (tint(0.2), bar, shade(0.22)) } else { (tint(0.45), bar, shade(0.32)) };
    }
    // A dark body and a light waveform in the clip's own colour; selection inverts them.
    if selected { (shade(0.7), bar, Color32::from_rgb(16, 16, 18)) } else { (shade(0.34), bar, tint(0.35)) }
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// How far a track's colour pulls a surface toward itself (0 = the plain surface, 1 = the colour).
/// Strip sections and Edit-window columns.
pub const TRACK_TINT_SECTION: f32 = 0.22;
/// Empty insert and send cells.
pub const TRACK_TINT_EMPTY: f32 = 0.16;
/// Used (not bypassed) insert and send faces. Kept faint so their names stay readable.
pub const TRACK_TINT_FACE: f32 = 0.12;

/// `base` pulled toward the track's colour by `amount`: the track's identity without losing the
/// surface's contrast for text.
pub fn track_tint(base: Color32, track: [u8; 3], amount: f32) -> Color32 {
    base.lerp_to_gamma(rgb(track), amount)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Make `light` (or dark) the active palette and apply its visuals. Call only when it changes.
pub fn apply(ctx: &egui::Context, light: bool) {
    apply_palette(ctx, if light { Palette::Light } else { Palette::Dark });
}

/// Make `palette` the active one and apply its visuals. Call only when it changes.
pub fn apply_palette(ctx: &egui::Context, palette: Palette) {
    if let Palette::Custom(bg) = palette {
        let tokens = custom_tokens(rgb(bg));
        *CUSTOM_CACHE.write().unwrap_or_else(PoisonError::into_inner) = Some((bg, tokens));
    }
    ACTIVE.store(palette.pack(), Ordering::Relaxed);
    let t = Tokens::current();
    let mut v = if palette_is_light(palette) { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.panel_fill = t.panel_bg;
    v.window_fill = t.panel_bg2;
    v.extreme_bg_color = t.slot_bg;
    v.faint_bg_color = t.panel_bg2;
    if palette == Palette::Oled {
        v.code_bg_color = t.slot_bg;
    }
    v.selection.bg_fill = t.accent;
    v.widgets.inactive.weak_bg_fill = t.button;
    v.widgets.inactive.bg_fill = t.button;
    v.widgets.hovered.weak_bg_fill = t.button_hi;
    v.widgets.noninteractive.fg_stroke.color = t.text;
    v.widgets.inactive.fg_stroke.color = t.text;
    v.window_corner_radius = egui::CornerRadius::same(6);
    v.menu_corner_radius = egui::CornerRadius::same(4);
    // Keep receiving native OS appearance changes even when our palette is manual. On macOS,
    // pinning egui's theme also pins the window appearance and hides later OS changes.
    ctx.set_theme(egui::ThemePreference::System);
    // Both egui style branches must use our resolved palette: egui can select either branch
    // as the OS changes, including while SoundCraft's manual Light/Dark choice stays fixed.
    ctx.all_styles_mut(|s| {
        s.visuals = v.clone();
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(6.0, 2.0);
        s.interaction.tooltip_delay = 0.4;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Services, SoundApp, UiState};
    use serde_json::json;

    #[test]
    fn test_palette_lock_is_reentrant_and_excludes_other_threads() {
        // Regression for the parallel-run flakes: the process-wide palette must be owned for a
        // whole test, re-entrant on the owning thread (so `SoundApp::new` can take it too), and
        // exclusive against other threads (so a frame cannot flip it mid-test).
        let outer = lock_test_palette();
        let inner = lock_test_palette();
        let app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _held = lock_test_palette();
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(100)).is_err(), "another thread must wait for the palette lock");
        drop(inner);
        drop(outer);
        drop(app);
        // Other tests compete for the lock too, and some hold it for seconds: allow for a queue.
        assert!(rx.recv_timeout(std::time::Duration::from_secs(120)).is_ok(), "the palette lock must be released when every holder drops it");
    }

    #[test]
    fn dark_is_the_default() {
        assert_eq!(ThemeMode::default(), ThemeMode::Dark);
        assert_eq!(UiState::default().theme, ThemeMode::Dark);
        // Prefs saved before the setting existed load as Dark.
        let old: UiState = serde_json::from_value(json!({"show_tracks_list": true})).unwrap_or_default();
        assert_eq!(old.theme, ThemeMode::Dark);
    }

    #[test]
    fn prefs_saved_before_track_tint_get_the_default() {
        let ui: UiState = serde_json::from_value(json!({"theme": "light"})).unwrap();
        assert!((ui.track_tint - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn modes_round_trip_through_prefs() {
        for mode in ThemeMode::ALL {
            assert_eq!(ThemeMode::parse(mode.id()), Some(mode));
            let ui = UiState { theme: mode, ..UiState::default() };
            let back: UiState = serde_json::from_value(serde_json::to_value(&ui).unwrap_or_default()).unwrap_or_default();
            assert_eq!(back.theme, mode);
        }
        assert_eq!(ThemeMode::parse(" Light "), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("sepia"), None);
    }

    #[test]
    fn oled_is_a_black_dark_palette() {
        assert_eq!(ThemeMode::ALL[1], ThemeMode::Oled, "OLED sits right after Dark in the Appearance list");
        assert_eq!(ThemeMode::Oled.label(), "OLED Black");
        assert_eq!(ThemeMode::Oled.id(), "oled");
        assert_eq!(serde_json::to_value(ThemeMode::Oled).unwrap(), json!("oled"));
        assert_eq!(ThemeMode::parse("oled"), Some(ThemeMode::Oled));
        assert_eq!(ThemeMode::parse(" OLED "), Some(ThemeMode::Oled));

        let _palette = lock_test_palette();
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.run("ui.theme", json!({"mode": "oled"})), Ok(json!({"mode": "oled"})));
        assert_eq!(app.ui.theme, ThemeMode::Oled);

        let ctx = egui::Context::default();
        apply_palette(&ctx, Palette::Oled);
        assert_eq!(active_palette(), Palette::Oled);
        assert!(!is_light());
        let t = Tokens::current();
        assert_eq!(t.panel_bg, Color32::BLACK);
        assert_eq!(t.window_bg, Color32::BLACK);
        assert_eq!(t.playlist_bg, Color32::BLACK);
        let visuals = ctx.global_style().visuals.clone();
        assert_eq!(visuals.panel_fill, Color32::BLACK);
        assert!(visuals.dark_mode, "OLED is a dark mode");
        assert_eq!(visuals.extreme_bg_color, t.slot_bg);
        // Text, accent, meters, selection and track colours are Dark's.
        assert_eq!(t.text, Tokens::DARK.text);
        assert_eq!(t.accent, Tokens::DARK.accent);
        assert_eq!(t.meter_green, Tokens::DARK.meter_green);
        assert_eq!(t.selection, Tokens::DARK.selection);
        // Secondary surfaces stay visibly separate from black, and text stays readable on them.
        assert!(t.panel_bg2.r() >= 8 && t.panel_bg2.r() <= 24, "secondary surface is a near-black grey");
        assert!(contrast(t.text, t.panel_bg) >= 7.0, "text on the panel");
        assert!(contrast(t.text, t.button) >= 7.0, "text on a button");
        assert!(contrast(t.text_dim, t.panel_bg2) >= 4.5, "dim text on a secondary surface");
        assert!(contrast(t.button_border, t.button) >= 2.0, "button edge stays visible");
        assert!(contrast(t.border, t.panel_bg) >= 1.2, "separators stay visible");

        // Leave the process-wide palette as the other tests expect it.
        apply_palette(&ctx, Palette::Dark);
        assert_eq!(active_palette(), Palette::Dark);
    }

    #[test]
    fn theme_command_sets_and_reports_the_mode() {
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.run("ui.theme", json!({})), Ok(json!({"mode": "dark"})));
        assert_eq!(app.run("ui.theme", json!({"mode": "light"})), Ok(json!({"mode": "light"})));
        assert_eq!(app.ui.theme, ThemeMode::Light);
        assert!(app.run("ui.theme", json!({"mode": "sepia"})).is_err());
        assert_eq!(app.ui.theme, ThemeMode::Light);
        assert_eq!(app.run("ui.theme", json!({"mode": "system"})), Ok(json!({"mode": "system"})));
        assert!(crate::menus::UI_COMMANDS.iter().any(|c| c.0 == "ui.theme"));
    }

    #[test]
    fn hex_parsing_accepts_both_lengths_and_rejects_hostile_input() {
        assert_eq!(parse_hex("#DDB5C7"), Some(SOFT_PINK));
        assert_eq!(parse_hex("ddb5c7"), Some(SOFT_PINK));
        assert_eq!(parse_hex("  #dDb5C7 "), Some(SOFT_PINK));
        assert_eq!(parse_hex("#FFF"), Some([255, 255, 255]));
        assert_eq!(parse_hex("abc"), Some([0xAA, 0xBB, 0xCC]));
        assert_eq!(format_hex(SOFT_PINK), "#DDB5C7");
        assert_eq!(format_hex([1, 2, 3]), "#010203");
        for bad in [
            "",
            "#",
            "#12",
            "#1234",
            "#12345",
            "#1234567",
            "#GGGGGG",
            "#ggg",
            "+ff0000",
            "-f0000",
            "#ff 000",
            "0x12345",
            "##FFFFFF",
            "#FFFF\u{0}0",
            "ß",
            "#ä12345",
            "#😀",
            "F\n",
            "#FFFF00 ;",
        ] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
        assert_eq!(parse_hex(&"F".repeat(100_000)), None, "a long paste is rejected without work");
    }

    #[test]
    fn custom_palettes_keep_text_readable() {
        for hex in ["#000000", "#FFFFFF", "#DDB5C7", "#BE86A3", "#808080", "#FF0000", "#0000FF", "#FFFF00", "#777777"] {
            let bg = rgb(parse_hex(hex).expect("table colour"));
            let t = custom_tokens(bg);
            assert_eq!(t.panel_bg, bg, "{hex}: panel is the colour");
            assert_eq!(t.playlist_bg, bg, "{hex}: playlist is the colour");
            assert!(contrast(t.text, t.panel_bg) >= 4.5, "{hex}: primary text on the panel");
            for fill in [t.button, t.button_hi, t.panel_bg2, t.slot_face[0], t.name_field_sel] {
                assert!(contrast(t.text, fill) >= 4.5, "{hex}: primary text on a fill {fill:?}");
            }
            for weak in [t.text_dim, t.visible_dot] {
                assert!(contrast(weak, t.panel_bg2) >= 3.0, "{hex}: weak text on a secondary surface");
                assert!(contrast(weak, t.panel_bg) >= 3.0, "{hex}: weak text on the panel");
            }
            assert!(contrast(t.counter_label, t.counter_bg) >= 3.0, "{hex}: counter label");
            assert!(contrast(t.section_label, t.strip_section) >= 3.0, "{hex}: section label");
            assert!(contrast(t.ruler_text, t.ruler_bg) >= 3.0, "{hex}: ruler text");
            // Surfaces step toward the text colour, so the hierarchy stays visible.
            assert_ne!(t.panel_bg2, t.panel_bg, "{hex}: secondary surface differs from the panel");
            assert_ne!(t.button, t.panel_bg, "{hex}: buttons differ from the panel");
        }
    }

    #[test]
    fn custom_colour_picks_its_base_set_by_luminance() {
        assert!(custom_is_light(rgb(SOFT_PINK)), "Soft Pink is light");
        assert!(custom_is_light(rgb(parse_hex("#FFFF00").expect("yellow"))));
        // Dark Pink (luminance about 0.31) still reads better with black text, so it is light too.
        assert!(custom_is_light(rgb(DARK_PINK)), "Dark Pink takes the light base");
        assert!(!custom_is_light(rgb([0x30, 0x30, 0x30])));
        assert!(!custom_is_light(rgb([0, 0, 0])));
        // The base's accent and track colours carry over.
        let light = custom_tokens(rgb(SOFT_PINK));
        assert_eq!(light.accent, Tokens::LIGHT.accent);
        assert_eq!(light.meter_green, Tokens::LIGHT.meter_green);
        let dark = custom_tokens(rgb([0x30, 0x30, 0x30]));
        assert_eq!(dark.accent, Tokens::DARK.accent);
        assert_eq!(dark.selection, Tokens::DARK.selection);
    }

    #[test]
    fn custom_mode_and_colour_round_trip_through_prefs() {
        assert_eq!(ThemeMode::parse("custom"), Some(ThemeMode::Custom));
        assert_eq!(ThemeMode::Custom.label(), "Custom");
        assert_eq!(serde_json::to_value(ThemeMode::Custom).expect("serialises"), json!("custom"));
        let ui = UiState { theme: ThemeMode::Custom, custom_bg: [1, 2, 3], ..UiState::default() };
        let value = serde_json::to_value(&ui).expect("serialises");
        assert_eq!(value["custom_bg"], json!([1, 2, 3]));
        let back: UiState = serde_json::from_value(value).expect("loads");
        assert_eq!(back.theme, ThemeMode::Custom);
        assert_eq!(back.custom_bg, [1, 2, 3]);
        let old: UiState = serde_json::from_value(json!({"theme": "dark"})).expect("an older ui.json loads");
        assert_eq!(old.custom_bg, SOFT_PINK, "a missing colour gets Soft Pink");
        assert_eq!(CUSTOM_PRESETS[2].1, [Tokens::DARK.panel_bg.r(), Tokens::DARK.panel_bg.g(), Tokens::DARK.panel_bg.b()]);
    }

    #[test]
    fn ui_theme_custom_command_validates_the_colour() {
        let _palette = lock_test_palette();
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.run("ui.theme", json!({"mode": "custom", "color": "#BE86A3"})), Ok(json!({"mode": "custom", "color": "#BE86A3"})));
        assert_eq!(app.ui.theme, ThemeMode::Custom);
        assert_eq!(app.ui.custom_bg, DARK_PINK);
        assert_eq!(app.run("ui.theme", json!({"mode": "custom"})), Ok(json!({"mode": "custom", "color": "#BE86A3"})), "color is optional");
        assert!(app.run("ui.theme", json!({"mode": "custom", "color": "#GGGGGG"})).is_err());
        assert!(app.run("ui.theme", json!({"mode": "custom", "color": 5})).is_err());
        assert!(app.run("ui.theme", json!({"mode": "sepia", "color": "#000000"})).is_err());
        assert_eq!(app.ui.custom_bg, DARK_PINK, "a rejected call changes nothing");
        assert_eq!(app.ui.theme, ThemeMode::Custom);
        assert_eq!(app.run("ui.theme", json!({"mode": "dark"})), Ok(json!({"mode": "dark"})));
        assert_eq!(app.ui.custom_bg, DARK_PINK, "leaving Custom keeps the colour for later");
        assert_eq!(app.run("ui.theme", json!({"color": "#000000"})), Ok(json!({"mode": "dark"})), "a colour alone keeps the mode");
        assert_eq!(app.ui.custom_bg, [0, 0, 0]);
    }

    #[test]
    fn custom_palette_applies_its_tokens_and_base_polarity() {
        let _palette = lock_test_palette();
        let ctx = egui::Context::default();
        apply_palette(&ctx, Palette::Custom(SOFT_PINK));
        assert_eq!(active_palette(), Palette::Custom(SOFT_PINK));
        assert!(is_light());
        assert_eq!(Tokens::current().panel_bg, rgb(SOFT_PINK));
        assert!(!ctx.global_style().visuals.dark_mode, "a light colour gets light visuals");
        assert_eq!(ctx.global_style().visuals.panel_fill, rgb(SOFT_PINK));
        // A new colour replaces the cached tokens.
        apply_palette(&ctx, Palette::Custom(DARK_PINK));
        assert_eq!(Tokens::current().panel_bg, rgb(DARK_PINK));
        apply_palette(&ctx, Palette::Custom([0, 0, 0]));
        assert!(!is_light());
        assert!(ctx.global_style().visuals.dark_mode, "a dark colour gets dark visuals");
        apply_palette(&ctx, Palette::Dark);
        assert_eq!(active_palette(), Palette::Dark);
    }

    #[test]
    fn application_frames_follow_system_appearance_and_preserve_manual_palettes() {
        // Mixer rendering tests also change the process-wide custom-painted palette.
        let _palette = lock_test_palette();
        fn frame(app: &mut SoundApp, ctx: &egui::Context, system_theme: Option<egui::Theme>, light: bool) {
            let mode = app.ui.theme;
            let colors = |app: &SoundApp| {
                app.engine
                    .session()
                    .tracks
                    .iter()
                    .map(|track| {
                        (track.color, track.playlists.iter().flat_map(|playlist| &playlist.clips).map(|clip| clip.color).collect::<Vec<_>>())
                    })
                    .collect::<Vec<_>>()
            };
            let before = colors(app);
            let mut output = ctx.run_ui(egui::RawInput { system_theme, ..Default::default() }, |ui| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                let tokens = if light { Tokens::LIGHT } else { Tokens::DARK };
                assert_eq!(is_light(), light, "custom palette for {mode:?} with {system_theme:?}");
                assert_eq!(Tokens::current().panel_bg, tokens.panel_bg);
                assert_eq!(ctx.options(|options| options.theme_preference), egui::ThemePreference::System);
                for theme in [egui::Theme::Light, egui::Theme::Dark] {
                    let style = ctx.style_of(theme);
                    assert_eq!(style.visuals.dark_mode, !light);
                    assert_eq!(style.visuals.panel_fill, tokens.panel_bg);
                    assert_eq!(style.visuals.widgets.inactive.bg_fill, tokens.button);
                    assert_eq!(style.visuals.widgets.inactive.fg_stroke.color, tokens.text);
                    assert_eq!(style.spacing.button_padding, egui::vec2(6.0, 2.0));
                }
                app.ui(ui);
                assert_eq!(ui.visuals().panel_fill, tokens.panel_bg);
                assert_eq!(ui.visuals().widgets.inactive.bg_fill, tokens.button);
            });
            output.textures_delta.clear();
            assert_eq!(app.ui.theme, mode);
            assert_eq!(serde_json::to_value(&app.ui).unwrap()["theme"], mode.id());
            assert_eq!(colors(app), before, "appearance must preserve track and clip colors");
        }

        let appearances = [Some(egui::Theme::Light), Some(egui::Theme::Dark), Some(egui::Theme::Light), None];
        for initial in [Some(egui::Theme::Light), Some(egui::Theme::Dark), None] {
            let ctx = egui::Context::default();
            let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
            app.ui = serde_json::from_value(json!({"theme": "system"})).unwrap();
            frame(&mut app, &ctx, initial, initial == Some(egui::Theme::Light));
        }

        let ctx = egui::Context::default();
        let mut engine = soundcraft_engine::demo::demo_engine();
        let track = engine.session_mut().tracks.first_mut().unwrap();
        track.color = [19, 83, 147];
        track.playlists.iter_mut().flat_map(|playlist| &mut playlist.clips).next().unwrap().color = Some([211, 57, 99]);
        let mut app = SoundApp::new(engine, None, Services::default());
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            app.run("ui.theme", json!({"mode": mode.id()})).unwrap();
            for appearance in appearances {
                let light = mode == ThemeMode::Light || (mode == ThemeMode::System && appearance == Some(egui::Theme::Light));
                frame(&mut app, &ctx, appearance, light);
            }
            if mode != ThemeMode::System {
                let latest = if mode == ThemeMode::Light { egui::Theme::Dark } else { egui::Theme::Light };
                frame(&mut app, &ctx, Some(latest), mode == ThemeMode::Light);
                app.run("ui.theme", json!({"mode": "system"})).unwrap();
                // Resolve the current context immediately, without another OS appearance event.
                app.logic(&ctx);
                assert_eq!(is_light(), latest == egui::Theme::Light);
                assert_eq!(ctx.global_style().visuals.dark_mode, latest != egui::Theme::Light);
                frame(&mut app, &ctx, Some(latest), latest == egui::Theme::Light);
            }
        }
    }
}
