//! Persistence for the sidebar layout (~/.tabt/layout.conf).
//!
//! Stores "structure + light state": colors/font + sidebar width/position + group name/collapsed state + each tab's
//! title and last working directory (cwd). On next launch a new shell is spawned directly in the restored cwd.
//!
//! The format is hand-written INI (no serde, to keep binary size down): `[section]` headers +
//! `key = value` lines. Ungrouped tabs go in an optional single `[tabs]` section (rendered at the top of the list);
//! groups are represented by **repeated `[group]` sections** to preserve order and allow duplicate names. Within a section each `tab = title` is
//! one tab, optionally followed by a `cwd = path` line (belonging to the nearest tab). Lines starting with `#`/`;` and
//! blank lines are ignored.
//!
//! ```ini
//! [settings]
//! style = Default
//! font_family = system
//! font_size = 13
//! sidebar_width = 200
//! sidebar_right = false
//!
//! [tabs]
//! tab = Loose tab
//! cwd = /Users/me
//!
//! [group]
//! name = Default
//! collapsed = false
//! tab = Terminal 1
//! cwd = /Users/me/proj
//! tab = server
//!
//! [group]
//! name = Work
//! collapsed = true
//! tab = api
//! ```
//!
//! Note: values are not escaped -- a newline in a group name/tab title would corrupt the format (renaming should forbid it).

use std::fs;
use std::path::PathBuf;

/// Highest valid status-dot color index (must match sidebar::DOT_COLORS: indices 0..=8).
const MAX_DOT: u8 = 8;

/// Persistent state of one tab (title + last working directory + status-dot color index + lock).
pub struct TabState {
    pub title: String,
    pub cwd: String,
    pub dot: u8,      // 0 = default/auto; 1..=8 = classic colors (see sidebar::DOT_COLORS)
    pub locked: bool, // locked tabs are protected from being closed (⌘W / the tab menu's Close)
}

/// The layout read back: color theme name + font + sidebar width/position + ungrouped tabs + each group.
pub struct Layout {
    pub style: String,
    pub font_family: String,
    pub font_size: f64,
    pub sidebar_w: f64,
    pub sidebar_right: bool,      // true=sidebar on the right
    pub show_border: bool,        // whether to draw the sidebar/header separator lines
    pub window_w: f64,            // saved window content width (0 = use the built-in default)
    pub window_h: f64,            // saved window content height (0 = use the built-in default)
    pub ungrouped: Vec<TabState>, // tabs not belonging to any group (rendered at the top of the list)
    pub groups: Vec<(String, bool, Vec<TabState>)>,
}

fn dir() -> PathBuf {
    let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    p.push(".tabt");
    p
}

fn file() -> PathBuf {
    let mut p = dir();
    p.push("layout.conf");
    p
}

/// The INI section currently being parsed.
enum Section {
    None,
    Settings,
    Tabs, // [tabs]: ungrouped tabs
    Group,
}

