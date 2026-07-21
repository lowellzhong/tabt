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
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventModifierFlags, NSFont, NSFontAttributeName, NSFontWeightRegular,
    NSForegroundColorAttributeName, NSImage, NSImageSymbolConfiguration, NSImageSymbolScale,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSPasteboard,
    NSPasteboardTypeString, NSRectFill, NSStringDrawing, NSTextInputClient, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSAttributedStringKey, NSInteger,
    NSMutableDictionary, NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSSize,
    NSString, NSUInteger,
};

use tabt_core::{char_width, Color, Grid, BOLD, UNDERLINE, WIDE_TRAILER};

use crate::settings;
use crate::theme::{self, Rgb, Theme};

/// Text padding relative to the view's edges (logical points).
pub(crate) const PAD: f64 = 10.0;
/// Extra vertical space added to each row on top of the natural glyph height (line spacing).
/// Glyphs are centered within the taller row, so half the gap sits above and half below.
/// Zero means the standard/default line height (the font's own natural glyph height).
const LINE_GAP: f64 = 0.0;

/// Callback for the shell exiting on its own (EOF/read error): (context, tab id). The tab and its
/// view are kept alive — TermView shows a "session ended" placeholder (see `mark_ended`) until the
/// user restarts it (`RestartFn`) or closes the tab explicitly. Uses a raw pointer + function
/// pointer rather than depending on the AppController type directly, to avoid a view ↔ app circular
/// dependency.
pub type EndFn = fn(*const c_void, u64);

/// Callback to respawn a fresh shell into an ended tab (context, tab id), fired when the user
/// presses Enter while the tab shows its "session ended" state (see `mark_ended`/`restart`).
pub type RestartFn = fn(*const c_void, u64);

/// Argument-less command callback (e.g. ⌘B to collapse the sidebar).
pub type CmdFn = fn(*const c_void);

pub struct TermViewIvars {
    grid: RefCell<Grid>,
    // Mutable: `restart()` swaps in a fresh fd after the previous shell has ended.
    master_fd: Cell<RawFd>,
    // Font/metrics come from the global crate::settings (changing the font takes effect immediately for all terminals).
    // Multi-tab support: this tab's id + callbacks. When there is no callback (default), end falls back to exiting the whole process.
    tab_id: Cell<u64>,
    close_ctx: Cell<*const c_void>,
    end_fn: Cell<Option<EndFn>>,
    restart_fn: Cell<Option<RestartFn>>,
    toggle_fn: Cell<Option<CmdFn>>, // ⌘B to collapse the sidebar
    // The shell exited (EOF/read error) and hasn't been restarted yet: input is ignored except
    // Enter, which triggers `restart_fn`. See `mark_ended`/`restart`.
    ended: Cell<bool>,
    // Mouse selection: anchor + current drag point (cell coordinates (col,row)); equal = empty selection.
    sel_anchor: Cell<Option<(usize, usize)>>,
    sel_head: Cell<Option<(usize, usize)>>,
    // IME composition (preedit): the in-progress marked text drawn inline at the cursor.
    // Empty when not composing. Set by setMarkedText:, cleared by insertText:/unmarkText.
    marked_text: RefCell<String>,
    // Leftover trackpad scroll distance (pixels) below one line's worth, carried to the next event
    // so a slow continuous swipe still advances instead of rounding to zero forever.
    scroll_accum: Cell<f64>,
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
            // Session already ended: there is no shell to type into. Only Enter/Return means
            // anything here — it restarts a fresh shell in place; every other key is a no-op.
            if self.ivars().ended.get() {
                let keycode = unsafe { event.keyCode() };
                if keycode == 36 || keycode == 76 {
                    if let Some(f) = self.ivars().restart_fn.get() {
                        f(self.ivars().close_ctx.get(), self.ivars().tab_id.get());
                    }
                }
                return;
            }

            let fd = self.ivars().master_fd.get();
            let flags = unsafe { event.modifierFlags() };
            let cmd = flags.contains(NSEventModifierFlags::NSEventModifierFlagCommand);
            let ctrl = flags.contains(NSEventModifierFlags::NSEventModifierFlagControl);

