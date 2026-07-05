//! Rendering view (step 2's dumb rendering + step 3's colored cell rendering).
//!
//! A custom `NSView` subclass:
//!   - uses a GCD dispatch source to pull bytes from the PTY master and feed them to `Grid`;
//!   - in `drawRect:`, draws in segments by cell attributes (foreground/background color, bold, underline, inverse):
//!     adjacent cells with identical attributes are merged into a single run and drawn at once with Core Text; background color and
//!     underline are filled with NSRectFill;
//!   - `keyDown:` writes keystrokes back to the PTY verbatim.
//! The window size is fixed; cols/rows are computed once at creation.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::unix::io::RawFd;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventModifierFlags, NSFont, NSFontAttributeName, NSFontWeightRegular,
    NSForegroundColorAttributeName, NSImage, NSImageSymbolConfiguration, NSImageSymbolScale,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSPasteboard,
    NSPasteboardTypeString, NSRectFill, NSStringDrawing, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSMutableDictionary, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use tabt_core::{Color, Grid, BOLD, UNDERLINE};

use crate::settings;
use crate::theme::{self, Rgb, Theme};

/// Text padding relative to the view's edges (logical points).
pub(crate) const PAD: f64 = 10.0;
/// Extra vertical space added to each row on top of the natural glyph height (line spacing).
/// Glyphs are centered within the taller row, so half the gap sits above and half below.
const LINE_GAP: f64 = 4.0;

/// Callback for session end (shell exit): (context, tab id). Uses a raw pointer + function pointer
/// rather than depending on the AppController type directly, to avoid a view ↔ app circular dependency.
pub type CloseFn = fn(*const c_void, u64);

/// Argument-less command callback (e.g. ⌘B to collapse the sidebar).
pub type CmdFn = fn(*const c_void);

pub struct TermViewIvars {
    grid: RefCell<Grid>,
    master_fd: RawFd,
    // Font/metrics come from the global crate::settings (changing the font takes effect immediately for all terminals).
    // Multi-tab support: this tab's id + exit callback. When there is no callback (default), it falls back to exiting the whole process.
    tab_id: Cell<u64>,
    close_ctx: Cell<*const c_void>,
    close_fn: Cell<Option<CloseFn>>,
    toggle_fn: Cell<Option<CmdFn>>, // ⌘B to collapse the sidebar
    closing: Cell<bool>,
    // Mouse selection: anchor + current drag point (cell coordinates (col,row)); equal = empty selection.
    sel_anchor: Cell<Option<(usize, usize)>>,
    sel_head: Cell<Option<(usize, usize)>>,
}

