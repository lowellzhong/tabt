//! AppController: runtime management of multi-tab / grouping.
//!
//! Owns all sessions (each tab = one PTY + one [`TermView`]); responsible for creating / switching /
//! moving / closing tabs, mounting the current active tab's view into the right-hand host container,
//! and persisting the group layout to ~/.tabt. It is a plain Rust struct (held in an `Rc`, exclusive to
//! the main thread); the sidebar and TermView call back into it via a raw pointer -- as long as it is
//! alive (main holds the `Rc` until app.run ends), the pointer stays valid.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::unix::io::RawFd;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2_app_kit::{NSAlert, NSApplication, NSAutoresizingMaskOptions, NSView, NSWindow, NSWindowButton};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::config;
use crate::divider::{Divider, DIVIDER_W};
use crate::header::{HeaderView, HEADER_H};
use crate::placeholder::PlaceholderView;
use crate::pty;
use crate::settings;
use crate::settings_dialog::SettingsDialog;
use crate::sidebar::{SidebarView, SIDEBAR_W};
use crate::theme;
use crate::toggle::{ToggleButton, TOGGLE_W};
use crate::view::{self, TermView};

struct Tab {
    id: u64,
    title: String,
    dot: u8,      // status-dot color index (0 = default/auto; 1..=8 = classic colors, see sidebar::DOT_COLORS)
    locked: bool, // locked tabs are protected from being closed by the user (⌘W / the tab menu)
    view: Retained<TermView>,
    master_fd: RawFd,
    shell_pid: libc::pid_t, // the shell's own pid/pgid; used to detect a foreground job (pty::has_foreground_job)
    reader: view::ReaderToken,
    spawn_cwd: String, // working directory at spawn time: cwd fallback when OSC 7 has not reported
}

struct Group {
    name: String,
    collapsed: bool, // when collapsed, the sidebar hides its tabs
    tabs: Vec<u64>,  // ordered tab ids
}

struct Model {
    ungrouped: Vec<u64>, // ids of tabs not belonging to any group (ordered, rendered at the top of the list)
    groups: Vec<Group>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_id: u64,
}

/// Group snapshot used by the sidebar for drawing.
pub struct GroupSnap {
    pub name: String,
    pub collapsed: bool,
    pub tabs: Vec<(u64, String, u8, bool)>, // (id, title, dot-color index, locked)
}

/// Read-only snapshot used by the sidebar for drawing (does not expose internal details like Retained).
pub struct Snapshot {
    pub ungrouped: Vec<(u64, String, u8, bool)>, // ungrouped tabs (id, title, dot-color index, locked), rendered at the top
    pub groups: Vec<GroupSnap>,
    pub active: Option<u64>,
    pub style: usize,
}

pub struct AppController {
    model: RefCell<Model>,
    window: Retained<NSWindow>,
    sidebar: Retained<SidebarView>,
    host: Retained<NSView>,
    toggle_btn: Retained<ToggleButton>,
    divider: Retained<Divider>,
    header: Retained<HeaderView>, // terminal-pane header bar (top of host)
    placeholder: Retained<PlaceholderView>, // empty-state view shown when there are no sessions
    style: Cell<usize>,        // index of the current color theme (theme::NAMES)
    collapsed: Cell<bool>,     // whether the sidebar is collapsed/hidden
    sidebar_w: Cell<f64>,      // current sidebar width (draggable)
    sidebar_right: Cell<bool>, // whether the sidebar is docked on the right
    settings_dialog: RefCell<Option<Retained<SettingsDialog>>>, // lazily built settings panel
    mtm: MainThreadMarker,
}