/// Load the layout; if the file is missing or empty, provide a default group + a single tab.
pub fn load() -> Layout {
    let text = fs::read_to_string(file()).unwrap_or_default();
    let mut style = "Default".to_string();
    let mut font_family = crate::settings::DEFAULT_FAMILY.to_string();
    let mut font_size = crate::settings::DEFAULT_SIZE;
    let mut sidebar_w = 232.0;
    let mut sidebar_right = false;
    let mut show_border = false;
    let mut window_w = 0.0;
    let mut window_h = 0.0;
    let mut ungrouped: Vec<TabState> = Vec::new();
    let mut groups: Vec<(String, bool, Vec<TabState>)> = Vec::new();
    let mut section = Section::None;

    for raw in text.lines() {
        let line = raw.trim();
        // Skip blank lines and comments (# / ;).
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        // Header [name]: switch the current section; each [group] opens a new group (order preserved, duplicate names allowed).
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            match name.trim() {
                "settings" => section = Section::Settings,
                "tabs" => section = Section::Tabs,
                "group" => {
                    section = Section::Group;
                    groups.push((String::new(), false, Vec::new()));
                }
                _ => section = Section::None, // unknown section: ignore its keys
            }
            continue;
        }
        // key = value (value may contain '='; split only on the first '=').
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match section {
            Section::Settings => match key {
                "style" => style = value.to_string(),
                "font_family" => {
                    if !value.is_empty() {
                        font_family = value.to_string();
                    }
                }
                "font_size" => {
                    if let Ok(n) = value.parse::<f64>() {
                        font_size = n;
                    }
                }
                "sidebar_width" => {
                    if let Ok(n) = value.parse::<f64>() {
                        sidebar_w = n;
                    }
                }
                "sidebar_right" => {
                    sidebar_right = value.eq_ignore_ascii_case("true") || value == "1";
                }
                "show_border" => {
                    show_border = value.eq_ignore_ascii_case("true") || value == "1";
                }
                "window_width" => {
                    if let Ok(n) = value.parse::<f64>() {
                        window_w = n;
                    }
                }
                "window_height" => {
                    if let Ok(n) = value.parse::<f64>() {
                        window_h = n;
                    }
                }
                _ => {}
            },
            Section::Tabs => match key {
                "tab" => ungrouped.push(TabState { title: value.to_string(), cwd: String::new(), dot: 0, locked: false }),
                "cwd" => {
                    if let Some(t) = ungrouped.last_mut() {
                        t.cwd = value.to_string();
                    }
                }
                "dot" => {
                    if let (Some(t), Ok(n)) = (ungrouped.last_mut(), value.parse::<u8>()) {
                        t.dot = if n <= MAX_DOT { n } else { 0 }; // ignore out-of-range indices
                    }
                }
                "lock" => {
                    if let Some(t) = ungrouped.last_mut() {
                        t.locked = value.eq_ignore_ascii_case("true") || value == "1";
                    }
                }
                _ => {}
            },
            Section::Group => {
                if let Some(g) = groups.last_mut() {
                    match key {
                        "name" => g.0 = value.to_string(),
                        "collapsed" => g.1 = value.eq_ignore_ascii_case("true") || value == "1",
                        "tab" => g.2.push(TabState { title: value.to_string(), cwd: String::new(), dot: 0, locked: false }),
                        // cwd / dot / lock belong to the nearest tab in this section.
                        "cwd" => {
                            if let Some(t) = g.2.last_mut() {
                                t.cwd = value.to_string();
                            }
                        }
                        "dot" => {
                            if let (Some(t), Ok(n)) = (g.2.last_mut(), value.parse::<u8>()) {
                                t.dot = if n <= MAX_DOT { n } else { 0 }; // ignore out-of-range indices
                            }
                        }
                        "lock" => {
                            if let Some(t) = g.2.last_mut() {
                                t.locked = value.eq_ignore_ascii_case("true") || value == "1";
                            }
                        }
                        _ => {}
                    }
                }
            }
            Section::None => {}
        }
    }

    // When entirely empty (no ungrouped tabs and no tabs inside groups), add one ungrouped tab so a terminal is available.
    let total_tabs = ungrouped.len() + groups.iter().map(|g| g.2.len()).sum::<usize>();
    if total_tabs == 0 {
        ungrouped.push(TabState { title: "Terminal 1".to_string(), cwd: String::new(), dot: 0, locked: false });
    }

    Layout { style, font_family, font_size, sidebar_w, sidebar_right, show_border, window_w, window_h, ungrouped, groups }
}

/// Write the layout back. Tabs in `ungrouped`/`groups` are (title, cwd, dot, locked).
pub fn save(
    style: &str,
    font_family: &str,
    font_size: f64,
    sidebar_w: f64,
    sidebar_right: bool,
    show_border: bool,
    window_w: f64,
    window_h: f64,
    ungrouped: &[(String, String, u8, bool)],
    groups: &[(String, bool, Vec<(String, String, u8, bool)>)],
) {
    let _ = fs::create_dir_all(dir());

    // For each tab, write one tab= line and optional cwd=/dot=/lock= lines.
    let write_tabs = |s: &mut String, tabs: &[(String, String, u8, bool)]| {
        for (title, cwd, dot, locked) in tabs {
            s.push_str(&format!("tab = {}\n", title));
            if !cwd.is_empty() {
                s.push_str(&format!("cwd = {}\n", cwd));
            }
            if *dot != 0 {
                s.push_str(&format!("dot = {}\n", dot));
            }
            if *locked {
                s.push_str("lock = true\n");
            }
        }
    };

    let mut s = String::new();
    s.push_str("[settings]\n");
    s.push_str(&format!("style = {}\n", style));
    s.push_str(&format!("font_family = {}\n", font_family));
    s.push_str(&format!("font_size = {}\n", font_size));
    s.push_str(&format!("sidebar_width = {}\n", sidebar_w));
    s.push_str(&format!("sidebar_right = {}\n", sidebar_right));
    s.push_str(&format!("show_border = {}\n", show_border));
    if window_w > 0.0 && window_h > 0.0 {
        s.push_str(&format!("window_width = {}\n", window_w));
        s.push_str(&format!("window_height = {}\n", window_h));
    }
    if !ungrouped.is_empty() {
        s.push_str("\n[tabs]\n");
        write_tabs(&mut s, ungrouped);
    }
    for (name, collapsed, tabs) in groups {
        s.push_str("\n[group]\n");
        s.push_str(&format!("name = {}\n", name));
        s.push_str(&format!("collapsed = {}\n", collapsed));
        write_tabs(&mut s, tabs);
    }
    let _ = fs::write(file(), s);
}
