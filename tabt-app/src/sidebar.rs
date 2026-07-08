//! Sidebar: self-drawn group + tab list.
//!
//! A custom NSView that draws group titles and tab rows itself, and handles clicks and drags itself:
//!   - two buttons at the top: "＋ New Terminal" and "＋ New Group";
//!   - each group has one title row, with its tabs listed indented below; click a tab to switch,
//!     drag a tab to another group to move it.
//!
//! All actions are forwarded to [`AppController`](crate::app::AppController). The view is never
//! the first responder (acceptsFirstResponder defaults to false); keyboard focus always stays on the terminal.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSEvent, NSFont, NSGraphicsContext, NSImage, NSMenu, NSMenuItem,
    NSRectClip, NSRectFill, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::app::{AppController, Snapshot};
use crate::settings;
use crate::theme;
use crate::view::{draw_symbol, draw_truncated, make_attrs, ns_color, rect};

pub const SIDEBAR_W: f64 = 232.0;
const ROW_H: f64 = 32.0; // session/settings row (per design spec)
const SECTION_H: f64 = 24.0; // "Sessions" section label row above the ungrouped tabs
const BTN_H: f64 = 26.0; // action button row (tighter top/bottom padding)
const SEARCH_H: f64 = 28.0;
const PAD: f64 = 14.0; // content left inset
const TOP_INSET: f64 = 40.0; // top title-bar zone (traffic lights + toggle); matches HEADER_H
const HPAD: f64 = 10.0; // row background (selected/hover/search box) inset from the sidebar's left and right edges
const GAP: f64 = 10.0;
const FROW_H: f64 = 32.0; // bottom settings row (same height as session rows)
const FPAD: f64 = 8.0; // settings row top/bottom margin (symmetric)
/// `group` sentinel for ungrouped tab rows (distinct from the button's usize::MAX).
const UNGROUPED: usize = usize::MAX - 1;

const DOT_RUNNING: (f64, f64, f64) = (52.0 / 255.0, 199.0 / 255.0, 89.0 / 255.0); // #34c759

// ---- Text hierarchy: derived from the active theme so it stays legible on light and dark themes.
// The primary color is the theme foreground; weaker tiers blend it toward the background.
fn text_primary() -> (f64, f64, f64) {
    theme::current().fg
}
fn text_secondary() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.30)
}
fn text_placeholder() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.55)
}
fn text_weakest() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.66)
}
/// Neutral overlay for box fills/hover (an alpha wash of the theme foreground): white-ish on
/// dark themes, dark on light themes — unlike a fixed white wash that vanishes on a light background.
fn overlay(alpha: f64) -> Retained<NSColor> {
    let f = theme::current().fg;
    rgba(f.0, f.1, f.2, alpha)
}

/// Classic status-dot colors. Index 0 = Default (auto: green when active, gray otherwise);
/// 1..=8 are explicit colors shown regardless of active state. Kept in sync with the
/// dot-color menu and the per-tab `dot` index persisted in the layout file.
pub const DOT_COLORS: [(&str, (f64, f64, f64)); 9] = [
    ("Default", (0.0, 0.0, 0.0)), // sentinel: never drawn directly (see draw_row)
    ("Red", (1.0, 69.0 / 255.0, 58.0 / 255.0)),
    ("Orange", (1.0, 159.0 / 255.0, 10.0 / 255.0)),
    ("Yellow", (1.0, 214.0 / 255.0, 10.0 / 255.0)),
    ("Green", (50.0 / 255.0, 215.0 / 255.0, 75.0 / 255.0)),
    ("Blue", (10.0 / 255.0, 132.0 / 255.0, 1.0)),
    ("Purple", (191.0 / 255.0, 90.0 / 255.0, 242.0 / 255.0)),
    ("Pink", (1.0, 55.0 / 255.0, 95.0 / 255.0)),
    ("Gray", (152.0 / 255.0, 152.0 / 255.0, 157.0 / 255.0)),
];

/// Target hit by a single press (Copy, so it fits in a Cell).
#[derive(Clone, Copy, PartialEq)]
enum Press {
    None,
    Search,
    Actions, // the row holding the side-by-side "Terminal" and "Group" buttons
    NewTab,
    NewGroup,
    Group(usize),
    Tab(u64, usize),  // (tab id, index of its group)
    GroupMenu(usize), // "⋯" at the right of a group row; click to pop up the group menu
    TabMenu(u64),     // "⋯" at the right of a tab row; click to pop up the tab menu
    TabDot(u64),      // the status dot at the left of a tab row; click to pick its color
    TabsLabel,        // "Sessions" section header above the ungrouped tabs (non-interactive)
    NotesLabel,
    NoteOpen(usize),
    NoteRecent(usize),
    NoteSubLabel,
    StyleMenu,        // bottom style row; click to pop up the color scheme menu
}

/// A single laid-out row.
struct Row {
    top: f64,
    h: f64,
    indent: f64,
    label: String,
    kind: Press,
    selected: bool,
    collapsed: bool, // only meaningful for group rows: collapsed state
    group: usize,    // the group this row belongs to (usize::MAX for button rows)
    dot: u8,         // tab rows: status-dot color index (0 = default/auto)
    locked: bool,    // tab rows: whether the tab is locked (protected from close)
}

pub struct SidebarIvars {
    controller: Cell<*const AppController>,
    font: Retained<NSFont>,
    font_small: Retained<NSFont>, // small font used for group section labels
    press: Cell<Press>,
    start_y: Cell<f64>,
    cur_x: Cell<f64>, // x of the most recent press (used to pop up menus at the mouse)
    cur_y: Cell<f64>,
    dragging: Cell<bool>,
    scroll: Cell<f64>, // vertical scroll amount of the list area (>=0, pixels the content shifts up)
    // Mouse hover: x/y within the view + whether inside the view (used for row highlight / dual-button split / hover "⋯").
    hover_x: Cell<f64>,
    hover_y: Cell<f64>,
    hovering: Cell<bool>,
    tracking_added: Cell<bool>,
    // Search: query string + whether in search (focused) state.
    query: RefCell<String>,
    searching: Cell<bool>,
    // Rename: object being edited (None = not editing) + edit buffer.
    editing: Cell<Option<Editing>>,
    edit_buf: RefCell<String>,
    // Text caret position (char index) for the active input (search query or rename buffer).
    caret: Cell<usize>,
    // Tab id the color-picker menu currently applies to (set when the menu opens).
    dot_target: Cell<u64>,
}

/// The object being renamed in place.
#[derive(Clone, Copy, PartialEq)]
enum Editing {
    Tab(u64),
    Group(usize),
}