            // ⌘B: collapse/expand the sidebar (keyCode 11 = 'b'). Any other ⌘-chord reaching here
            // has no menu item bound to it (bound ones are intercepted by the menu system before
            // keyDown:) and no meaning to a shell — swallow it rather than forward the bare character.
            if cmd {
                if unsafe { event.keyCode() } == 11 {
                    if let Some(f) = self.ivars().toggle_fn.get() {
                        f(self.ivars().close_ctx.get());
                    }
                }
                return;
            }

            // Typing snaps the viewport back to the live bottom: the keystroke goes to the shell,
            // and its echo/response would otherwise appear off-screen below the scrolled-back view.
            // Bound to a local so the RefMut is released before key_seq() borrows the grid again.
            let snapped = self.ivars().grid.borrow_mut().scroll_to_bottom();
            if snapped {
                unsafe { self.setNeedsDisplay(true) };
            }

            // While an IME composition is active, every key belongs to the input system (to select a
            // candidate, extend/cancel the composition, etc.) — route it there unconditionally below.
            let composing = !self.ivars().marked_text.borrow().is_empty();

            if !composing {
                // Special keys (arrows, Tab/⇧Tab, Return, Delete, Esc, function keys) must send precise
                // terminal byte sequences rather than characters() (which yields private-use code points).
                if let Some(seq) = self.key_seq(event) {
                    unsafe { write_all(fd, seq) };
                    return;
                }
                // Control chords: handle BEFORE the input system, otherwise AppKit's emacs-style default
                // key bindings would hijack them (Ctrl+A → move-to-line-start, etc.). macOS already folds
                // Ctrl+letter into the control byte in characters() (Ctrl+C → 0x03, Ctrl+[ → 0x1b, …).
                if ctrl {
                    if let Some(s) = unsafe { event.characters() } {
                        let bytes = s.to_string().into_bytes();
                        if !bytes.is_empty() {
                            unsafe { write_all(fd, &bytes) };
                        }
                    }
                    return;
                }
            }

            // Plain text and IME composition flow through the macOS text input system: committed text
            // arrives via insertText:, in-progress composition via setMarkedText:. Option/Alt intentionally
            // falls through here (matches Terminal.app's default: dead keys / é / …), so "Option as Meta"
            // ESC-prefixing is deliberately not emitted.
            let array = NSArray::from_slice(&[event]);
            unsafe { self.interpretKeyEvents(&array) };
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

        // ---- Scrollback ----
        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            // A trackpad reports precise per-pixel deltas; a mouse wheel reports whole lines.
            // Positive deltaY means the content moves down, revealing older output above — which is
            // exactly the direction `scroll_view` calls positive.
            let dy = unsafe { event.scrollingDeltaY() };
            let lines = if unsafe { event.hasPreciseScrollingDeltas() } {
                let acc = self.ivars().scroll_accum.get() + dy;
                let lines = (acc / settings::line_h()).trunc();
                self.ivars().scroll_accum.set(acc - lines * settings::line_h());
                lines as isize
            } else {
                self.ivars().scroll_accum.set(0.0);
                dy.round() as isize
            };
            if lines != 0 && self.ivars().grid.borrow_mut().scroll_view(lines) {
                unsafe { self.setNeedsDisplay(true) };
            }
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
                    let fd = self.ivars().master_fd.get();
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

    // The text input system drives IME composition through these callbacks. They are invoked
    // *synchronously and re-entrantly* from inside interpretKeyEvents: (see key_down), so every
    // method must take the shortest possible RefCell borrow — holding one across an AppKit call
    // would abort the process (panic="abort", no unwind) on the re-entrant borrow.
    unsafe impl NSTextInputClient for TermView {
        // Commit text: a finished IME composition, or a plain typed character. Write it to the PTY
        // exactly as a keystroke and clear any composition state.
        #[method(insertText:replacementRange:)]
        fn insert_text(&self, string: &AnyObject, _replacement: NSRange) {
            let text = ns_input_string(string);
            self.ivars().marked_text.borrow_mut().clear();
            if !text.is_empty() {
                unsafe { write_all(self.ivars().master_fd.get(), text.as_bytes()) };
            }
            unsafe { self.setNeedsDisplay(true) };
        }