declare_class!(
    pub struct TermView;

    // SAFETY:
    // - The superclass NSView has no special subclassing constraints.
    // - NSView is itself MainThreadOnly; the subclass inherits this.
    // - TermView does not implement Drop.
    unsafe impl ClassType for TermView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "TermView";
    }

    impl DeclaredClass for TermView {
        type Ivars = TermViewIvars;
    }

    unsafe impl NSObjectProtocol for TermView {}

    unsafe impl TermView {
        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        // Origin at top-left, y increasing downward — row numbers map directly to pixels, avoiding coordinate flipping.
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        // Let the view become first responder so it can receive keyDown.
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            let fd = self.ivars().master_fd;
            // ⌘B: collapse/expand the sidebar (keyCode 11 = 'b').
            let cmd = unsafe { event.modifierFlags() }
                .contains(NSEventModifierFlags::NSEventModifierFlagCommand);
            if cmd && unsafe { event.keyCode() } == 11 {
                if let Some(f) = self.ivars().toggle_fn.get() {
                    f(self.ivars().close_ctx.get());
                }
                return;
            }
            // Special keys like arrow keys must send terminal escape sequences, not characters() (which gives private-use-area code points).
            if let Some(seq) = self.special_key_seq(unsafe { event.keyCode() }) {
                unsafe { write_all(fd, seq) };
                return;
            }
            // Any other ⌘-chord reaching here has no menu item bound to it (those are intercepted
            // by the menu system before keyDown: is ever called) and no meaning to a shell — swallow
            // it instead of forwarding the bare character (e.g. ⌘J would otherwise send a plain 'j').
            if cmd {
                return;
            }
            if let Some(s) = unsafe { event.characters() } {
                let bytes = s.to_string().into_bytes();
                if !bytes.is_empty() {
                    unsafe { write_all(fd, &bytes) };
                }
            }
        }

        // Window size change: first let the superclass update the frame, then re-lay-out the grid to the new size and notify the PTY.
        #[method(setFrameSize:)]
        fn set_frame_size(&self, size: NSSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.on_resize(size);
        }

        // ---- Mouse selection ----
        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            let c = self.cell_at(event);
            self.ivars().sel_anchor.set(Some(c));
            self.ivars().sel_head.set(Some(c));
            unsafe { self.setNeedsDisplay(true) };
        }

        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.ivars().sel_head.set(Some(self.cell_at(event)));
            unsafe { self.setNeedsDisplay(true) };
        }

        // ---- Edit menu actions (target=nil, travel the responder chain to reach the first responder) ----
        #[method(copy:)]
        fn copy_action(&self, _sender: Option<&AnyObject>) {
            let text = self.selected_text();
            if text.is_empty() {
                return;
            }
            unsafe {
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                pb.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
            }
        }

        #[method(paste:)]
        fn paste_action(&self, _sender: Option<&AnyObject>) {
            let s = unsafe {
                NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString)
            };
            if let Some(s) = s {
                let bytes = s.to_string().into_bytes();
                if !bytes.is_empty() {
                    let fd = self.ivars().master_fd;
                    // Bracketed paste (DECSET 2004): wrap the paste so the program can tell it
                    // apart from typed keystrokes, e.g. a shell won't try to execute each
                    // newline-terminated line of a multi-line paste immediately.
                    if self.ivars().grid.borrow().bracketed_paste() {
                        unsafe {
                            write_all(fd, b"\x1b[200~");
                            write_all(fd, &bytes);
                            write_all(fd, b"\x1b[201~");
                        }
                    } else {
                        unsafe { write_all(fd, &bytes) };
                    }
                }
            }
        }

        #[method(selectAll:)]
        fn select_all_action(&self, _sender: Option<&AnyObject>) {
            let grid = self.ivars().grid.borrow();
            let (cols, rows) = (grid.cols, grid.rows);
            drop(grid);
            self.ivars().sel_anchor.set(Some((0, 0)));
            self.ivars().sel_head.set(Some((cols - 1, rows - 1)));
            unsafe { self.setNeedsDisplay(true) };
        }
    }
);

