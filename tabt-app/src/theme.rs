//! Color themes (classic styles).
//!
//! Centralizes theme data + the global state of the "current theme" (main-thread exclusive, read when drawing), shared by view /
//! app / sidebar without them depending on each other. CRT-style themes (classic green / amber) are treated as a "monochrome phosphor screen":
//! SGR colors are ignored, body text always uses the phosphor color (bold slightly brightened).

use std::cell::Cell;

/// Normalized RGB components ([0,1]).
pub type Rgb = (f64, f64, f64);

#[derive(Clone, Copy)]
pub struct Theme {
    pub fg: Rgb,                      // default foreground
    pub bg: Rgb,                      // default background + window/body base color
    pub palette: [(u8, u8, u8); 16],  // ANSI 16 colors (unused in mono themes)
    pub mono: bool,                   // true=monochrome phosphor screen, ignore SGR colors
}

impl Theme {
    /// Sidebar background: tracks the theme so it stays coherent when the theme changes.
    /// Nudged very slightly off the body background to hint a panel edge on both dark and light themes.
    pub fn is_dark(&self) -> bool {
        luminance(self.bg) <= 0.5
    }

    pub fn sidebar_bg(&self) -> Rgb {
        // Dark themes: a touch lighter; light themes: a touch darker.
        let d = if self.is_dark() { 0.02 } else { -0.02 };
        (
            (self.bg.0 + d).clamp(0.0, 1.0),
            (self.bg.1 + d).clamp(0.0, 1.0),
            (self.bg.2 + d).clamp(0.0, 1.0),
        )
    }

    /// Subtle separator/border line color derived from the theme (background nudged toward the
    /// foreground), so borders read correctly on dark and light themes alike.
    pub fn border(&self) -> Rgb {
        mix(self.bg, self.fg, 0.10)
    }
}

/// Linear blend a→b by t (0 = a, 1 = b). The single RGB-mixing helper used across the app.
pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Perceptual-ish luminance of an Rgb (0..1), used to tell light themes from dark ones.
fn luminance(c: Rgb) -> f64 {
    0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2
}

/// The display order in the sidebar footer = the index in by_index.
pub const NAMES: [&str; 9] = [
    "Default",
    "Classic Green",
    "Amber",
    "Solarized Dark",
    "Solarized Light",
    "One Dark",
    "Dracula",
    "Nord",
    "GitHub Light",
];

/// Standard xterm 16 colors (used by the default theme).
const BASE16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Solarized 16-color mapping (ANSI index -> specific Solarized color).
const SOLARIZED: [(u8, u8, u8); 16] = [
    (7, 54, 66),     // 0  base02
    (220, 50, 47),   // 1  red
    (133, 153, 0),   // 2  green
    (181, 137, 0),   // 3  yellow
    (38, 139, 210),  // 4  blue
    (211, 54, 130),  // 5  magenta
    (42, 161, 152),  // 6  cyan
    (238, 232, 213), // 7  base2
    (0, 43, 54),     // 8  base03
    (203, 75, 22),   // 9  orange
    (88, 110, 117),  // 10 base01
    (101, 123, 131), // 11 base00
    (131, 148, 150), // 12 base0
    (108, 113, 196), // 13 violet
    (147, 161, 161), // 14 base1
    (253, 246, 227), // 15 base3
];

/// One Dark (Atom): dark, soft high contrast.
const ONE_DARK: [(u8, u8, u8); 16] = [
    (40, 44, 52),
    (224, 108, 117),
    (152, 195, 121),
    (229, 192, 123),
    (97, 175, 239),
    (198, 120, 221),
    (86, 182, 194),
    (171, 178, 191),
    (92, 99, 112),
    (224, 108, 117),
    (152, 195, 121),
    (229, 192, 123),
    (97, 175, 239),
    (198, 120, 221),
    (86, 182, 194),
    (255, 255, 255),
];

/// Dracula: dark purple background, high saturation.
const DRACULA: [(u8, u8, u8); 16] = [
    (33, 34, 44),
    (255, 85, 85),
    (80, 250, 123),
    (241, 250, 140),
    (189, 147, 249),
    (255, 121, 198),
    (139, 233, 253),
    (248, 248, 242),
    (98, 114, 164),
    (255, 110, 103),
    (90, 247, 142),
    (244, 249, 157),
    (202, 169, 250),
    (255, 146, 208),
    (154, 237, 254),
    (255, 255, 255),
];