        // Composition in progress: stash the marked (preedit) text so render() draws it inline and
        // hasMarkedText/markedRange report it. selected_range/replacement are unused (single-line preedit).
        #[method(setMarkedText:selectedRange:replacementRange:)]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            *self.ivars().marked_text.borrow_mut() = ns_input_string(string);
            unsafe { self.setNeedsDisplay(true) };
        }

        #[method(unmarkText)]
        fn unmark_text(&self) {
            self.ivars().marked_text.borrow_mut().clear();
            unsafe { self.setNeedsDisplay(true) };
        }

        #[method(hasMarkedText)]
        fn has_marked_text(&self) -> bool {
            !self.ivars().marked_text.borrow().is_empty()
        }

        #[method(markedRange)]
        fn marked_range(&self) -> NSRange {
            let n = self.ivars().marked_text.borrow().chars().count();
            if n == 0 {
                NSRange::new(NS_NOT_FOUND, 0)
            } else {
                NSRange::new(0, n)
            }
        }

        #[method(selectedRange)]
        fn selected_range(&self) -> NSRange {
            // Caret at the end of the preedit; no live selection to report otherwise.
            let n = self.ivars().marked_text.borrow().chars().count();
            if n == 0 {
                NSRange::new(NS_NOT_FOUND, 0)
            } else {
                NSRange::new(n, 0)
            }
        }

        // We don't back the composition with a document, so there is no substring to hand back.
        #[method_id(attributedSubstringForProposedRange:actualRange:)]
        fn attributed_substring(
            &self,
            _range: NSRange,
            _actual: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[method_id(validAttributesForMarkedText)]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        // Where the IME should anchor its candidate window: the cursor cell, in screen coordinates.
        #[method(firstRectForCharacterRange:actualRange:)]
        fn first_rect(&self, range: NSRange, actual: NSRangePointer) -> NSRect {
            if !actual.is_null() {
                unsafe { *actual = range };
            }
            let (cw, lh) = (settings::cell_w(), settings::line_h());
            let (cc, cr) = self.ivars().grid.borrow().cursor; // (usize, usize) is Copy: borrow drops here
            let cell = rect(PAD + cc as f64 * cw, PAD + cr as f64 * lh, cw, lh);
            match self.window() {
                Some(win) => {
                    let in_window = self.convertRect_toView(cell, None);
                    win.convertRectToScreen(in_window)
                }
                None => cell,
            }
        }

        #[method(characterIndexForPoint:)]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            NS_NOT_FOUND
        }

        // Reached for keys the input system maps to editing commands (only while composing, since
        // key_down handles control/special keys directly otherwise). No-op — and crucially, *not*
        // forwarding to super suppresses AppKit's system beep for unhandled commands.
        #[method(doCommandBySelector:)]
        fn do_command_by_selector(&self, _selector: Sel) {}
    }
);