impl TermView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        master_fd: RawFd,
        cols: usize,
        rows: usize,
    ) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(TermViewIvars {
            grid: RefCell::new(Grid::new(cols, rows)),
            master_fd,
            tab_id: Cell::new(0),
            close_ctx: Cell::new(std::ptr::null()),
            close_fn: Cell::new(None),
            toggle_fn: Cell::new(None),
            closing: Cell::new(false),
            sel_anchor: Cell::new(None),
            sel_head: Cell::new(None),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    /// Current working directory (reported via OSC 7, used for "Open in Finder" and session restore).
    pub fn cwd(&self) -> String {
        self.ivars().grid.borrow().cwd().to_string()
    }

    /// Re-lay-out the grid to new cols/rows (called by the controller after a font change).
    pub fn resize_grid(&self, cols: usize, rows: usize) {
        self.ivars().grid.borrow_mut().resize(cols, rows);
        self.clear_selection();
    }

    /// Clear the mouse selection. Must be called after the grid size changes: the old selection coordinates may be out of bounds, and if they linger they cause
    /// a subsequent copy (grid.cell(c,r) inside selected_text) to panic on an out-of-bounds access.
    fn clear_selection(&self) {
        self.ivars().sel_anchor.set(None);
        self.ivars().sel_head.set(None);
    }

    /// Clear screen (⌘K): directly feed ED(2) + cursor home to the Grid.
    pub fn clear(&self) {
        self.ivars().grid.borrow_mut().feed(b"\x1b[2J\x1b[H");
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Bind the owning tab id, exit callback, and ⌘B collapse callback (called by AppController after creation).
    pub fn attach(&self, ctx: *const c_void, tab_id: u64, close: CloseFn, toggle: CmdFn) {
        self.ivars().tab_id.set(tab_id);
        self.ivars().close_ctx.set(ctx);
        self.ivars().close_fn.set(Some(close));
        self.ivars().toggle_fn.set(Some(toggle));
    }

    /// shell exit: defer the close action to the next runloop tick — we must never release
    /// ourselves (the last Retained) directly inside on_readable (a method on self).
    fn schedule_close(&self) {
        if self.ivars().closing.get() {
            return;
        }
        self.ivars().closing.set(true);
        match self.ivars().close_fn.get() {
            None => std::process::exit(0), // No controller (single-terminal case): legacy behavior
            Some(f) => {
                let ctx = self.ivars().close_ctx.get();
                let id = self.ivars().tab_id.get();
                let boxed = Box::into_raw(Box::new((ctx, id, f)));
                unsafe {
                    let q: dispatch::Queue = &dispatch::_dispatch_main_q as *const _ as *mut _;
                    dispatch::dispatch_async_f(q, boxed as *mut c_void, close_trampoline);
                }
            }
        }
    }

    /// Map a macOS virtual keycode to a terminal escape sequence. Returning None means it's not a special key,
    /// handing it back to `characters()` for the normal character path. Arrow keys and Home/End are affected by DECCKM:
    /// in application cursor keys mode send `ESC O x`, otherwise send `ESC [ x`.
    fn special_key_seq(&self, keycode: u16) -> Option<&'static [u8]> {
        let app = self.ivars().grid.borrow().app_cursor_keys();
        let seq: &'static [u8] = match keycode {
            126 => if app { b"\x1bOA" } else { b"\x1b[A" }, // ↑
            125 => if app { b"\x1bOB" } else { b"\x1b[B" }, // ↓
            124 => if app { b"\x1bOC" } else { b"\x1b[C" }, // →
            123 => if app { b"\x1bOD" } else { b"\x1b[D" }, // ←
            115 => if app { b"\x1bOH" } else { b"\x1b[H" }, // Home
            119 => if app { b"\x1bOF" } else { b"\x1b[F" }, // End
            116 => b"\x1b[5~",                              // PageUp
            121 => b"\x1b[6~",                              // PageDown
            117 => b"\x1b[3~",                              // Forward Delete
            _ => return None,
        };
        Some(seq)
    }

    /// GCD dispatch source fires: drain all readable data from the master, feed it to the Grid, then request a redraw.
    fn on_readable(&self) {
        if self.ivars().closing.get() {
            return; // Already closing: EOF fires repeatedly, just ignore it
        }
        let fd = self.ivars().master_fd;
        let mut buf = [0u8; 8192];
        let mut dirty = false;
        loop {
            let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                let replies = {
                    let mut grid = self.ivars().grid.borrow_mut();
                    grid.feed(&buf[..r as usize]);
                    grid.take_replies()
                };
                // DSR/DA replies (cursor-position/device-attribute queries) go back to the PTY
                // exactly like a keystroke would — some programs block waiting for one.
                if !replies.is_empty() {
                    unsafe { write_all(fd, &replies) };
                }
                dirty = true;
            } else if r == 0 {
                self.schedule_close(); // shell exit (EOF)
                break;
            } else {
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => break,  // Drained
                    _ => {
                        self.schedule_close(); // EIO etc.: the shell is gone
                        break;
                    }
                }
            }
        }
        if dirty {
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    /// The view size changed: compute new cols/rows from the cell metrics, re-lay-out the grid, and use TIOCSWINSZ
    /// to tell the PTY the new size (the shell receives SIGWINCH and redraws).
    fn on_resize(&self, size: NSSize) {
        let ivars = self.ivars();
        let cols = (((size.width - 2.0 * PAD) / settings::cell_w()).floor() as i64).max(1) as usize;
        let rows = (((size.height - 2.0 * PAD) / settings::line_h()).floor() as i64).max(1) as usize;

        {
            let mut grid = ivars.grid.borrow_mut();
            if cols == grid.cols && rows == grid.rows {
                return;
            }
            grid.resize(cols, rows);
        }
        self.clear_selection(); // Old selection coordinates may be out of bounds

        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(ivars.master_fd, libc::TIOCSWINSZ, &ws);
            self.setNeedsDisplay(true);
        }
    }

    /// Mouse point → cell coordinates (col,row).
    fn cell_at(&self, event: &NSEvent) -> (usize, usize) {
        let p = unsafe { event.locationInWindow() };
        let lp = self.convertPoint_fromView(p, None);
        let ivars = self.ivars();
        let grid = ivars.grid.borrow();
        let (cols, rows) = (grid.cols as i64, grid.rows as i64);
        let col = (((lp.x - PAD) / settings::cell_w()).floor() as i64).clamp(0, cols - 1);
        let row = (((lp.y - PAD) / settings::line_h()).floor() as i64).clamp(0, rows - 1);
        (col as usize, row as usize)
    }

    /// The normalized selection ((start, end), inclusive of both ends); returns None for an empty selection.
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.ivars().sel_anchor.get()?;
        let h = self.ivars().sel_head.get()?;
        if a == h {
            return None;
        }
        Some(if (a.1, a.0) <= (h.1, h.0) { (a, h) } else { (h, a) })
    }

    /// Selection text (strip trailing whitespace per line, join lines with newlines).
    fn selected_text(&self) -> String {
        let ((sc, sr), (ec, er)) = match self.selection_range() {
            Some(r) => r,
            None => return String::new(),
        };
        let grid = self.ivars().grid.borrow();
        let cols = grid.cols;
        let mut out = String::new();
        for r in sr..=er {
            let c0 = if r == sr { sc } else { 0 };
            let c1 = if r == er { ec } else { cols - 1 };
            let mut line = String::new();
            for c in c0..=c1 {
                line.push(grid.cell(c, r).ch);
            }
            out.push_str(line.trim_end());
            if r != er {
                out.push('\n');
            }
        }
        out
    }

    fn render(&self) {
        let ivars = self.ivars();
        let (cw, lh) = (settings::cell_w(), settings::line_h());
        let font = settings::font();
        let font_bold = settings::font_bold();
        // Fetch the theme once per frame instead of once (or twice) per cell inside eff() —
        // Theme is a ~90-byte Copy struct, so a per-cell fetch adds up over a full grid.
        let t = theme::current();
        let default_bg = t.bg;

        // Whole-window background (uses the current theme's background color).
        unsafe {
            ns_color(default_bg).set();
            NSRectFill(self.bounds());
        }

        let grid = ivars.grid.borrow();
        let (cols, rows) = (grid.cols, grid.rows);

        // Selection highlight: drawn as a background BEFORE the glyphs so the text stays fully
        // opaque (and readable) on top, rather than being dimmed by an overlay.
        if let Some(((sc, sr), (ec, er))) = self.selection_range() {
            unsafe {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.30, 0.45, 0.75, 0.45).set();
            }
            for r in sr..=er {
                let c0 = if r == sr { sc } else { 0 };
                let c1 = if r == er { ec } else { cols - 1 };
                let x = PAD + c0 as f64 * cw;
                let width = (c1 - c0 + 1) as f64 * cw;
                unsafe { NSRectFill(rect(x, PAD + r as f64 * lh, width, lh)) };
            }
        }

        for r in 0..rows {
            let y = PAD + r as f64 * lh;
            let mut c = 0;
            while c < cols {
                // Merge adjacent cells starting at c with identical attributes into a single run.
                let start = c;
                let a0 = eff(grid.cell(start, r), &t);
                c += 1;
                while c < cols && eff(grid.cell(c, r), &t) == a0 {
                    c += 1;
                }
                let (fg, bg, flags) = a0;
                let run_x = PAD + start as f64 * cw;
                let run_w = (c - start) as f64 * cw;

                // Background: only fill non-default backgrounds (the default background is already covered by the whole-window background).
                if bg != default_bg {
                    unsafe {
                        ns_color(bg).set();
                        NSRectFill(rect(run_x, y, run_w, lh));
                    }
                }

                // Text: a run of pure whitespace need not draw glyphs.
                let text: String = (start..c).map(|i| grid.cell(i, r).ch).collect();
                if !text.trim().is_empty() {
                    let font = if flags & BOLD != 0 { &font_bold } else { &font };
                    let color = ns_color(fg);
                    let attrs = make_attrs(font, Some(&color));
                    let ns = NSString::from_str(&text);
                    unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(run_x, y + LINE_GAP / 2.0), Some(&attrs)) };
                }

                // Underline: fill a 1pt foreground-color line at the bottom of the run (avoids the NSNumber attribute).
                if flags & UNDERLINE != 0 {
                    unsafe {
                        ns_color(fg).set();
                        NSRectFill(rect(run_x, y + lh - 1.0, run_w, 1.0));
                    }
                }
            }
        }


        // Cursor: an inverse-video block (fill with the cursor cell's foreground color, then redraw the character in its background color).
        if grid.cursor_visible() {
            let (cc, cr) = grid.cursor;
            if cc < cols && cr < rows {
                let x = PAD + cc as f64 * cw;
                let y = PAD + cr as f64 * lh;
                let cell = grid.cell(cc, cr);
                let (fg, bg, _) = eff(cell, &t);
                unsafe {
                    ns_color(fg).set();
                    NSRectFill(rect(x, y, cw, lh));
                }
                if cell.ch != ' ' {
                    let color = ns_color(bg);
                    let attrs = make_attrs(&font, Some(&color));
                    let ns = NSString::from_str(&cell.ch.to_string());
                    unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(x, y + LINE_GAP / 2.0), Some(&attrs)) };
                }
            }
        }
    }
}