declare_class!(
    pub struct SidebarView;

    unsafe impl ClassType for SidebarView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "SidebarView";
    }

    impl DeclaredClass for SidebarView {
        type Ivars = SidebarIvars;
    }

    unsafe impl NSObjectProtocol for SidebarView {}

    unsafe impl SidebarView {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            self.on_down(event);
        }

        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.on_drag(event);
        }

        #[method(mouseUp:)]
        fn mouse_up(&self, event: &NSEvent) {
            self.on_up(event);
        }

        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.on_scroll(event);
        }

        // Right-click: on a group/tab row, pop up the corresponding "more" menu.
        #[method(rightMouseDown:)]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.on_right_down(event);
        }

        // Attach a mouse tracking area covering the visible region (InVisibleRect auto-adapts to size, so add it only once).
        #[method(updateTrackingAreas)]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            if self.ivars().tracking_added.get() {
                return;
            }
            let mtm = MainThreadMarker::new().expect("main thread");
            let opts = NSTrackingAreaOptions::NSTrackingMouseEnteredAndExited
                | NSTrackingAreaOptions::NSTrackingMouseMoved
                | NSTrackingAreaOptions::NSTrackingActiveInKeyWindow
                | NSTrackingAreaOptions::NSTrackingInVisibleRect;
            let owner: &AnyObject = unsafe { &*(self as *const Self as *const AnyObject) };
            let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(mtm.alloc(), zero, opts, Some(owner), None)
            };
            unsafe { self.addTrackingArea(&area) };
            self.ivars().tracking_added.set(true);
        }

        #[method(mouseMoved:)]
        fn mouse_moved(&self, event: &NSEvent) {
            self.set_hover(event);
        }

        #[method(mouseEntered:)]
        fn mouse_entered(&self, event: &NSEvent) {
            self.set_hover(event);
        }

        #[method(mouseExited:)]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovering.set(false);
            unsafe { self.setNeedsDisplay(true) };
        }

        // Only take over the keyboard in search/rename state; otherwise leave focus to the terminal.
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            self.ivars().searching.get() || self.ivars().editing.get().is_some()
        }

        // Click elsewhere (e.g. the terminal) → exit search, abandon rename.
        #[method(resignFirstResponder)]
        fn resign_first_responder(&self) -> bool {
            self.ivars().searching.set(false);
            self.ivars().query.borrow_mut().clear();
            self.ivars().editing.set(None);
            unsafe { self.setNeedsDisplay(true) };
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            self.on_key(event);
        }

        // ---- Group/tab "⋯" menu-item handlers (tag holds the group index or tab id) ----
        #[method(groupRename:)]
        fn group_rename(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            self.start_edit(Editing::Group(gi));
        }

        #[method(groupToggle:)]
        fn group_toggle(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            if let Some(ctrl) = self.controller() {
                ctrl.toggle_group_collapsed(gi);
            }
        }

        #[method(groupDelete:)]
        fn group_delete(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            if let Some(ctrl) = self.controller() {
                ctrl.delete_group(gi);
            }
        }

        #[method(tabRename:)]
        fn tab_rename(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            self.start_edit(Editing::Tab(id));
        }

        #[method(tabRevealInFinder:)]
        fn tab_reveal_in_finder(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.reveal_in_finder_id(id);
            }
        }

        #[method(tabToggleLock:)]
        fn tab_toggle_lock(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.toggle_tab_lock(id);
            }
        }

        #[method(tabClose:)]
        fn tab_close(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.close_tab_user(id);
            }
        }

        // Color picked from the status-dot menu: tag = DOT_COLORS index, applied to the pending tab.
        #[method(pickDotColor:)]
        fn pick_dot_color(&self, item: &NSMenuItem) {
            let idx = unsafe { item.tag() } as u8;
            let id = self.ivars().dot_target.get();
            if let Some(ctrl) = self.controller() {
                ctrl.set_tab_dot(id, idx);
            }
        }
    }
);