impl AppController {
    pub fn new(
        mtm: MainThreadMarker,
        window: Retained<NSWindow>,
        sidebar: Retained<SidebarView>,
        host: Retained<NSView>,
        toggle_btn: Retained<ToggleButton>,
        divider: Retained<Divider>,
    ) -> Rc<Self> {
        // Terminal-pane header bar: pinned to the top of host, full width.
        let hb = host.bounds();
        let header = HeaderView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, hb.size.height - HEADER_H), NSSize::new(hb.size.width, HEADER_H)),
        );
        unsafe {
            header.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable | NSAutoresizingMaskOptions::NSViewMinYMargin,
            );
            host.addSubview(&header);
        }
        // Empty-state placeholder occupying the terminal area (below the header); mounted only when there are no sessions.
        let placeholder = PlaceholderView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(hb.size.width, (hb.size.height - HEADER_H).max(0.0))),
        );
        unsafe {
            placeholder.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
        }
        let c = Rc::new(AppController {
            model: RefCell::new(Model { ungrouped: Vec::new(), groups: Vec::new(), tabs: Vec::new(), active: None, next_id: 1 }),
            window,
            sidebar,
            host,
            toggle_btn,
            divider,
            header,
            placeholder,
            style: Cell::new(0),
            collapsed: Cell::new(false),
            sidebar_w: Cell::new(SIDEBAR_W),
            sidebar_right: Cell::new(false),
            settings_dialog: RefCell::new(None),
            mtm,
        });
        // The sidebar / toggle button / divider get the controller's raw pointer (the controller lives in an Rc, so its address is stable).
        c.sidebar.set_controller(Rc::as_ptr(&c));
        c.toggle_btn.set_controller(Rc::as_ptr(&c));
        c.divider.set_controller(Rc::as_ptr(&c));
        c
    }

    /// Drag to adjust the sidebar width (ignored when collapsed).
    pub fn set_sidebar_width(&self, w: f64) {
        if self.collapsed.get() {
            return;
        }
        self.sidebar_w.set(w.clamp(190.0, 480.0));
        self.relayout();
    }

    /// Divider drag: window coordinate x -> sidebar width (for a right-side bar, width = content width - x).
    pub fn drag_sidebar_width(&self, window_x: f64) {
        let fw = self
            .window
            .contentView()
            .map(|c| c.bounds().size.width)
            .unwrap_or(0.0);
        let w = if self.sidebar_right.get() { fw - window_x } else { window_x };
        self.set_sidebar_width(w);
    }

    /// Persist the current layout (called when a divider drag ends).
    pub fn save_layout(&self) {
        self.save();
    }

    /// Set the sidebar's left/right position and persist.
    pub fn set_sidebar_side(&self, right: bool) {
        if self.sidebar_right.get() == right {
            return;
        }
        self.sidebar_right.set(right);
        self.relayout();
        self.save();
    }

    pub fn sidebar_on_right(&self) -> bool {
        self.sidebar_right.get()
    }

    /// Move the traffic-light buttons down so they vertically center in the taller title-bar
    /// zone (HEADER_H). macOS resets them on resize, so this is re-applied from windowDidResize.
    pub fn reposition_traffic_lights(&self) {
        for b in [
            NSWindowButton::NSWindowCloseButton,
            NSWindowButton::NSWindowMiniaturizeButton,
            NSWindowButton::NSWindowZoomButton,
        ] {
            if let Some(btn) = self.window.standardWindowButton(b) {
                // The buttons live in the (non-flipped) titlebar container that spans the full
                // window height, so its top edge is at `super_h`. Center each button in the
                // HEADER_H band pinned to that top edge.
                let super_h = unsafe { btn.superview() }.map(|s| s.frame().size.height).unwrap_or(0.0);
                if super_h <= 0.0 {
                    continue;
                }
                let bh = btn.frame().size.height;
                let mut o = btn.frame().origin;
                o.y = super_h - HEADER_H / 2.0 - bh / 2.0; // down = smaller y
                unsafe { btn.setFrameOrigin(o) };
            }
        }
    }

    /// Re-lay out the sidebar, divider, and terminal host per the current width / side / collapsed state,
    /// and set the autoresizing mask (on window resize: the sidebar keeps a fixed width against the edge, host fills the rest).
    fn relayout(&self) {
        use NSAutoresizingMaskOptions as M;
        let full = self
            .window
            .contentView()
            .map(|c| c.bounds())
            .unwrap_or_else(|| self.host.frame());
        let (fw, fh) = (full.size.width, full.size.height);
        let w = self.sidebar_w.get();
        let right = self.sidebar_right.get();
        let mk = |x: f64, width: f64| NSRect::new(NSPoint::new(x, 0.0), NSSize::new(width, fh));
        unsafe {
            if self.collapsed.get() {
                self.sidebar.setHidden(true);
                self.divider.setHidden(true);
                self.host.setFrame(mk(0.0, fw));
                self.host.setAutoresizingMask(M::NSViewWidthSizable | M::NSViewHeightSizable);
            } else {
                self.sidebar.setHidden(false);
                self.divider.setHidden(false);
                self.host.setAutoresizingMask(M::NSViewWidthSizable | M::NSViewHeightSizable);
                if right {
                    let sx = fw - w;
                    self.sidebar.setFrame(mk(sx, w));
                    self.sidebar.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMinXMargin);
                    self.host.setFrame(mk(0.0, sx));
                    self.divider.setFrame(NSRect::new(
                        NSPoint::new(sx - DIVIDER_W / 2.0, 0.0),
                        NSSize::new(DIVIDER_W, fh),
                    ));
                    self.divider.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMinXMargin);
                } else {
                    self.sidebar.setFrame(mk(0.0, w));
                    self.sidebar.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMaxXMargin);
                    self.host.setFrame(mk(w, fw - w));
                    self.divider.setFrame(NSRect::new(
                        NSPoint::new(w - DIVIDER_W / 2.0, 0.0),
                        NSSize::new(DIVIDER_W, fh),
                    ));
                    self.divider.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMaxXMargin);
                }
            }
            // Toggle button lives in the top title-bar zone (container is non-flipped, so top = high y).
            // Expanded: at the sidebar edge nearest the terminal. Collapsed: right after the traffic
            // lights, with even gaps, and the header title follows.
            let (tw, th) = (TOGGLE_W + 12.0, TOGGLE_W);
            let icon = 17.0; // toggle glyph size (see toggle.rs); centered within the button
            let icon_pad = (tw - icon) / 2.0;
            let (tx, title_inset) = if self.collapsed.get() {
                let gap = 14.0;
                let icon_left = 72.0 + gap; // 72 ≈ right edge of the traffic lights
                (icon_left - icon_pad, icon_left + icon + gap)
            } else if right {
                // Sidebar docked right: the terminal pane is on the left, so the traffic lights
                // sit at the window's top-left over the header — the title must clear them.
                (fw - w + 8.0, 72.0 + 14.0)
            } else {
                (w - tw - 8.0, 16.0)
            };
            self.toggle_btn.setFrame(NSRect::new(
                NSPoint::new(tx, fh - th - 9.0),
                NSSize::new(tw, th),
            ));
            self.header.set_left_inset(title_inset);
            self.sidebar.setNeedsDisplay(true);
            self.toggle_btn.setNeedsDisplay(true);
        }
    }

    /// The title bar always shows the current active tab's name; falls back to "TabT" when there is no active tab.
    fn update_title(&self) {
        let m = self.model.borrow();
        let title = m
            .active
            .and_then(|a| m.tabs.iter().find(|t| t.id == a))
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "TabT".to_string());
        self.window.setTitle(&NSString::from_str(&title));
        self.header.set_title(&title);
    }

    /// Load the layout from ~/.tabt and spawn a new shell for each tab.
    pub fn bootstrap(&self) {
        let layout = config::load();
        // Apply the color theme + font (both must be set before spawning tabs and computing cols/rows).
        let idx = theme::index_of(&layout.style);
        self.style.set(idx);
        theme::set(theme::by_index(idx));
        settings::set(&layout.font_family, layout.font_size);
        // The sidebar width/position must be set before spawning tabs and computing host dimensions.
        self.sidebar_w.set(layout.sidebar_w.clamp(190.0, 480.0));
        self.sidebar_right.set(layout.sidebar_right);
        settings::set_show_border(layout.show_border);
        // Restore the saved window size (clamped to a sane range) before laying out / spawning
        // tabs. The upper bound guards against a corrupted config producing an unusable
        // off-screen window; it's a generous cap, not a real display-size limit.
        if layout.window_w > 0.0 && layout.window_h > 0.0 {
            let sz = NSSize::new(layout.window_w.clamp(480.0, 6000.0), layout.window_h.clamp(320.0, 4000.0));
            self.window.setContentSize(sz);
            self.window.center();
        }
        self.relayout();
        // Spawn ungrouped tabs first (rendered at the top), then each group. A tab that fails to
        // spawn (e.g. the system is out of file descriptors) is silently skipped — restore
        // whatever we can rather than aborting the whole session restore.
        for t in layout.ungrouped {
            let _ = self.spawn_tab(None, t.title, &t.cwd, t.dot, t.locked);
        }
        for (name, collapsed, tabs) in layout.groups {
            let gi = {
                let mut m = self.model.borrow_mut();
                m.groups.push(Group { name, collapsed, tabs: Vec::new() });
                m.groups.len() - 1
            };
            for t in tabs {
                let _ = self.spawn_tab(Some(gi), t.title, &t.cwd, t.dot, t.locked);
            }
        }
        let first = self.model.borrow().tabs.first().map(|t| t.id);
        match first {
            Some(id) => self.select(id),
            // Every saved tab failed to spawn (e.g. the system is out of file descriptors at
            // launch) — show the empty-state placeholder instead of a blank host view.
            None => self.show_placeholder(),
        }
        self.save(); // persist once, ensuring ~/.tabt exists and reflects the current layout
        self.refresh_sidebar();
    }

    /// Current window content size (width, height), persisted so the next launch reopens at the same size.
    fn window_size(&self) -> (f64, f64) {
        let s = self
            .window
            .contentView()
            .map(|c| c.frame().size)
            .unwrap_or_else(|| self.window.frame().size);
        (s.width, s.height)
    }

    /// Compute cols/rows from the terminal-view area (host minus the header bar) and font metrics.
    /// Must match `TermView::on_resize`'s formula, otherwise a font change (which reflows via this
    /// instead of a view resize) would set a row count that doesn't fit the visible area.
    fn dims(&self) -> (usize, usize) {
        let b = self.host.bounds();
        let w = b.size.width - 2.0 * view::PAD;
        let h = b.size.height - HEADER_H - 2.0 * view::PAD;
        let cols = ((w / settings::cell_w()).floor() as i64).max(1) as usize;
        let rows = ((h / settings::line_h()).floor() as i64).max(1) as usize;
        (cols, rows)
    }

    /// Create a new tab (without switching to it): spawn a PTY + TermView + reader in `cwd`
    /// (for session restore, may be empty). When `group` is None it goes into the ungrouped list. Registered into the model.
    /// Returns `None` if the PTY/process itself couldn't be spawned (e.g. out of file
    /// descriptors) — the caller must skip creating this one tab without disturbing any others.
    fn spawn_tab(&self, group: Option<usize>, title: String, cwd: &str, dot: u8, locked: bool) -> Option<u64> {
        let id = {
            let mut m = self.model.borrow_mut();
            let id = m.next_id;
            m.next_id += 1;
            id
        };
        let (cols, rows) = self.dims();
        let (fd, shell_pid) = pty::spawn(cols as u16, rows as u16, cwd)?;
        let frame = self.host.bounds();
        let v = TermView::new(self.mtm, frame, fd, cols, rows);
        v.attach(self as *const AppController as *const c_void, id, close_cb, toggle_cb);
        let reader = view::attach_reader(&v);

        let mut m = self.model.borrow_mut();
        m.tabs.push(Tab { id, title, dot, locked, view: v, master_fd: fd, shell_pid, reader, spawn_cwd: cwd.to_string() });
        match group {
            Some(gi) if gi < m.groups.len() => m.groups[gi].tabs.push(id),
            _ => m.ungrouped.push(id),
        }
        Some(id)
    }

    pub fn select(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            if !m.tabs.iter().any(|t| t.id == id) {
                return;
            }
            m.active = Some(id);
        }
        self.layout_active();
        self.refresh_sidebar();
        self.update_title();
    }

    /// Mount the active tab's view into the host (remove the others), and make it the keyboard first responder.
    fn layout_active(&self) {
        let m = self.model.borrow();
        let active = match m.active {
            Some(a) => a,
            None => return,
        };
        // Terminal fills the host below the header bar (host is non-flipped: y=0 is the bottom).
        let hb = self.host.bounds();
        let bounds = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(hb.size.width, (hb.size.height - HEADER_H).max(0.0)),
        );
        unsafe { self.placeholder.removeFromSuperview() }; // hide the empty-state view when a session is active
        for t in &m.tabs {
            unsafe { t.view.removeFromSuperview() };
        }
        if let Some(tab) = m.tabs.iter().find(|t| t.id == active) {
            unsafe {
                tab.view.setFrame(bounds); // triggers setFrameSize -> grid reflow + TIOCSWINSZ
                tab.view.setAutoresizingMask(
                    NSAutoresizingMaskOptions::NSViewWidthSizable
                        | NSAutoresizingMaskOptions::NSViewHeightSizable,
                );
                self.host.addSubview(&tab.view);
                self.window.makeFirstResponder(Some(&tab.view));
                tab.view.setNeedsDisplay(true);
            }
        }
        drop(m);
        // makeFirstResponder makes AppKit relay out the titlebar and reset the traffic lights to
        // their default position — and that relayout runs *after* this call, so re-centering
        // synchronously here would be overwritten. Defer it to the next main-queue turn.
        self.defer_reposition_traffic_lights();
    }

    /// Mount the empty-state placeholder in the terminal area (no sessions left).
    fn show_placeholder(&self) {
        let hb = self.host.bounds();
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(hb.size.width, (hb.size.height - HEADER_H).max(0.0)),
        );
        unsafe {
            self.placeholder.setFrame(frame);
            self.host.addSubview(&self.placeholder);
            self.placeholder.setNeedsDisplay(true);
        }
    }

    /// Re-center the traffic lights on the next runloop turn (after AppKit's own titlebar layout).
    fn defer_reposition_traffic_lights(&self) {
        let q: view::dispatch::Queue = unsafe { &view::dispatch::_dispatch_main_q as *const _ as *mut _ };
        unsafe {
            view::dispatch::dispatch_async_f(q, self as *const AppController as *mut c_void, reposition_trampoline);
        }
    }

    /// New tab. When a tab is selected, the new tab inherits its working directory and is inserted
    /// immediately after it within the same group/list. With no selection, it lands in the ungrouped
    /// list (rendered at the top) in the home directory; it can be dragged into a group when needed.
    pub fn add_tab_default(&self) {
        // Snapshot the selected tab's cwd + group under a single borrow, before spawning.
        let anchor = {
            let m = self.model.borrow();
            m.active.and_then(|a| {
                let cwd = m.tabs.iter().find(|t| t.id == a).map(|t| {
                    // Prefer the live OSC 7 cwd; fall back to the directory it was spawned in.
                    let live = t.view.cwd();
                    if live.is_empty() { t.spawn_cwd.clone() } else { live }
                })?;
                // If the active tab's group is collapsed, the new tab would be created hidden inside
                // it. Place it at the top level (ungrouped) instead so it's visible.
                let group = m
                    .groups
                    .iter()
                    .position(|g| g.tabs.contains(&a))
                    .filter(|&gi| !m.groups[gi].collapsed);
                Some((a, group, cwd))
            })
        };
        let n = self.model.borrow().next_id;
        let (group, cwd) = match &anchor {
            Some((_, g, cwd)) => (*g, cwd.clone()),
            None => (None, String::new()),
        };
        match self.spawn_tab(group, format!("Terminal {}", n), &cwd, 0, false) {
            Some(id) => {
                if let Some((active_id, _, _)) = anchor {
                    self.place_tab_after(group, id, active_id);
                }
                self.select(id);
                self.save();
                self.refresh_sidebar();
            }
            None => self.alert_spawn_failed(),
        }
    }

    /// Create a new terminal inside group `gi` (from the group row's "New Terminal" menu item).
    /// Inherits the active tab's cwd when there is one, otherwise the default shell directory.
    /// `spawn_tab` appends the new tab to the group's end.
    pub fn add_tab_in_group(&self, gi: usize) {
        if gi >= self.model.borrow().groups.len() {
            return;
        }
        let cwd = {
            let m = self.model.borrow();
            m.active
                .and_then(|a| {
                    m.tabs.iter().find(|t| t.id == a).map(|t| {
                        let live = t.view.cwd();
                        if live.is_empty() { t.spawn_cwd.clone() } else { live }
                    })
                })
                .unwrap_or_default()
        };
        let n = self.model.borrow().next_id;
        match self.spawn_tab(Some(gi), format!("Terminal {}", n), &cwd, 0, false) {
            Some(id) => {
                self.select(id);
                self.save();
                self.refresh_sidebar();
            }
            None => self.alert_spawn_failed(),
        }
    }

    /// Move `id` to sit immediately after `anchor` within `group`'s ordered list (both must already be
    /// in that same list — `spawn_tab` appended `id` to its end). Keeps a new tab next to its origin.
    fn place_tab_after(&self, group: Option<usize>, id: u64, anchor: u64) {
        let mut m = self.model.borrow_mut();
        let list: &mut Vec<u64> = match group {
            Some(gi) if gi < m.groups.len() => &mut m.groups[gi].tabs,
            _ => &mut m.ungrouped,
        };
        if let Some(p) = list.iter().position(|&t| t == id) {
            list.remove(p);
        }
        let pos = list
            .iter()
            .position(|&t| t == anchor)
            .map(|p| p + 1)
            .unwrap_or(list.len());
        list.insert(pos, id);
    }

    /// Shown when a new tab's PTY/process couldn't be created (e.g. out of file descriptors).
    /// Existing tabs are unaffected; this only ever fails the one new tab.
    fn alert_spawn_failed(&self) {
        let alert = unsafe { NSAlert::new(self.mtm) };
        unsafe {
            alert.setMessageText(&NSString::from_str("Couldn’t open a new terminal"));
            alert.setInformativeText(&NSString::from_str(
                "The system refused to create a new session (it may be low on resources). Your other tabs are unaffected.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
    }

    pub fn add_group_default(&self) {
        let n = self.model.borrow().groups.len() + 1;
        self.model.borrow_mut().groups.push(Group { name: format!("Group {}", n), collapsed: false, tabs: Vec::new() });
        self.save();
        self.refresh_sidebar();
        // The new group is appended at the end of the list; scroll the sidebar to the bottom to make it visible.
        self.sidebar.scroll_to_bottom();
    }

    /// Drag to move a tab: move to `to_group`, inserting before tab `before` (None = append to the end).
    /// Supports both in-group reordering and cross-group moves.
    /// Drag to move a tab: target `to` is Some(group index) or None (ungrouped list), inserting before tab
    /// `before` (None = append). Supports same-group / cross-group / in-and-out of the ungrouped area.
    pub fn move_tab_to(&self, id: u64, to: Option<usize>, before: Option<u64>) {
        {
            let mut m = self.model.borrow_mut();
            if let Some(gi) = to {
                if gi >= m.groups.len() {
                    return;
                }
            }
            // First remove from the ungrouped list + all groups, then locate the insertion point by `before`.
            m.ungrouped.retain(|&t| t != id);
            for g in &mut m.groups {
                g.tabs.retain(|&t| t != id);
            }
            let list = match to {
                Some(gi) => &mut m.groups[gi].tabs,
                None => &mut m.ungrouped,
            };
            let pos = match before {
                Some(bid) => list.iter().position(|&t| t == bid).unwrap_or(list.len()),
                None => list.len(),
            };
            list.insert(pos, id);
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Drag to reorder groups: move group index `from` to insertion position `target` (0..=len, meaning
    /// "insert before the target-th element of the original array", so when target>from subtract 1 first to offset the removal).
    pub fn move_group(&self, from: usize, mut target: usize) {
        {
            let mut m = self.model.borrow_mut();
            if from >= m.groups.len() {
                return;
            }
            if target > from {
                target -= 1;
            }
            let g = m.groups.remove(from);
            let target = target.min(m.groups.len());
            m.groups.insert(target, g);
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Collapse/expand a group (the sidebar hides/shows its tabs).
    pub fn toggle_group_collapsed(&self, gi: usize) {
        {
            let mut m = self.model.borrow_mut();
            match m.groups.get_mut(gi) {
                Some(g) => g.collapsed = !g.collapsed,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Delete an entire group. If the group contains tabs, show a confirmation dialog first.
    pub fn delete_group(&self, gi: usize) {
        let (name, ids): (String, Vec<u64>) = match self.model.borrow().groups.get(gi) {
            Some(g) => (g.name.clone(), g.tabs.clone()),
            None => return,
        };
        if !ids.is_empty() && !self.confirm_delete_group(&name, ids.len()) {
            return;
        }
        // Remove the group entry first: otherwise, when closing the last tab triggers terminate->persist, the empty group
        // would be written back to the config and "revived" on the next launch.
        {
            let mut m = self.model.borrow_mut();
            if gi < m.groups.len() {
                m.groups.remove(gi);
            }
        }
        // Tear down each tab one by one (without persisting per tab). Deleting a group is an
        // explicit user action, so if it empties the app out entirely, stay open with the
        // empty-state placeholder rather than quitting (matches closing the last tab).
        let closed_active = ids.iter().any(|id| self.model.borrow().active == Some(*id));
        for id in &ids {
            self.teardown_tab(*id);
        }
        if self.model.borrow().tabs.is_empty() {
            self.went_empty();
            return;
        }
        if closed_active {
            if let Some(a) = self.model.borrow().tabs.first().map(|t| t.id) {
                self.select(a);
            }
        }
        self.save(); // persist only once for the whole deletion
        self.refresh_sidebar();
    }

    /// Confirmation dialog before deleting a non-empty group. Returns true = confirm deletion.
    fn confirm_delete_group(&self, name: &str, count: usize) -> bool {
        confirm(
            self.mtm,
            &format!("Delete group “{}”?", name),
            &format!("This will close {} terminal(s) in this group. This cannot be undone.", count),
            "Delete",
        )
    }

    /// Remove from the model and release a tab's PTY/view/reader (no persist, no reselect, no terminate).
    /// Returns whether the tab actually existed. If the removed tab is active, clear `active` and leave the caller to reselect.
    fn teardown_tab(&self, id: u64) -> bool {
        // First pull the TabData out of the model (we are now on a new runloop tick, out of on_readable).
        let removed = {
            let mut m = self.model.borrow_mut();
            let idx = match m.tabs.iter().position(|t| t.id == id) {
                Some(i) => i,
                None => return false,
            };
            m.ungrouped.retain(|&t| t != id);
            for g in &mut m.groups {
                g.tabs.retain(|&t| t != id);
            }
            if m.active == Some(id) {
                m.active = None;
            }
            m.tabs.remove(idx)
        };
        view::cancel_reader(&removed.reader);
        unsafe {
            removed.view.removeFromSuperview();
            libc::close(removed.master_fd);
        }
        drop(removed); // Retained<TermView> is released here, safely
        true
    }

    /// The tab adjacent to `id` in visual order (ungrouped first, followed by each group): prefer the
    /// preceding one, falling back to the next when closing the very first tab. Used to move focus to a
    /// neighbor after closing the active tab.
    fn adjacent_tab(&self, id: u64) -> Option<u64> {
        let m = self.model.borrow();
        let mut order: Vec<u64> = m.ungrouped.clone();
        for g in &m.groups {
            order.extend(g.tabs.iter().copied());
        }
        let pos = order.iter().position(|&t| t == id)?;
        pos.checked_sub(1)
            .and_then(|p| order.get(p).copied())
            .or_else(|| order.get(pos + 1).copied())
    }

    /// Close a tab because its shell process exited on its own. If it was the last tab, the app
    /// quits — nothing is left running, unlike an explicit user-initiated close (see `close_tab_user`).
    pub fn close_tab(&self, id: u64) {
        self.close_tab_impl(id, true);
    }

    /// Close a tab the user explicitly asked to close (⌘W, or "Close" in the tab's "⋯" menu).
    /// If it was the last tab, the app stays open with the empty-state placeholder instead of quitting.
    /// If the shell has a foreground job (not just an idle prompt), confirm first — closing tears
    /// down the PTY, which kills that job with no chance to save work.
    pub fn close_tab_user(&self, id: u64) {
        // A locked tab is protected from user-initiated close: silently ignore. The user must
        // unlock it first (via the tab menu). The shell exiting on its own still closes it.
        if self.is_tab_locked(id) {
            return;
        }
        if self.tab_has_foreground_job(id) && !confirm(self.mtm, "Close this tab?", RUNNING_JOB_WARNING, "Close") {
            return;
        }
        self.close_tab_impl(id, false);
    }

    /// ⌘Q (via the app delegate's `applicationShouldTerminate:`): if any open tab has a
    /// foreground job running, confirm once before quitting rather than silently killing every
    /// session. Returns whether termination should proceed.
    pub fn confirm_quit(&self) -> bool {
        if !self.model.borrow().tabs.iter().any(|t| pty::has_foreground_job(t.master_fd, t.shell_pid)) {
            return true;
        }
        confirm(
            self.mtm,
            "Quit TabT?",
            "One or more terminals still have a process running. Quitting will terminate all of them.",
            "Quit",
        )
    }

    /// Whether the given tab's shell currently has a foreground job running (see `pty::has_foreground_job`).
    fn tab_has_foreground_job(&self, id: u64) -> bool {
        self.model
            .borrow()
            .tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| pty::has_foreground_job(t.master_fd, t.shell_pid))
            .unwrap_or(false)
    }

    /// The model just dropped to zero tabs from an explicit user action (closing the last tab,
    /// deleting the last group). Keep the app open with the empty-state placeholder rather than
    /// quitting — only a further close/delete with nothing at all left should quit.
    fn went_empty(&self) {
        self.show_placeholder();
        self.update_title();
        self.refresh_sidebar();
        self.save();
    }

    fn close_tab_impl(&self, id: u64, quit_if_empty: bool) {
        let was_active = self.model.borrow().active == Some(id);
        // The neighbor tab must be computed before teardown (at this point id is still in visual order).
        let neighbor = if was_active { self.adjacent_tab(id) } else { None };
        if !self.teardown_tab(id) {
            return;
        }
        // Last tab closed.
        if self.model.borrow().tabs.is_empty() {
            if quit_if_empty {
                unsafe { NSApplication::sharedApplication(self.mtm).terminate(None) };
                return;
            }
            self.went_empty();
            return;
        }
        if was_active {
            let pick = neighbor
                .filter(|nid| self.model.borrow().tabs.iter().any(|t| t.id == *nid))
                .or_else(|| self.model.borrow().tabs.first().map(|t| t.id));
            if let Some(a) = pick {
                self.select(a);
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Rename a tab (committed after double-click in-place editing in the sidebar).
    pub fn rename_tab(&self, id: u64, name: String) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t.title = name,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
        self.update_title(); // when collapsed, the renamed tab may be the active one
    }

    /// Toggle a tab's locked state. A locked tab is protected from user-initiated close (⌘W / the
    /// tab menu's Close); the shell exiting on its own still closes it (see `close_tab`).
    pub fn toggle_tab_lock(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t.locked = !t.locked,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Whether the given tab is locked (protected from user-initiated close).
    fn is_tab_locked(&self, id: u64) -> bool {
        self.model.borrow().tabs.iter().find(|t| t.id == id).map(|t| t.locked).unwrap_or(false)
    }

    /// Set a tab's status-dot color (index into sidebar::DOT_COLORS; 0 = default/auto).
    pub fn set_tab_dot(&self, id: u64, dot: u8) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t.dot = dot,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Rename a group.
    pub fn rename_group(&self, gi: usize, name: String) {
        {
            let mut m = self.model.borrow_mut();
            match m.groups.get_mut(gi) {
                Some(g) => g.name = name,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    pub fn snapshot(&self) -> Snapshot {
        let m = self.model.borrow();
        let title_of = |id: &u64| m.tabs.iter().find(|t| t.id == *id).map(|t| (t.id, t.title.clone(), t.dot, t.locked));
        let ungrouped = m.ungrouped.iter().filter_map(title_of).collect();
        let groups = m
            .groups
            .iter()
            .map(|g| {
                let tabs = g.tabs.iter().filter_map(title_of).collect();
                GroupSnap { name: g.name.clone(), collapsed: g.collapsed, tabs }
            })
            .collect();
        Snapshot { ungrouped, groups, active: m.active, style: self.style.get() }
    }

    /// Switch the global color theme: immediately redraw the current terminal and sidebar, and persist.
    pub fn set_style(&self, idx: usize) {
        if idx >= theme::NAMES.len() || idx == self.style.get() {
            return;
        }
        self.style.set(idx);
        theme::set(theme::by_index(idx));
        // Only the active tab is present; the other tabs will be redrawn with the new theme the next time they are switched to.
        if let Some(a) = self.model.borrow().active {
            if let Some(tab) = self.model.borrow().tabs.iter().find(|t| t.id == a) {
                unsafe { tab.view.setNeedsDisplay(true) };
            }
        }
        unsafe { self.header.setNeedsDisplay(true) }; // header bg tracks the terminal color
        unsafe { self.placeholder.setNeedsDisplay(true) }; // empty-state colors track the theme too
        self.refresh_sidebar();
        self.save();
    }

    /// Adjust the global font size (delta is usually ±1).
    pub fn change_font_size(&self, delta: f64) {
        settings::set(&settings::family(), settings::size() + delta);
        self.reflow_all();
        self.save();
    }

    /// Toggle whether the sidebar/header separator borders are drawn (Settings → Border).
    pub fn set_show_border(&self, on: bool) {
        settings::set_show_border(on);
        self.refresh_sidebar();
        unsafe { self.header.setNeedsDisplay(true) };
        self.save();
    }

    /// Set the global font size to an absolute value (used by the settings dialog).
    pub fn set_font_size(&self, size: f64) {
        settings::set(&settings::family(), size);
        self.reflow_all();
        self.save();
    }

    /// ⌘0: restore the default font size.
    pub fn reset_font_size(&self) {
        self.set_font_size(settings::DEFAULT_SIZE);
    }

    /// Switch the global font family (settings::FAMILIES index).
    pub fn set_font_family(&self, idx: usize) {
        if let Some(fam) = settings::FAMILIES.get(idx) {
            settings::set(fam, settings::size());
            self.reflow_all();
            self.save();
        }
    }

    /// After a font change: reflow each tab's grid per the new metrics, and notify each PTY.
    fn reflow_all(&self) {
        let (cols, rows) = self.dims();
        let m = self.model.borrow();
        for tab in &m.tabs {
            tab.view.resize_grid(cols, rows);
            let ws = libc::winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(tab.master_fd, libc::TIOCSWINSZ, &ws) };
            unsafe { tab.view.setNeedsDisplay(true) };
        }
    }

    fn refresh_sidebar(&self) {
        unsafe { self.sidebar.setNeedsDisplay(true) };
    }

    /// Hand keyboard focus back to the current active terminal (used when exiting sidebar search).
    pub fn focus_terminal(&self) {
        let m = self.model.borrow();
        if let Some(a) = m.active {
            if let Some(tab) = m.tabs.iter().find(|t| t.id == a) {
                self.window.makeFirstResponder(Some(&tab.view));
            }
        }
    }

    /// Give keyboard focus to the sidebar (used when entering search).
    pub fn focus_sidebar(&self) {
        self.window.makeFirstResponder(Some(&self.sidebar));
    }

    /// ⌘F: enter sidebar search (expand first if collapsed).
    pub fn focus_search(&self) {
        if self.collapsed.get() {
            self.toggle_sidebar();
        }
        self.sidebar.begin_search();
    }

    /// ⌘,: open the settings dialog (built lazily, then reused).
    pub fn open_settings(&self) {
        if self.settings_dialog.borrow().is_none() {
            let d = SettingsDialog::new(self.mtm);
            d.set_controller(self as *const AppController);
            *self.settings_dialog.borrow_mut() = Some(d);
        }
        let d = self.settings_dialog.borrow();
        d.as_ref().unwrap().show(self.mtm);
    }

    /// ⌘W: close the active tab but keep the app running even if it was the last one.
    /// Only when there are no tabs at all does ⌘W quit the app.
    pub fn close_active_tab(&self) {
        let a = self.model.borrow().active;
        match a {
            Some(a) => self.close_tab_user(a),
            None => unsafe { NSApplication::sharedApplication(self.mtm).terminate(None) },
        }
    }

    /// Open the active tab's current directory in Finder.
    pub fn reveal_in_finder(&self) {
        if let Some(a) = self.model.borrow().active {
            self.reveal_in_finder_id(a);
        }
    }

    /// Open a specific tab's current directory in Finder (reported via OSC 7; falls back to the
    /// spawn-time directory when missing). Used by the per-tab context menu.
    pub fn reveal_in_finder_id(&self, id: u64) {
        let cwd = {
            let m = self.model.borrow();
            m.tabs
                .iter()
                .find(|t| t.id == id)
                .map(|t| {
                    let live = t.view.cwd();
                    if live.is_empty() { t.spawn_cwd.clone() } else { live }
                })
                .unwrap_or_default()
        };
        if !cwd.is_empty() {
            let _ = std::process::Command::new("open").arg(&cwd).spawn();
        }
    }

    /// Clear the screen (⌘K).
    pub fn clear_active(&self) {
        let m = self.model.borrow();
        if let Some(a) = m.active {
            if let Some(tab) = m.tabs.iter().find(|t| t.id == a) {
                tab.view.clear();
            }
        }
    }

    /// Collapse/expand the sidebar (⌘B). When collapsed, the terminal fills the entire content area.
    pub fn toggle_sidebar(&self) {
        self.collapsed.set(!self.collapsed.get());
        self.relayout();
        self.update_title();
    }

    /// Persist once on app exit (save a snapshot of each tab's current content). See the app delegate in main.
    pub fn persist(&self) {
        self.save();
    }

    /// Persist the layout + session state (cwd + each tab's currently visible content).
    fn save(&self) {
        let m = self.model.borrow();
        // A single tab id -> (title, cwd, dot, locked).
        let tab_state = |id: &u64| {
            m.tabs.iter().find(|t| t.id == *id).map(|t| {
                // cwd prefers the live OSC 7 report; falls back to the spawn-time directory when missing.
                let live = t.view.cwd();
                let cwd = if live.is_empty() { t.spawn_cwd.clone() } else { live };
                (t.title.clone(), cwd, t.dot, t.locked)
            })
        };
        let ungrouped: Vec<(String, String, u8, bool)> = m.ungrouped.iter().filter_map(tab_state).collect();
        let groups: Vec<(String, bool, Vec<(String, String, u8, bool)>)> = m
            .groups
            .iter()
            .map(|g| {
                let tabs = g.tabs.iter().filter_map(tab_state).collect();
                (g.name.clone(), g.collapsed, tabs)
            })
            .collect();
        drop(m);
        config::save(
            theme::NAMES[self.style.get()],
            &settings::family(),
            settings::size(),
            self.sidebar_w.get(),
            self.sidebar_right.get(),
            settings::show_border(),
            self.window_size().0,
            self.window_size().1,
            &ungrouped,
            &groups,
        );
    }
}

/// Shared wording for "closing this will kill a running process" confirmations.
const RUNNING_JOB_WARNING: &str = "A process is still running in this terminal. Closing it will terminate that process.";

/// A Close/Cancel-style confirmation alert. `affirmative` labels the destructive button (shown
/// first); returns true only if the user picked it.
fn confirm(mtm: MainThreadMarker, title: &str, info: &str, affirmative: &str) -> bool {
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(info));
        alert.addButtonWithTitle(&NSString::from_str(affirmative));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.runModal() == 1000 // NSAlertFirstButtonReturn
    }
}

/// TermView calls back into the controller through this when the shell exits (see `view::CloseFn`).
fn close_cb(ctx: *const c_void, id: u64) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.close_tab(id);
}

/// When TermView receives ⌘B it calls back into the controller to collapse the sidebar (see `view::CmdFn`).
fn toggle_cb(ctx: *const c_void) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.toggle_sidebar();
}

/// Deferred (next main-queue turn) traffic-light re-center; `p` is a `*const AppController`.
extern "C" fn reposition_trampoline(p: *mut c_void) {
    let ctrl = unsafe { &*(p as *const AppController) };
    ctrl.reposition_traffic_lights();
}