/// Compute a cell's effective colors: apply the given theme, resolve the palette, and apply
/// inverse video. `t` is fetched once per frame by the caller, not once per cell.
fn eff(cell: &tabt_core::Cell, t: &Theme) -> (Rgb, Rgb, u8) {
    let bold = cell.flags & BOLD != 0;

    let (mut fg, mut bg) = if t.mono {
        // Monochrome phosphor screen: ignore SGR colors, always use the phosphor color; bold is slightly brightened.
        let base = if bold { brighten(t.fg) } else { t.fg };
        (base, t.bg)
    } else {
        (resolve(cell.fg, t.fg, bold, t), resolve(cell.bg, t.bg, false, t))
    };

    if cell.flags & tabt_core::INVERSE != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg, cell.flags)
}

/// Brighten by one step (how bold is rendered under a monochrome theme).
fn brighten((r, g, b): Rgb) -> Rgb {
    ((r * 1.3).min(1.0), (g * 1.3).min(1.0), (b * 1.3).min(1.0))
}

/// Color → normalized RGB. Bold promotes dark colors 0-7 to bright colors 8-15 (classic terminal behavior).
fn resolve(color: Color, default: Rgb, bold: bool, t: &Theme) -> Rgb {
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0),
        Color::Indexed(n) => {
            let n = if bold && n < 8 { n + 8 } else { n };
            let (r, g, b) = palette(n, t);
            (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
        }
    }
}