impl SidebarView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let font = unsafe { NSFont::systemFontOfSize(12.0) };
        let font_small = unsafe { NSFont::systemFontOfSize(10.5) };
        let this = mtm.alloc();
        let this = this.set_ivars(SidebarIvars {
            controller: Cell::new(std::ptr::null()),
            font,
            font_small,
            press: Cell::new(Press::None),
            start_y: Cell::new(0.0),
            cur_x: Cell::new(0.0),
            cur_y: Cell::new(0.0),
            dragging: Cell::new(false),
            scroll: Cell::new(0.0),
            hover_x: Cell::new(-1.0),
            hover_y: Cell::new(-1.0),
            hovering: Cell::new(false),
            tracking_added: Cell::new(false),
            query: RefCell::new(String::new()),
            searching: Cell::new(false),
            editing: Cell::new(None),
            edit_buf: RefCell::new(String::new()),
            caret: Cell::new(0),
            dot_target: Cell::new(0),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    pub fn set_controller(&self, c: *const AppController) {
        self.ivars().controller.set(c);
    }

    fn controller(&self) -> Option<&AppController> {
        let p = self.ivars().controller.get();
        if p.is_null() {
            None
        } else {
            Some(unsafe { &*p })
        }
    }

    /// y coordinate within the view (already a flipped coordinate system, origin at top-left).
    /// Record the hover point (x/y) and request a redraw.
    fn set_hover(&self, event: &NSEvent) {
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        self.ivars().hover_x.set(p.x);
        self.ivars().hover_y.set(p.y);
        self.ivars().hovering.set(true);
        unsafe { self.setNeedsDisplay(true) };
    }

    fn point_y(&self, event: &NSEvent) -> f64 {
        let p = unsafe { event.locationInWindow() };
        self.convertPoint_fromView(p, None).y
    }

    /// Top y of the group/tab list area (below the search box + the two buttons). Above this is the fixed area, which does not scroll.
    fn list_top() -> f64 {
        TOP_INSET + SEARCH_H + GAP + BTN_H + GAP
    }

    /// Build all rows. `scroll` only affects groups/tabs (the list area); search/buttons stay fixed.
    /// A list row's `top` is returned directly as a screen coordinate (scroll already subtracted), so hit testing and dragging need no further conversion.
    fn build_rows(snap: &Snapshot, query: &str, scroll: f64) -> Vec<Row> {
        let q = query.to_lowercase();
        let mut rows = Vec::new();
        let mut y = TOP_INSET;
        // Search box (label holds the current query; drawn specially in render).
        rows.push(Row { top: y, h: SEARCH_H, indent: PAD, label: query.to_string(), kind: Press::Search, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
        y += SEARCH_H + GAP;

        // Side-by-side "Terminal" and "Group" buttons, occupying one row.
        rows.push(Row { top: y, h: BTN_H, indent: PAD, label: String::new(), kind: Press::Actions, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
        y += BTN_H + GAP;

        // Ungrouped tabs (the "会话" session list), rendered at the top with a shallow indent.
        let matched_ung: Vec<&(u64, String, u8, bool)> = snap
            .ungrouped
            .iter()
            .filter(|(_, t, _, _)| q.is_empty() || t.to_lowercase().contains(&q))
            .collect();
        if !matched_ung.is_empty() {
            // "Sessions" section label above the tabs (matches the GROUP labels below).
            rows.push(Row { top: y - scroll, h: SECTION_H, indent: PAD, label: "Sessions".to_string(), kind: Press::TabsLabel, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
            y += SECTION_H;
            for (id, title, dot, locked) in matched_ung {
                let selected = snap.active == Some(*id);
                rows.push(Row { top: y - scroll, h: ROW_H, indent: 16.0, label: title.clone(), kind: Press::Tab(*id, UNGROUPED), selected, collapsed: false, group: UNGROUPED, dot: *dot, locked: *locked });
                y += ROW_H;
            }
        }

        for (gi, g) in snap.groups.iter().enumerate() {
            // Filter: when the query is non-empty, keep only tabs whose title matches, and hide groups with no match.
            let matched: Vec<&(u64, String, u8, bool)> = g
                .tabs
                .iter()
                .filter(|(_, t, _, _)| q.is_empty() || t.to_lowercase().contains(&q))
                .collect();
            if !q.is_empty() && matched.is_empty() {
                continue;
            }
            rows.push(Row { top: y - scroll, h: ROW_H, indent: PAD, label: g.name.clone(), kind: Press::Group(gi), selected: false, collapsed: g.collapsed, group: gi, dot: 0, locked: false });
            y += ROW_H;
            // Hide tabs when collapsed and not in search state; while searching, always show matches (to make collapsed tabs findable).
            if g.collapsed && q.is_empty() {
                continue;
            }
            for (id, title, dot, locked) in matched {
                let selected = snap.active == Some(*id);
                rows.push(Row { top: y - scroll, h: ROW_H, indent: 26.0, label: title.clone(), kind: Press::Tab(*id, gi), selected, collapsed: false, group: gi, dot: *dot, locked: *locked });
                y += ROW_H;
            }
        }

        let matched_open: Vec<(usize, &crate::app::NoteSnap)> = snap
            .open_notes
            .iter()
            .enumerate()
            .filter(|(_, n)| q.is_empty() || n.title.to_lowercase().contains(&q))
            .collect();
        let matched_recent: Vec<(usize, &crate::app::NoteSnap)> = snap
            .recent_notes
            .iter()
            .enumerate()
            .filter(|(_, n)| q.is_empty() || n.title.to_lowercase().contains(&q))
            .collect();
        if !matched_open.is_empty() || !matched_recent.is_empty() || q.is_empty() {
            rows.push(Row { top: y - scroll, h: SECTION_H, indent: PAD, label: "Note".to_string(), kind: Press::NotesLabel, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
            y += SECTION_H;
            if !matched_open.is_empty() || q.is_empty() {
                rows.push(Row { top: y - scroll, h: SECTION_H, indent: PAD + 10.0, label: "Open".to_string(), kind: Press::NoteSubLabel, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
                y += SECTION_H;
                for (idx, note) in matched_open {
                    let selected = snap.active_note.as_deref() == Some(note.path.as_str());
                    rows.push(Row { top: y - scroll, h: ROW_H, indent: 16.0, label: note.title.clone(), kind: Press::NoteOpen(idx), selected, collapsed: false, group: usize::MAX, dot: 0, locked: false });
                    y += ROW_H;
                }
            }
            if !matched_recent.is_empty() || q.is_empty() {
                rows.push(Row { top: y - scroll, h: SECTION_H, indent: PAD + 10.0, label: "Recent".to_string(), kind: Press::NoteSubLabel, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
                y += SECTION_H;
                for (_, note) in matched_recent {
                    rows.push(Row { top: y - scroll, h: ROW_H, indent: 16.0, label: note.title.clone(), kind: Press::NoteRecent(note.index), selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false });
                    y += ROW_H;
                }
            }
        }
        rows
    }

    fn is_list_row(kind: Press) -> bool {
        matches!(
            kind,
            Press::Group(_)
                | Press::Tab(..)
                | Press::TabsLabel
                | Press::NotesLabel
                | Press::NoteSubLabel
                | Press::NoteOpen(_)
                | Press::NoteRecent(_)
        )
    }

    /// Height of the bottom settings area (settings row + symmetric top/bottom margins).
    fn footer_height() -> f64 {
        FROW_H + 2.0 * FPAD
    }

    /// Bottom settings row: click to pop up the settings menu (color scheme / font / font size). Top/bottom margins are symmetric.
    fn footer_rows(_snap: &Snapshot, height: f64) -> Vec<Row> {
        let y = (height - Self::footer_height()).max(0.0) + FPAD;
        vec![Row {
            top: y,
            h: FROW_H,
            indent: PAD,
            label: "Settings".to_string(),
            kind: Press::StyleMenu,
            selected: false,
            collapsed: false,
            group: usize::MAX,
            dot: 0,
            locked: false,
        }]
    }

    /// Merged rows of the top list + bottom style area (used for drawing and hit testing).
    fn all_rows(snap: &Snapshot, height: f64, query: &str, scroll: f64) -> Vec<Row> {
        let mut rows = Self::build_rows(snap, query, scroll);
        rows.extend(Self::footer_rows(snap, height));
        rows
    }

    /// Current scroll amount (reads the ivar).
    fn scroll(&self) -> f64 {
        self.ivars().scroll.get()
    }

    /// Maximum scroll amount when list content exceeds the visible height (0 if content is short).
    fn max_scroll(&self, snap: &Snapshot, query: &str, height: f64) -> f64 {
        let rows = Self::build_rows(snap, query, 0.0);
        Self::max_scroll_of(&rows, height)
    }

    /// Same computation as `max_scroll`, but from an already-built (unscrolled) row list —
    /// lets `render()` measure and position rows from a single `build_rows` call instead of two.
    fn max_scroll_of(rows: &[Row], height: f64) -> f64 {
        let content_bottom = rows
            .iter()
            .filter(|r| Self::is_list_row(r.kind))
            .map(|r| r.top + r.h)
            .fold(Self::list_top(), f64::max);
        let footer_top = height - Self::footer_height();
        (content_bottom - footer_top).max(0.0)
    }

    fn render(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        let query = self.ivars().query.borrow().clone();
        let searching = self.ivars().searching.get();
        let editing = self.ivars().editing.get();
        let bounds = self.bounds();
        let (w, h) = (bounds.size.width, bounds.size.height);
        let footer_top = h - Self::footer_height();
        let list_top = Self::list_top();

        // Build the scrollable rows once (unscrolled), derive max_scroll from that same build,
        // then shift positions for the actual scroll offset — avoids cloning every tab/group
        // title a second time via a second build_rows call.
        let mut rows = Self::build_rows(&snap, &query, 0.0);
        let max_scroll = Self::max_scroll_of(&rows, h);
        let scroll = self.scroll().clamp(0.0, max_scroll);
        self.ivars().scroll.set(scroll);
        if scroll > 0.0 {
            for r in &mut rows {
                if Self::is_list_row(r.kind) {
                    r.top -= scroll;
                }
            }
        }
        rows.extend(Self::footer_rows(&snap, h));

        unsafe {
            // Sidebar background + separators derive from the active theme (they change with it).
            let t = theme::current();
            ns_color(t.sidebar_bg()).set();
            NSRectFill(bounds);
            // Separators are optional (Settings → Border) and derive from the theme.
            if settings::show_border() {
                // Edge 1px separator on the side facing the terminal: left edge when the sidebar is
                // docked on the right, right edge otherwise.
                let on_right = self.controller().map(|c| c.sidebar_on_right()).unwrap_or(false);
                let edge_x = if on_right { 0.0 } else { w - 1.0 };
                ns_color(t.border()).set();
                NSRectFill(rect(edge_x, 0.0, 1.0, h));
                // Separator line above the bottom settings area
                ns_color(t.border()).set();
                NSRectFill(rect(0.0, footer_top - 1.0, w, 1.0));
            }
        }

        // Hover hit (don't show hover highlight while dragging, to avoid overlapping the drop line).
        let hovering = self.ivars().hovering.get() && !self.ivars().dragging.get();
        let hy = self.ivars().hover_y.get();
        let hovered = |row: &Row| hovering && hy >= row.top && hy < row.top + row.h;

        // The fixed area (search box + two buttons + bottom style row) doesn't scroll; draw directly.
        for row in &rows {
            if !Self::is_list_row(row.kind) {
                self.draw_row(row, w, &query, searching, editing, hovered(row));
            }
        }

        // The list area (sessions and notes) is clipped to [list_top, footer_top) and drawn scrolled.
        let list_rect = rect(0.0, list_top, w, (footer_top - list_top).max(0.0));
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(list_rect) };
        for row in &rows {
            if Self::is_list_row(row.kind) {
                self.draw_row(row, w, &query, searching, editing, hovered(row));
            }
        }
        // Drag drop line (theme-aligned; clipped within the list area, so it won't spill into the fixed area).
        if self.ivars().dragging.get() {
            if let Some(y) = self.drop_indicator_y(&snap) {
                round_fill(rect(8.0, y - 1.5, w - 16.0, 3.0), 1.5, &overlay(0.7));
            }
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }

        // Right-edge scrollbar indicator: hidden by default, shown only while the mouse hovers the sidebar (like the system overlay style).
        if max_scroll > 0.0 && self.ivars().hovering.get() {
            let track_h = footer_top - list_top;
            let content_h = track_h + max_scroll;
            let thumb_h = (track_h * track_h / content_h).max(28.0);
            let thumb_y = list_top + (scroll / max_scroll) * (track_h - thumb_h);
            round_fill(rect(w - 7.0, thumb_y, 3.0, thumb_h), 1.5, &rgba(0.45, 0.45, 0.52, 0.9));
        }
    }

    /// Draw a single row (search box / rename box / button / group / tab / style row).
    fn draw_row(&self, row: &Row, w: f64, query: &str, searching: bool, editing: Option<Editing>, hovered: bool) {
        if let Press::Search = row.kind {
            self.draw_search(row, w, query, searching);
            return;
        }
        // Tab/group being renamed: draw the in-place edit box instead of the normal title.
        let editing_this = match row.kind {
            Press::Tab(id, _) => editing == Some(Editing::Tab(id)),
            Press::Group(gi) => editing == Some(Editing::Group(gi)),
            _ => false,
        };
        if editing_this {
            self.draw_edit(row, w);
            return;
        }

        // Side-by-side "Terminal" and "Group" buttons.
        if let Press::Actions = row.kind {
            self.draw_actions(row, w);
            return;
        }

        let inset = rect(HPAD, row.top + 1.0, w - 2.0 * HPAD, row.h - 2.0);
        let vmid = |ih: f64| row.top + (row.h - ih) / 2.0; // vertically center icon/text

        // ---- Top-level list labels (mirrors the GROUP labels, no folder icon) ----
        if matches!(row.kind, Press::TabsLabel | Press::NotesLabel) {
            draw_truncated(&row.label.to_uppercase(), rect(row.indent, vmid(13.0), (w - 2.0 * PAD).max(0.0), 15.0), &self.ivars().font_small, text_weakest());
            return;
        }

        // ---- Note sublabels ("Open" / "Recent") ----
        if let Press::NoteSubLabel = row.kind {
            draw_truncated(&row.label.to_uppercase(), rect(row.indent, vmid(13.0), (w - 2.0 * PAD).max(0.0), 15.0), &self.ivars().font_small, text_weakest());
            return;
        }

        // ---- Group title: section label + system folder icon ----
        if let Press::Group(_) = row.kind {
            if hovered {
                round_fill(inset, 7.0, &overlay(0.06));
            }
            let folder = if row.collapsed { "folder.fill" } else { "folder" };
            draw_symbol(folder, rect(row.indent, vmid(12.0), 14.0, 12.0), text_weakest());
            let label_x = row.indent + 20.0;
            draw_truncated(&row.label, rect(label_x, vmid(13.0), (w - 34.0 - label_x).max(0.0), 15.0), &self.ivars().font_small, text_weakest());
            if hovered {
                draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
            }
            return;
        }

        // ---- Bottom settings row: gear + Settings + ⌘, badge (8px inset inside the container) ----
        if let Press::StyleMenu = row.kind {
            if hovered {
                round_fill(inset, 7.0, &overlay(0.06));
            }
            let ip = 8.0; // inset relative to the hover container (HPAD..w-HPAD)
            let col = text_secondary(); // theme-aligned foreground
            draw_symbol("gearshape", rect(HPAD + ip, vmid(15.0), 15.0, 15.0), col);
            let attrs = make_attrs(&self.ivars().font, Some(&ns_color(col)));
            let ns = NSString::from_str("Settings");
            unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(HPAD + ip + 22.0, vmid(16.0)), Some(&attrs)) };
            self.draw_badge("⌘,", w - HPAD - ip, row.top + row.h / 2.0);
            return;
        }

        // ---- Note row ----
        if matches!(row.kind, Press::NoteOpen(_) | Press::NoteRecent(_)) {
            if row.selected {
                round_fill(inset, 7.0, &overlay(0.16));
                round_stroke(inset, 7.0, 1.0, &overlay(0.24));
            } else if hovered {
                round_fill(inset, 7.0, &overlay(0.06));
            }
            let fg = if row.selected { text_primary() } else { text_secondary() };
            draw_symbol("doc.text", rect(row.indent + 2.0, vmid(14.0), 13.0, 14.0), fg);
            let name_x = row.indent + 22.0;
            draw_truncated(&row.label, rect(name_x, vmid(16.0), (w - 28.0 - name_x).max(0.0), 18.0), &self.ivars().font, fg);
            return;
        }

        // ---- Session row (Tab) ----
        // Selection highlight is a theme-aligned neutral wash (adapts to light/dark), not a fixed accent.
        if row.selected {
            round_fill(inset, 7.0, &overlay(0.16));
            round_stroke(inset, 7.0, 1.0, &overlay(0.24));
        } else if hovered {
            round_fill(inset, 7.0, &overlay(0.06));
        }
        // Status dot: an explicit per-tab color if set, else auto (active = green, otherwise = gray).
        // Index defensively (the dot index comes from the on-disk layout file and may be out of range).
        let dot = if let Some((_, c)) = DOT_COLORS.get(row.dot as usize).filter(|_| row.dot != 0) {
            *c
        } else if row.selected {
            DOT_RUNNING
        } else {
            (0.37, 0.37, 0.40)
        };
        round_fill(rect(row.indent, vmid(6.0), 6.0, 6.0), 3.0, &ns_color(dot));
        // Small terminal icon + session name.
        let fg = if row.selected { text_primary() } else { text_secondary() };
        draw_symbol("terminal", rect(row.indent + 12.0, vmid(12.0), 14.0, 12.0), fg);
        // Name truncates with an ellipsis; leaves room for the right-side meta / "⋯".
        let name_x = row.indent + 30.0;
        draw_truncated(&row.label, rect(name_x, vmid(16.0), (w - 40.0 - name_x).max(0.0), 18.0), &self.ivars().font, fg);
        // Right side: "⋯" while hovered (so the menu — including Unlock — is reachable on any tab);
        // otherwise a lock glyph for locked tabs, "⋯" for the selected tab, and nothing at rest.
        if hovered {
            draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
        } else if row.locked {
            draw_symbol("lock.fill", rect(w - 26.0, vmid(12.0), 11.0, 12.0), text_placeholder());
        } else if row.selected {
            draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
        }
    }

    /// The row of side-by-side "Terminal" and "Group" buttons.
    fn draw_actions(&self, row: &Row, w: f64) {
        let hovering = self.ivars().hovering.get() && !self.ivars().dragging.get();
        let hx = self.ivars().hover_x.get();
        let hy = self.ivars().hover_y.get();
        let in_row = hovering && hy >= row.top && hy < row.top + row.h;
        let gap = 8.0;
        let bw = (w - 2.0 * HPAD - gap) / 2.0;
        let left = rect(HPAD, row.top, bw, row.h);
        let right = rect(HPAD + bw + gap, row.top, bw, row.h);
        let hover_left = in_row && hx < HPAD + bw + gap / 2.0;
        let hover_right = in_row && !hover_left;

        // Both buttons share the same neutral low-opacity white style (no accent highlight).
        let lbg = if hover_left { overlay(0.10) } else { overlay(0.06) };
        round_fill(left, 7.0, &lbg);
        round_stroke(left, 7.0, 1.0, &overlay(0.07));
        self.draw_btn_content(left, "plus", "Terminal", text_secondary());

        let rbg = if hover_right { overlay(0.10) } else { overlay(0.06) };
        round_fill(right, 7.0, &rbg);
        round_stroke(right, 7.0, 1.0, &overlay(0.07));
        self.draw_btn_content(right, "folder", "Group", text_secondary());
    }

    /// Button content: icon + text, horizontally centered.
    fn draw_btn_content(&self, r: NSRect, icon: &str, label: &str, col: (f64, f64, f64)) {
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(col)));
        let ns = NSString::from_str(label);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        let (icon_w, ig) = (13.0, 6.0);
        let start = r.origin.x + (r.size.width - (icon_w + ig + tw)) / 2.0;
        let cy = r.origin.y + r.size.height / 2.0;
        draw_symbol(icon, rect(start, cy - 6.5, icon_w, 13.0), col);
        unsafe {
            ns.drawAtPoint_withAttributes(NSPoint::new(start + icon_w + ig, cy - 8.0), Some(&attrs));
        }
    }

    /// Shortcut hint: small monospace text right-aligned to `right_x`, vertically centered at `cy` (no background).
    fn draw_badge(&self, text: &str, right_x: f64, cy: f64) {
        let attrs = make_attrs(&self.ivars().font_small, Some(&ns_color(text_weakest())));
        let ns = NSString::from_str(text);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        unsafe {
            ns.drawAtPoint_withAttributes(NSPoint::new(right_x - tw, cy - 13.0 / 2.0), Some(&attrs));
        }
    }

    /// Which "region" a row belongs to: None = ungrouped region, Some(gi) = a group; non-list rows return the outer None.
    fn row_region(row: &Row) -> Option<Option<usize>> {
        match row.kind {
            Press::Group(gi) => Some(Some(gi)),
            Press::Tab(_, _) => Some(if row.group == UNGROUPED { None } else { Some(row.group) }),
            _ => None,
        }
    }

    /// Tab drag drop target: returns (target region Some(group)/None(ungrouped), which tab to insert before / None = end).
    /// Excludes the dragged tab itself during computation, to support reordering within the same region.
    fn tab_drop_target(&self, snap: &Snapshot, dragged: u64) -> Option<(Option<usize>, Option<u64>)> {
        let query = self.ivars().query.borrow().clone();
        let rows = Self::build_rows(snap, &query, self.scroll());
        let y = self.ivars().cur_y.get();
        // The region of the row that's hit is the target region.
        let mut region = None;
        for row in &rows {
            if y >= row.top && y < row.top + row.h {
                if let Some(r) = Self::row_region(row) {
                    region = Some(r);
                    break;
                }
            }
        }
        let region = region.unwrap_or_else(|| {
            // No row hit: above the first group title → ungrouped region; otherwise the last group (ungrouped region if there are no groups).
            let first_group_top = rows.iter().find_map(|r| match r.kind {
                Press::Group(_) => Some(r.top),
                _ => None,
            });
            match first_group_top {
                Some(gt) if y < gt => None,
                _ => snap.groups.len().checked_sub(1).map(Some).unwrap_or(None),
            }
        });
        // Insert before the first tab in the target region whose vertical midpoint is below the cursor; append if all are above.
        let before = rows
            .iter()
            .filter(|r| Self::row_region(r) == Some(region) && matches!(r.kind, Press::Tab(..)))
            .filter_map(|r| match r.kind {
                Press::Tab(tid, _) if tid != dragged => Some((tid, r.top + r.h / 2.0)),
                _ => None,
            })
            .find(|(_, mid)| y < *mid)
            .map(|(tid, _)| tid);
        Some((region, before))
    }

    /// The y (insertion position) the drag placeholder line should snap to; None means don't draw it.
    /// Kept consistent with `on_up`'s drop decision, so the preview line faithfully reflects the final drop position.
    fn drop_indicator_y(&self, snap: &Snapshot) -> Option<f64> {
        let query = self.ivars().query.borrow().clone();
        let rows = Self::build_rows(snap, &query, self.scroll());
        let list_bottom = rows.last().map(|r| r.top + r.h).unwrap_or(0.0);
        let y = self.ivars().cur_y.get();
        match self.ivars().press.get() {
            // Group: the line snaps to the top edge of the target-th group title; past the end, snaps to the list bottom.
            Press::Group(_) => {
                let heads: Vec<f64> = rows
                    .iter()
                    .filter_map(|r| match r.kind {
                        Press::Group(_) => Some(r.top),
                        _ => None,
                    })
                    .collect();
                let mut target = 0usize;
                for (i, top) in heads.iter().enumerate() {
                    let bottom = heads.get(i + 1).copied().unwrap_or(list_bottom);
                    if (top + bottom) / 2.0 < y {
                        target += 1;
                    }
                }
                Some(heads.get(target).copied().unwrap_or(list_bottom))
            }
            // Tab: the line snaps to the insertion position — the top edge of the `before` tab, or the bottom edge of the target region's last row.
            Press::Tab(dragged, _) => {
                let (region, before) = self.tab_drop_target(snap, dragged)?;
                match before {
                    Some(bid) => rows
                        .iter()
                        .find(|r| matches!(r.kind, Press::Tab(tid, _) if tid == bid))
                        .map(|r| r.top),
                    None => rows
                        .iter()
                        .filter(|r| Self::row_region(r) == Some(region))
                        .map(|r| r.top + r.h)
                        .fold(None, |acc: Option<f64>, b| Some(acc.map_or(b, |a: f64| a.max(b)))),
                }
            }
            _ => None,
        }
    }

    /// Scroll wheel/trackpad: scroll the list area up/down (clamped to 0..max_scroll).
    fn on_scroll(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let query = self.ivars().query.borrow().clone();
        let h = self.bounds().size.height;
        let max = self.max_scroll(&snap, &query, h);
        if max <= 0.0 {
            if self.scroll() != 0.0 {
                self.ivars().scroll.set(0.0);
                unsafe { self.setNeedsDisplay(true) };
            }
            return;
        }
        let dy = unsafe { event.scrollingDeltaY() };
        let next = (self.scroll() - dy).clamp(0.0, max);
        if next != self.scroll() {
            self.ivars().scroll.set(next);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    /// Hit test: which row the y within the view hits (list rows are only valid within the visible area, to avoid mis-hits on scrolled-out rows).
    fn row_at(&self, snap: &Snapshot, y: f64, h: f64, query: &str) -> Press {
        let footer_top = h - Self::footer_height();
        for row in &Self::all_rows(snap, h, query, self.scroll()) {
            let list_row = Self::is_list_row(row.kind);
            if list_row && (y < Self::list_top() || y >= footer_top) {
                continue;
            }
            if y >= row.top && y < row.top + row.h {
                return row.kind;
            }
        }
        Press::None
    }

    /// Right-click: on a group/tab row, pop up the corresponding "more" menu (positioned at the mouse).
    fn on_right_down(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        let h = self.bounds().size.height;
        let query = self.ivars().query.borrow().clone();
        self.ivars().cur_x.set(p.x); // so popup positions at the mouse
        self.ivars().cur_y.set(p.y);
        match self.row_at(&snap, p.y, h, &query) {
            Press::Group(gi) => self.open_group_menu(gi),
            Press::Tab(id, _) => self.open_tab_menu(id),
            _ => {}
        }
    }

    /// Scroll the list to the bottom (called after creating a group, so the new group at the end is visible).
    pub fn scroll_to_bottom(&self) {
        if let Some(ctrl) = self.controller() {
            let snap = ctrl.snapshot();
            let query = self.ivars().query.borrow().clone();
            let h = self.bounds().size.height;
            let max = self.max_scroll(&snap, &query, h);
            self.ivars().scroll.set(max);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn on_down(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        let (x, y) = (p.x, p.y);
        let (w, h) = (self.bounds().size.width, self.bounds().size.height);
        let query = self.ivars().query.borrow().clone();
        let mut press = self.row_at(&snap, y, h, &query);
        // Dual-button row: by x, land on "Terminal" (left) or "Group" (right).
        if press == Press::Actions {
            let gap = 8.0;
            let bw = (w - 2.0 * HPAD - gap) / 2.0;
            press = if x < HPAD + bw + gap / 2.0 { Press::NewTab } else { Press::NewGroup };
        }
        // When hitting the "⋯" area at the right of a group/tab row, pop up the corresponding more menu instead.
        if x >= w - 28.0 {
            press = match press {
                Press::Group(gi) => Press::GroupMenu(gi),
                Press::Tab(id, _) => Press::TabMenu(id),
                other => other,
            };
        }
        // Clicking the status dot at the left of a tab row → open its color picker.
        if let Press::Tab(id, grp) = press {
            let indent = if grp == UNGROUPED { 16.0 } else { 26.0 };
            if x >= indent - 3.0 && x <= indent + 11.0 {
                press = Press::TabDot(id);
            }
        }
        self.ivars().press.set(press);
        self.ivars().start_y.set(y);
        self.ivars().cur_x.set(x);
        self.ivars().cur_y.set(y);
        self.ivars().dragging.set(false);
    }

    fn on_drag(&self, event: &NSEvent) {
        // Dragging is only enabled when there's no search filter: while filtering, rows are a matched subset, so the drop position would be misaligned with the actual reordering.
        let draggable = matches!(self.ivars().press.get(), Press::Tab(..) | Press::Group(_))
            && self.ivars().query.borrow().is_empty();
        if draggable {
            let y = self.point_y(event);
            if (y - self.ivars().start_y.get()).abs() > 4.0 {
                self.ivars().dragging.set(true);
            }
            self.ivars().cur_y.set(y);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn on_up(&self, event: &NSEvent) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let press = self.ivars().press.get();
        if self.ivars().dragging.get() {
            match press {
                Press::Tab(id, _) => {
                    let snap = ctrl.snapshot();
                    if let Some((g, before)) = self.tab_drop_target(&snap, id) {
                        ctrl.move_tab_to(id, g, before);
                    }
                }
                Press::Group(gi) => {
                    // The new insertion index is how many group vertical midpoints the drop position crossed.
                    let snap = ctrl.snapshot();
                    let rows = Self::build_rows(&snap, "", self.scroll());
                    let heads: Vec<f64> = rows
                        .iter()
                        .filter_map(|r| match r.kind {
                            Press::Group(_) => Some(r.top),
                            _ => None,
                        })
                        .collect();
                    let list_bottom = rows.last().map(|r| r.top + r.h).unwrap_or(0.0);
                    let y = self.ivars().cur_y.get();
                    let mut target = 0usize;
                    for (i, top) in heads.iter().enumerate() {
                        let bottom = heads.get(i + 1).copied().unwrap_or(list_bottom);
                        if (top + bottom) / 2.0 < y {
                            target += 1;
                        }
                    }
                    ctrl.move_group(gi, target);
                }
                _ => {}
            }
        } else {
            match press {
                Press::Search => self.enter_search(),
                Press::NewTab => {
                    self.exit_search();
                    ctrl.add_tab_default();
                }
                Press::NewGroup => {
                    self.exit_search();
                    ctrl.add_group_default();
                }
                Press::Tab(id, _) => {
                    self.exit_search();
                    // Double-click → rename in place; single click → select.
                    if unsafe { event.clickCount() } >= 2 {
                        self.start_edit(Editing::Tab(id));
                    } else {
                        ctrl.select(id);
                    }
                }
                Press::NoteOpen(idx) => {
                    self.exit_search();
                    ctrl.select_open_note(idx);
                }
                Press::NoteRecent(idx) => {
                    self.exit_search();
                    ctrl.select_recent_note(idx);
                }
                Press::Group(gi) => {
                    // Single-click a group title → collapse/expand (rename etc. go through the "⋯" menu).
                    ctrl.toggle_group_collapsed(gi);
                }
                Press::GroupMenu(gi) => self.open_group_menu(gi),
                Press::TabMenu(id) => self.open_tab_menu(id),
                Press::TabDot(id) => self.open_dot_menu(id),
                Press::StyleMenu => {
                    if let Some(c) = self.controller() {
                        c.open_settings();
                    }
                }
                _ => {}
            }
        }
        self.ivars().press.set(Press::None);
        self.ivars().dragging.set(false);
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Draw the top search box: light box + magnifier + placeholder/query + ⌘F badge on the right; when focused, stroke it and draw the cursor.
    fn draw_search(&self, row: &Row, w: f64, query: &str, searching: bool) {
        let box_rect = rect(HPAD, row.top, w - 2.0 * HPAD, row.h);
        round_fill(box_rect, 7.0, &overlay(0.06));
        // Inner stroke (accent color when focused, otherwise very faint white).
        if searching {
            round_stroke(box_rect, 7.0, 1.0, &rgba(ACCENT_ICON.0, ACCENT_ICON.1, ACCENT_ICON.2, 0.7));
        } else {
            round_stroke(box_rect, 7.0, 1.0, &overlay(0.05));
        }
        // Magnifier SF icon on the left.
        draw_symbol("magnifyingglass", rect(HPAD + 9.0, row.top + (row.h - 13.0) / 2.0, 13.0, 13.0), text_placeholder());
        let text_x = HPAD + 27.0;
        let (text, color) = if query.is_empty() {
            ("Search…".to_string(), text_placeholder())
        } else {
            (query.to_string(), text_primary())
        };
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(color)));
        let ns = NSString::from_str(&text);
        // Scroll + clip so a long query never spills past the box (matches the rename box).
        let right_pad = 9.0;
        let avail = (w - HPAD - right_pad - text_x).max(0.0);
        let caret_w = self.caret_width(query);
        let offset = if searching { (caret_w - avail).max(0.0) } else { 0.0 };
        let clip = rect(text_x, row.top, avail + right_pad, row.h);
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(clip) };
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(text_x - offset, row.top + (row.h - 16.0) / 2.0), Some(&attrs)) };
        // Cursor: when focused, sits at the caret position within the query (hugs the left for an empty query).
        if searching {
            unsafe {
                ns_color(text_primary()).set();
                NSRectFill(rect(text_x + caret_w - offset + 1.0, row.top + 4.0, 1.0, row.h - 9.0));
            }
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }

        // ⌘F badge on the right (shown when not focused).
        if !searching {
            self.draw_badge("⌘F", w - HPAD - 8.0, row.top + row.h / 2.0);
        }
    }

    /// In-place rename edit box: a rounded box the same width as a normal row + vertically centered text + cursor.
    fn draw_edit(&self, row: &Row, w: f64) {
        let box_rect = rect(HPAD, row.top + 1.0, w - 2.0 * HPAD, row.h - 2.0);
        let text = self.ivars().edit_buf.borrow().clone();
        round_fill(box_rect, 7.0, &overlay(0.08));
        round_stroke(box_rect, 7.0, 1.0, &rgba(ACCENT_ICON.0, ACCENT_ICON.1, ACCENT_ICON.2, 0.7));
        let text_x = HPAD + 9.0;
        let right_pad = 9.0;
        let avail = (w - HPAD - right_pad - text_x).max(0.0); // visible text width inside the box
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(text_primary())));
        let ns = NSString::from_str(&text);
        // Scroll horizontally so the caret stays inside the box when the text is longer than the box.
        let caret_w = self.caret_width(&text);
        let offset = (caret_w - avail).max(0.0);
        // Clip the text to the box interior so long content never spills past the rounded border.
        let clip = rect(text_x, row.top, avail + right_pad, row.h);
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(clip) };
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(text_x - offset, row.top + (row.h - 16.0) / 2.0), Some(&attrs)) };
        // Cursor at the caret (shifted by the same scroll offset).
        unsafe {
            ns_color(text_primary()).set();
            NSRectFill(rect(text_x + caret_w - offset + 1.0, row.top + (row.h - 15.0) / 2.0, 1.0, 15.0));
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }
    }

    /// Glyph width of the substring before the caret, for cursor positioning (uses the sidebar font).
    fn caret_width(&self, text: &str) -> f64 {
        let upto = &text[..Self::char_byte(text, self.ivars().caret.get())];
        if upto.is_empty() {
            return 0.0;
        }
        let attrs = make_attrs(&self.ivars().font, None);
        let ns = NSString::from_str(upto);
        unsafe { ns.sizeWithAttributes(Some(&attrs)).width }
    }

    /// Build a menu item: title + action targeting this view + tag (holds the group index or tab id).
    fn menu_item(&self, title: &str, action: Sel, tag: isize) -> Retained<NSMenuItem> {
        let mtm = MainThreadMarker::new().expect("main thread");
        let empty = NSString::from_str("");
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), Some(action), &empty)
        };
        unsafe {
            item.setTag(tag);
            let _: () = msg_send![&item, setTarget: self];
        }
        item
    }

    /// Pop up a menu at the mouse position (the click point of "⋯" or a right-click).
    fn popup(&self, menu: &Retained<NSMenu>) {
        let loc = NSPoint::new(self.ivars().cur_x.get(), self.ivars().cur_y.get());
        unsafe { menu.popUpMenuPositioningItem_atLocation_inView(None, loc, Some(&**self)) };
    }

    /// Group "more" menu: rename / collapse-expand / delete.
    fn open_group_menu(&self, gi: usize) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let collapsed = self
            .controller()
            .map(|c| c.snapshot())
            .and_then(|s| s.groups.get(gi).map(|g| g.collapsed))
            .unwrap_or(false);
        let menu = NSMenu::new(mtm);
        menu.addItem(&self.menu_item("Rename", sel!(groupRename:), gi as isize));
        let toggle = if collapsed { "Expand" } else { "Collapse" };
        menu.addItem(&self.menu_item(toggle, sel!(groupToggle:), gi as isize));
        let sep = NSMenuItem::separatorItem(mtm);
        menu.addItem(&sep);
        menu.addItem(&self.menu_item("Delete Group", sel!(groupDelete:), gi as isize));
        self.popup(&menu);
    }

    /// Whether tab `id` is currently locked (looked up from the controller's snapshot).
    fn tab_locked(&self, id: u64) -> bool {
        self.controller()
            .map(|c| c.snapshot())
            .map(|s| {
                s.ungrouped
                    .iter()
                    .chain(s.groups.iter().flat_map(|g| g.tabs.iter()))
                    .find(|(tid, _, _, _)| *tid == id)
                    .map(|(_, _, _, locked)| *locked)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Tab "more" menu: rename / reveal / lock-unlock / close (Close is disabled while locked).
    fn open_tab_menu(&self, id: u64) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let locked = self.tab_locked(id);
        let menu = NSMenu::new(mtm);
        // Manage item enablement ourselves so a locked tab's Close renders greyed-out (AppKit's
        // auto-enable would re-enable it since the item has a valid target/action).
        unsafe { menu.setAutoenablesItems(false) };
        menu.addItem(&self.menu_item("Rename", sel!(tabRename:), id as isize));
        menu.addItem(&self.menu_item("Reveal in Finder", sel!(tabRevealInFinder:), id as isize));
        let sep = NSMenuItem::separatorItem(mtm);
        menu.addItem(&sep);
        menu.addItem(&self.menu_item(if locked { "Unlock" } else { "Lock" }, sel!(tabToggleLock:), id as isize));
        let close = self.menu_item("Close", sel!(tabClose:), id as isize);
        if locked {
            unsafe { close.setEnabled(false) };
        }
        menu.addItem(&close);
        self.popup(&menu);
    }

    /// Status-dot color picker: the classic colors + "Default", each with a color swatch and a
    /// checkmark on the tab's current color.
    fn open_dot_menu(&self, id: u64) {
        let mtm = MainThreadMarker::new().expect("main thread");
        self.ivars().dot_target.set(id);
        // Look up this tab's current color index to check the matching item.
        let cur = self
            .controller()
            .map(|c| c.snapshot())
            .map(|s| {
                s.ungrouped
                    .iter()
                    .chain(s.groups.iter().flat_map(|g| g.tabs.iter()))
                    .find(|(tid, _, _, _)| *tid == id)
                    .map(|(_, _, d, _)| *d)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let menu = NSMenu::new(mtm);
        for (i, (name, rgb)) in DOT_COLORS.iter().enumerate() {
            let item = self.menu_item(name, sel!(pickDotColor:), i as isize);
            if i != 0 {
                unsafe { item.setImage(Some(&Self::swatch(*rgb, mtm))) };
            }
            if i as u8 == cur {
                unsafe {
                    let _: () = msg_send![&item, setState: 1isize];
                }
            }
            menu.addItem(&item);
        }
        self.popup(&menu);
    }

    /// A small rounded color swatch image for a color-menu item.
    #[allow(deprecated)] // lockFocus/unlockFocus: fine for a tiny static swatch, avoids a block-based API
    fn swatch(rgb: (f64, f64, f64), mtm: MainThreadMarker) -> Retained<NSImage> {
        let img = unsafe { NSImage::initWithSize(mtm.alloc(), NSSize::new(12.0, 12.0)) };
        unsafe {
            img.lockFocus();
            ns_color(rgb).set();
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(1.0, 1.0, 10.0, 10.0), 5.0, 5.0);
            path.fill();
            img.unlockFocus();
        }
        img
    }

    fn enter_search(&self) {
        self.ivars().searching.set(true);
        self.ivars().caret.set(self.ivars().query.borrow().chars().count());
        if let Some(ctrl) = self.controller() {
            ctrl.focus_sidebar();
        }
    }

    /// Enter search state and redraw (⌘F triggered from the menu).
    pub fn begin_search(&self) {
        self.enter_search();
        unsafe { self.setNeedsDisplay(true) };
    }

    fn exit_search(&self) {
        self.ivars().searching.set(false);
        self.ivars().query.borrow_mut().clear();
        if let Some(ctrl) = self.controller() {
            ctrl.focus_terminal();
        }
    }

    /// Keyboard input: rename takes priority, then search. Both only trigger when the sidebar is focused.
    fn on_key(&self, event: &NSEvent) {
        let code = unsafe { event.keyCode() };
        // ---- Rename state ----
        if self.ivars().editing.get().is_some() {
            match code {
                53 => self.cancel_rename(),      // Esc
                36 | 76 => self.commit_rename(),  // Return / Enter
                123 => self.caret_left(),         // ←
                124 => self.caret_right(&self.ivars().edit_buf), // →
                115 => self.caret_home(),         // Home
                119 => self.caret_end(&self.ivars().edit_buf),   // End
                51 => self.caret_backspace(&self.ivars().edit_buf), // Backspace
                _ => self.caret_insert(&self.ivars().edit_buf, event),
            }
            unsafe { self.setNeedsDisplay(true) };
            return;
        }
        // ---- Search state ----
        if !self.ivars().searching.get() {
            return;
        }
        match code {
            53 => self.exit_search(),             // Esc
            36 | 76 => self.select_first_match(), // Return / Enter
            123 => self.caret_left(),             // ←
            124 => self.caret_right(&self.ivars().query), // →
            115 => self.caret_home(),             // Home
            119 => self.caret_end(&self.ivars().query), // End
            51 => self.caret_backspace(&self.ivars().query), // Backspace
            _ => self.caret_insert(&self.ivars().query, event),
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Byte offset of char index `i` into `s` (clamped to the string end).
    fn char_byte(s: &str, i: usize) -> usize {
        s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
    }

    fn caret_left(&self) {
        let c = self.ivars().caret.get();
        if c > 0 {
            self.ivars().caret.set(c - 1);
        }
    }

    fn caret_right(&self, buf: &RefCell<String>) {
        let n = buf.borrow().chars().count();
        let c = self.ivars().caret.get();
        if c < n {
            self.ivars().caret.set(c + 1);
        }
    }

    fn caret_home(&self) {
        self.ivars().caret.set(0);
    }

    fn caret_end(&self, buf: &RefCell<String>) {
        self.ivars().caret.set(buf.borrow().chars().count());
    }

    /// Delete the char before the caret.
    fn caret_backspace(&self, buf: &RefCell<String>) {
        let c = self.ivars().caret.get();
        if c == 0 {
            return;
        }
        let mut s = buf.borrow_mut();
        let start = Self::char_byte(&s, c - 1);
        let end = Self::char_byte(&s, c);
        s.replace_range(start..end, "");
        self.ivars().caret.set(c - 1);
    }

    /// Insert the event's typable characters at the caret.
    fn caret_insert(&self, buf: &RefCell<String>, event: &NSEvent) {
        let s = match unsafe { event.characters() } {
            Some(s) => s.to_string(),
            None => return,
        };
        for ch in s.chars() {
            if !is_typable(ch) {
                continue;
            }
            let c = self.ivars().caret.get();
            let mut b = buf.borrow_mut();
            let at = Self::char_byte(&b, c);
            b.insert(at, ch);
            drop(b);
            self.ivars().caret.set(c + 1);
        }
    }

    fn select_first_match(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        let query = self.ivars().query.borrow().clone();
        for row in Self::build_rows(&snap, &query, self.scroll()) {
            match row.kind {
                Press::Tab(id, _) => {
                    self.exit_search();
                    ctrl.select(id);
                    return;
                }
                Press::NoteOpen(idx) => {
                    self.exit_search();
                    ctrl.select_open_note(idx);
                    return;
                }
                Press::NoteRecent(idx) => {
                    self.exit_search();
                    ctrl.select_recent_note(idx);
                    return;
                }
                _ => {}
            }
        }
    }

    /// Start an in-place rename (tab or group): the buffer is initialized to the current name, and the sidebar takes keyboard focus.
    fn start_edit(&self, what: Editing) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        let init = match what {
            Editing::Tab(id) => snap
                .ungrouped
                .iter()
                .chain(snap.groups.iter().flat_map(|g| g.tabs.iter()))
                .find(|(tid, _, _, _)| *tid == id)
                .map(|(_, t, _, _)| t.clone())
                .unwrap_or_default(),
            Editing::Group(gi) => snap.groups.get(gi).map(|g| g.name.clone()).unwrap_or_default(),
        };
        self.ivars().searching.set(false);
        self.ivars().query.borrow_mut().clear();
        self.ivars().editing.set(Some(what));
        self.ivars().caret.set(init.chars().count()); // caret at end of the initial name
        *self.ivars().edit_buf.borrow_mut() = init;
        ctrl.focus_sidebar();
        unsafe { self.setNeedsDisplay(true) };
    }

    fn commit_rename(&self) {
        if let (Some(what), Some(ctrl)) = (self.ivars().editing.get(), self.controller()) {
            let name = self.ivars().edit_buf.borrow().trim().to_string();
            if !name.is_empty() {
                match what {
                    Editing::Tab(id) => ctrl.rename_tab(id, name),
                    Editing::Group(gi) => ctrl.rename_group(gi, name),
                }
            }
        }
        self.cancel_rename();
    }

    fn cancel_rename(&self) {
        self.ivars().editing.set(None);
        self.ivars().edit_buf.borrow_mut().clear();
        if let Some(ctrl) = self.controller() {
            ctrl.focus_terminal();
        }
        unsafe { self.setNeedsDisplay(true) };
    }
}

/// Whether a character is typable text: excludes control characters and AppKit
/// function-key private-use code points (U+E000..U+F8FF; arrow keys / Home / End /
/// PageUp / forward-delete land here and must not leak into search/rename text).
fn is_typable(ch: char) -> bool {
    !ch.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&ch)
}

/// sRGB color (with alpha).
fn rgba(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a) }
}

// ---- Accent color: amber #f0b15a, used only for the search/rename focus rings ----
const ACCENT_ICON: (f64, f64, f64) = (240.0 / 255.0, 177.0 / 255.0, 90.0 / 255.0);

/// Fill a rounded rectangle.
fn round_fill(r: NSRect, radius: f64, color: &NSColor) {
    unsafe {
        color.set();
        let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
        p.fill();
    }
}

/// Stroke a rounded rectangle.
fn round_stroke(r: NSRect, radius: f64, width: f64, color: &NSColor) {
    unsafe {
        color.set();
        let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
        p.setLineWidth(width);
        p.stroke();
    }
}