/// NSNotFound (== NSIntegerMax) as an unsigned index, used to report "no range/index".
const NS_NOT_FOUND: NSUInteger = NSInteger::MAX as NSUInteger;

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
            master_fd: Cell::new(master_fd),
            tab_id: Cell::new(0),
            close_ctx: Cell::new(std::ptr::null()),
            end_fn: Cell::new(None),
            restart_fn: Cell::new(None),
            toggle_fn: Cell::new(None),
            ended: Cell::new(false),
            scroll_accum: Cell::new(0.0),
            sel_anchor: Cell::new(None),
            sel_head: Cell::new(None),
            marked_text: RefCell::new(String::new()),
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
        let mut grid = self.ivars().grid.borrow_mut();
        grid.feed(b"\x1b[2J\x1b[H");
        grid.scroll_to_bottom(); // a cleared screen has nothing to read back to
        drop(grid);
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Bind the owning tab id, end/restart callbacks, and ⌘B collapse callback (called by AppController after creation).
    pub fn attach(&self, ctx: *const c_void, tab_id: u64, end: EndFn, restart: RestartFn, toggle: CmdFn) {
        self.ivars().tab_id.set(tab_id);
        self.ivars().close_ctx.set(ctx);
        self.ivars().end_fn.set(Some(end));
        self.ivars().restart_fn.set(Some(restart));
        self.ivars().toggle_fn.set(Some(toggle));
    }

    /// The shell exited (EOF or a fatal read error): print a status line into the grid, flip into
    /// the "ended" state (ignores all input but Enter until restarted), and notify the controller
    /// so it can cancel the now-dead reader/fd. The tab and view stay mounted — `restart` revives
    /// them in place, so unlike the old close-on-exit behavior nothing here tears down `self`.
    fn mark_ended(&self) {
        if self.ivars().ended.get() {
            return;
        }
        self.ivars().ended.set(true);
        {
            let mut grid = self.ivars().grid.borrow_mut();
            grid.feed(b"\r\n\x1b[33m[Session ended -- press Enter to restart]\x1b[0m");
            grid.scroll_to_bottom();
        }
        unsafe { self.setNeedsDisplay(true) };
        match self.ivars().end_fn.get() {
            None => std::process::exit(0), // No controller (single-terminal case): legacy behavior
            Some(f) => f(self.ivars().close_ctx.get(), self.ivars().tab_id.get()),
        }
    }

    /// Respawn a fresh shell into this (already-ended) tab in place: swap in the new master fd,
    /// reset the grid to a clean slate, and clear the "ended" state. Called by AppController once
    /// it has spawned the replacement PTY (see `RestartFn`).
    pub fn restart(&self, fd: RawFd, cols: usize, rows: usize) {
        self.ivars().master_fd.set(fd);
        *self.ivars().grid.borrow_mut() = Grid::new(cols, rows);
        self.clear_selection();
        self.ivars().ended.set(false);
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Map a key event to a terminal byte sequence. Returning None means it is not a special key,
    /// handing it to the text input system for the normal text path. Arrow keys and Home/End are
    /// affected by DECCKM: in application cursor keys mode send `ESC O x`, otherwise `ESC [ x`.
    fn key_seq(&self, event: &NSEvent) -> Option<&'static [u8]> {
        let keycode = unsafe { event.keyCode() };
        let shift = unsafe { event.modifierFlags() }
            .contains(NSEventModifierFlags::NSEventModifierFlagShift);
        let app = self.ivars().grid.borrow().app_cursor_keys(); // bool is Copy: borrow drops here
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
            48 => if shift { b"\x1b[Z" } else { b"\t" },    // Tab / ⇧Tab (CBT back-tab)
            36 | 76 => b"\r",                               // Return / keypad Enter
            51 => b"\x7f",                                  // Delete (Backspace) → DEL
            53 => b"\x1b",                                  // Escape
            122 => b"\x1bOP",                               // F1
            120 => b"\x1bOQ",                               // F2
            99 => b"\x1bOR",                                // F3
            118 => b"\x1bOS",                               // F4
            96 => b"\x1b[15~",                              // F5
            97 => b"\x1b[17~",                              // F6
            98 => b"\x1b[18~",                              // F7
            100 => b"\x1b[19~",                             // F8
            101 => b"\x1b[20~",                             // F9
            109 => b"\x1b[21~",                             // F10
            103 => b"\x1b[23~",                             // F11
            111 => b"\x1b[24~",                             // F12
            _ => return None,
        };
        Some(seq)
    }

    /// GCD dispatch source fires: drain all readable data from the master, feed it to the Grid, then request a redraw.
    fn on_readable(&self) {
        if self.ivars().ended.get() {
            return; // Already ended: EOF/read errors keep firing on the dead fd, ignore them
        }
        let fd = self.ivars().master_fd.get();
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
                self.mark_ended(); // shell exit (EOF)
                break;
            } else {
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => break,  // Drained
                    _ => {
                        self.mark_ended(); // EIO etc.: the shell is gone
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
            libc::ioctl(ivars.master_fd.get(), libc::TIOCSWINSZ, &ws);
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
                let ch = grid.view_cell(c, r).ch;
                if ch != '\0' {
                    line.push(ch); // skip wide-char trailer placeholders
                }
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
                let a0 = eff(grid.view_cell(start, r), &t);
                c += 1;
                while c < cols && eff(grid.view_cell(c, r), &t) == a0 {
                    c += 1;
                }
                let (fg, bg, flags) = a0;

                // Wide-char trailer run: the lead cell's glyph and its extended background/underline
                // already cover this column. Skip it entirely — painting its background here would
                // overwrite the right half of the wide glyph drawn by the lead cell.
                if flags & WIDE_TRAILER != 0 {
                    continue;
                }

                let run_x = PAD + start as f64 * cw;
                // A wide lead char is always the last cell of its run (its trailer has distinct attrs),
                // so if the cell just past the run is a trailer, extend the fill by one column to cover it.
                let trailing = c < cols && grid.view_cell(c, r).flags & WIDE_TRAILER != 0;
                let run_w = (c - start) as f64 * cw + if trailing { cw } else { 0.0 };

                // Background: only fill non-default backgrounds (the default background is already covered by the whole-window background).
                if bg != default_bg {
                    unsafe {
                        ns_color(bg).set();
                        NSRectFill(rect(run_x, y, run_w, lh));
                    }
                }

                // Text: a run of pure whitespace need not draw glyphs. Wide-char trailer cells hold
                // '\0' (the lead cell's glyph already spans both columns) — filter them out. The run
                // always breaks at a wide char (its trailer carries WIDE_TRAILER, a distinct attr set),
                // so each wide glyph is drawn on its own, positioned at its exact cell origin.
                let text: String = (start..c)
                    .map(|i| grid.view_cell(i, r).ch)
                    .filter(|&ch| ch != '\0')
                    .collect();
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
        // Hidden while scrolled back into the scrollback: the cursor lives on the live screen, and
        // painting it at the same row of a history viewport would mark an unrelated line.
        if grid.cursor_visible() && grid.view_offset() == 0 {
            let (cc, cr) = grid.cursor;
            if cc < cols && cr < rows {
                let x = PAD + cc as f64 * cw;
                let y = PAD + cr as f64 * lh;
                let cell = grid.view_cell(cc, cr);
                let (fg, bg, _) = eff(cell, &t);
                // A wide glyph occupies two columns; the cursor block covers both.
                let cur_w = if char_width(cell.ch) == 2 { 2.0 * cw } else { cw };
                unsafe {
                    ns_color(fg).set();
                    NSRectFill(rect(x, y, cur_w, lh));
                }
                if cell.ch != ' ' {
                    let color = ns_color(bg);
                    let attrs = make_attrs(&font, Some(&color));
                    let ns = NSString::from_str(&cell.ch.to_string());
                    unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(x, y + LINE_GAP / 2.0), Some(&attrs)) };
                }
            }
        }

        // IME preedit: draw the in-progress composition inline at the cursor, underlined, on top of
        // the grid (and the cursor block). Distinct RefCell from `grid`, so this borrow is independent.
        let marked = ivars.marked_text.borrow();
        if !marked.is_empty() && grid.view_offset() == 0 {
            let (cc, cr) = grid.cursor;
            if cc < cols && cr < rows {
                let x = PAD + cc as f64 * cw;
                let y = PAD + cr as f64 * lh;
                let n = marked.chars().count();
                // Clamp the width to the row's remaining cells so it can't overflow the view.
                let w = (n as f64 * cw).min((cols - cc) as f64 * cw);
                unsafe {
                    // Opaque background so the preedit stays readable over whatever was underneath.
                    ns_color(default_bg).set();
                    NSRectFill(rect(x, y, w, lh));
                    let color = ns_color(t.fg);
                    let attrs = make_attrs(&font, Some(&color));
                    let ns = NSString::from_str(&marked);
                    ns.drawAtPoint_withAttributes(NSPoint::new(x, y + LINE_GAP / 2.0), Some(&attrs));
                    // Underline marks the run as composing (not yet committed) text.
                    ns_color(t.fg).set();
                    NSRectFill(rect(x, y + lh - 1.0, w, 1.0));
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

/// Extract the plain string from an `insertText:`/`setMarkedText:` argument, which AppKit hands over
/// as `id` — either an `NSString` or an `NSAttributedString`.
fn ns_input_string(string: &AnyObject) -> String {
    unsafe {
        let is_attributed: bool = msg_send![string, isKindOfClass: NSAttributedString::class()];
        if is_attributed {
            let attr: &NSAttributedString = &*(string as *const AnyObject as *const NSAttributedString);
            attr.string().to_string()
        } else {
            let ns: &NSString = &*(string as *const AnyObject as *const NSString);
            ns.to_string()
        }
    }
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
    let fd = view.ivars().master_fd.get();
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