/// xterm 256-color palette: 0-15 use the theme's 16 colors, 16-231 are the 6×6×6 color cube, 232-255 are grayscale.
fn palette(n: u8, t: &Theme) -> (u8, u8, u8) {
    match n {
        0..=15 => t.palette[n as usize],
        16..=231 => {
            let i = n - 16;
            let step = |v: u8| if v == 0 { 0u8 } else { 55 + 40 * v };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        _ => {
            let v = 8 + (n - 232) * 10;
            (v, v, v)
        }
    }
}

pub(crate) fn ns_color(rgb: Rgb) -> Retained<NSColor> {
    unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(rgb.0, rgb.1, rgb.2, 1.0) }
}

pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Draw an SF Symbol icon inside `rect` (colored hierarchically by `color`). Silently skipped when the symbol is missing.
/// Uniformly sets point size from the rect height + Regular weight + Medium scale, making the icon as
/// crisp and consistent as a system control (rendered the same way as the title bar's sidebar.left).
pub(crate) fn draw_symbol(name: &str, rect: NSRect, color: Rgb) {
    unsafe {
        let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(name),
            None,
        );
        if let Some(img) = img {
            let color_cfg =
                NSImageSymbolConfiguration::configurationWithHierarchicalColor(&ns_color(color));
            let size_cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight_scale(
                rect.size.height * 0.92,
                NSFontWeightRegular,
                NSImageSymbolScale::Medium,
            );
            // Merge the weight/size and coloring configurations.
            let cfg = size_cfg.configurationByApplyingConfiguration(&color_cfg);
            let colored = img.imageWithSymbolConfiguration(&cfg).unwrap_or(img);
            // Draw centered at the symbol's own size to avoid drawInRect stretching (e.g. "⋯" squashed into a vertical ellipse).
            let sz = colored.size();
            let dst = NSRect::new(
                NSPoint::new(
                    rect.origin.x + (rect.size.width - sz.width) / 2.0,
                    rect.origin.y + (rect.size.height - sz.height) / 2.0,
                ),
                sz,
            );
            colored.drawInRect(dst);
        }
    }
}