/// Nord: cool-toned Nordic blue-gray.
const NORD: [(u8, u8, u8); 16] = [
    (59, 66, 82),
    (191, 97, 106),
    (163, 190, 140),
    (235, 203, 139),
    (129, 161, 193),
    (180, 142, 173),
    (136, 192, 208),
    (229, 233, 240),
    (76, 86, 106),
    (191, 97, 106),
    (163, 190, 140),
    (235, 203, 139),
    (129, 161, 193),
    (180, 142, 173),
    (143, 188, 187),
    (236, 239, 244),
];

/// GitHub Light: white background, suited for daytime.
const GITHUB_LIGHT: [(u8, u8, u8); 16] = [
    (36, 41, 46),
    (215, 58, 73),
    (40, 167, 69),
    (219, 171, 9),
    (3, 102, 214),
    (90, 50, 163),
    (27, 124, 131),
    (106, 115, 125),
    (149, 157, 165),
    (203, 36, 49),
    (34, 134, 58),
    (176, 136, 0),
    (0, 92, 197),
    (90, 50, 163),
    (49, 146, 170),
    (209, 213, 218),
];

/// Get a theme by index (out of bounds falls back to default). The order must match [`NAMES`].
pub fn by_index(i: usize) -> Theme {
    match i {
        1 => Theme {
            // Classic green: green text on black CRT
            fg: (0.20, 1.0, 0.30),
            bg: (0.0, 0.03, 0.0),
            palette: BASE16,
            mono: true,
        },
        2 => Theme {
            // Amber: amber CRT
            fg: (1.0, 0.72, 0.18),
            bg: (0.05, 0.02, 0.0),
            palette: BASE16,
            mono: true,
        },
        3 => Theme {
            // Solarized Dark
            fg: (131.0 / 255.0, 148.0 / 255.0, 150.0 / 255.0), // base0
            bg: (0.0, 43.0 / 255.0, 54.0 / 255.0),             // base03
            palette: SOLARIZED,
            mono: false,
        },
        4 => Theme {
            // Solarized Light (light): base00 text / base3 background
            fg: (101.0 / 255.0, 123.0 / 255.0, 131.0 / 255.0), // base00
            bg: (253.0 / 255.0, 246.0 / 255.0, 227.0 / 255.0), // base3
            palette: SOLARIZED,
            mono: false,
        },
        5 => Theme {
            // One Dark
            fg: (171.0 / 255.0, 178.0 / 255.0, 191.0 / 255.0),
            bg: (40.0 / 255.0, 44.0 / 255.0, 52.0 / 255.0),
            palette: ONE_DARK,
            mono: false,
        },
        6 => Theme {
            // Dracula
            fg: (248.0 / 255.0, 248.0 / 255.0, 242.0 / 255.0),
            bg: (40.0 / 255.0, 42.0 / 255.0, 54.0 / 255.0),
            palette: DRACULA,
            mono: false,
        },
        7 => Theme {
            // Nord
            fg: (216.0 / 255.0, 222.0 / 255.0, 233.0 / 255.0),
            bg: (46.0 / 255.0, 52.0 / 255.0, 64.0 / 255.0),
            palette: NORD,
            mono: false,
        },
        8 => Theme {
            // GitHub Light (light): near-black text / pure white background
            fg: (36.0 / 255.0, 41.0 / 255.0, 46.0 / 255.0),
            bg: (1.0, 1.0, 1.0),
            palette: GITHUB_LIGHT,
            mono: false,
        },
        _ => Theme {
            // Default: warm dark-gray background #1d1c1b (not pure black) + light text #e8e8ec
            fg: (232.0 / 255.0, 232.0 / 255.0, 236.0 / 255.0),
            bg: (29.0 / 255.0, 28.0 / 255.0, 27.0 / 255.0),
            palette: BASE16,
            mono: false,
        },
    }
}

/// Reverse-look up an index by name (returns 0 if not found).
pub fn index_of(name: &str) -> usize {
    NAMES.iter().position(|&n| n == name).unwrap_or(0)
}

thread_local! {
    static CURRENT: Cell<Theme> = Cell::new(by_index(0));
}

/// Set the current theme (called when switching styles; takes effect once each view redraws).
pub fn set(t: Theme) {
    CURRENT.with(|c| c.set(t));
}

/// Read the current theme (called when drawing).
pub fn current() -> Theme {
    CURRENT.with(|c| c.get())
}