/// Measure the width/height of a single monospace-font character, used to convert the window content size into cols/rows.
pub fn cell_metrics(font: &Retained<NSFont>) -> (f64, f64) {
    let attrs = make_attrs(font, None);
    let sz = unsafe { NSString::from_str("M").sizeWithAttributes(Some(&attrs)) };
    (sz.width, sz.height + LINE_GAP)
}

/// Build an NSString drawing-attributes dictionary: font required, foreground color optional.
/// AnyObject does not satisfy `from_slice`'s IsRetainable constraint, so use the raw setObject:forKey:.
pub(crate) fn make_attrs(
    font: &Retained<NSFont>,
    fg: Option<&Retained<NSColor>>,
) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
    let mut dict = NSMutableDictionary::<NSString, AnyObject>::new();
    let font_any = unsafe { Retained::cast::<AnyObject>(font.clone()) };
    unsafe {
        dict.setObject_forKey(&font_any, ProtocolObject::from_ref(NSFontAttributeName));
    }
    if let Some(fg) = fg {
        let fg_any = unsafe { Retained::cast::<AnyObject>(fg.clone()) };
        unsafe {
            dict.setObject_forKey(&fg_any, ProtocolObject::from_ref(NSForegroundColorAttributeName));
        }
    }
    dict
}

/// Draw a single line of text truncated with a tail ellipsis (`…`) to fit `rect`'s width.
/// Used for tab/group names so long labels never overflow their row.
pub(crate) fn draw_truncated(text: &str, rect: NSRect, font: &Retained<NSFont>, fg: Rgb) {
    let mut dict = make_attrs(font, Some(&ns_color(fg)));
    unsafe {
        let style = NSMutableParagraphStyle::new();
        style.setLineBreakMode(NSLineBreakMode::NSLineBreakByTruncatingTail);
        let style_any = Retained::cast::<AnyObject>(style);
        dict.setObject_forKey(&style_any, ProtocolObject::from_ref(NSParagraphStyleAttributeName));
        NSString::from_str(text).drawInRect_withAttributes(rect, Some(&dict));
    }
}

/// Reader handle: holds the dispatch source, used to cancel it when the tab closes, to prevent the reader from continuing to fire
/// on a closed fd / released view.
pub struct ReaderToken(dispatch::Source);

/// Attach a reader on the main queue: calls back `on_readable` when the PTY master is readable.
///
/// `view`'s lifetime is managed by AppController (stored in its model) and covers the reader's lifespan,
/// so using a raw pointer as the dispatch context is safe. Returns a handle for cancellation on close.
pub fn attach_reader(view: &TermView) -> ReaderToken {
    extern "C" fn handler(ctx: *mut c_void) {
        let view: &TermView = unsafe { &*(ctx as *const TermView) };
        view.on_readable();
    }
    let fd = view.ivars().master_fd;
    unsafe {
        let queue: dispatch::Queue = &dispatch::_dispatch_main_q as *const _ as *mut _;
        let ty: dispatch::SourceType = &dispatch::_dispatch_source_type_read;
        let src = dispatch::dispatch_source_create(ty, fd as usize, 0, queue);
        dispatch::dispatch_set_context(src, view as *const TermView as *mut c_void);
        dispatch::dispatch_source_set_event_handler_f(src, handler);
        dispatch::dispatch_resume(src);
        ReaderToken(src)
    }
}

/// Cancel the reader (called when the tab closes).
pub fn cancel_reader(t: &ReaderToken) {
    unsafe { dispatch::dispatch_source_cancel(t.0) };
}

/// Landing point for schedule_close's deferred execution: calls back CloseFn on a main-queue tick.
extern "C" fn close_trampoline(p: *mut c_void) {
    let b = unsafe { Box::from_raw(p as *mut (*const c_void, u64, CloseFn)) };
    let (ctx, id, f) = *b;
    f(ctx, id);
}

unsafe fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let w = libc::write(fd, data.as_ptr().cast(), data.len());
        if w <= 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // The master is non-blocking: when the TTY input buffer is full it returns EAGAIN. Wait until it's writable, then continue,
                // otherwise a large paste would be silently truncated.
                Some(libc::EAGAIN) => {
                    let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                    // Wait at most 2s; give up on timeout/error to avoid hanging the main thread.
                    if libc::poll(&mut pfd, 1, 2000) <= 0 {
                        return;
                    }
                    continue;
                }
                _ => return,
            }
        }
        data = &data[w as usize..];
    }
}

/// Minimal libdispatch bindings: declare only the few symbols needed to attach a READ dispatch source.
/// Follows this project's "use raw FFI rather than add a dependency" style.
pub(crate) mod dispatch {
    use std::ffi::c_void;

    /// Opaque dispatch object; we only take its address, never read it by value.
    #[repr(C)]
    pub struct Object {
        _private: [u8; 0],
    }
    pub type Source = *mut Object;
    pub type Queue = *mut Object;
    pub type SourceType = *const Object;
    pub type FunctionT = extern "C" fn(*mut c_void);

    extern "C" {
        pub static _dispatch_source_type_read: Object;
        pub static _dispatch_main_q: Object;
        pub fn dispatch_source_create(
            type_: SourceType,
            handle: usize,
            mask: usize,
            queue: Queue,
        ) -> Source;
        pub fn dispatch_set_context(object: Source, context: *mut c_void);
        pub fn dispatch_source_set_event_handler_f(source: Source, handler: FunctionT);
        pub fn dispatch_resume(object: Source);
        pub fn dispatch_source_cancel(source: Source);
        pub fn dispatch_async_f(queue: Queue, context: *mut c_void, work: FunctionT);
    }
}
