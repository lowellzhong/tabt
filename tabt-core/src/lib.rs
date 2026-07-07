//! term-core — terminal emulation core (pure logic, zero OS dependencies).
//!
//! Step 3: replace the minimal `feed` from step 2 (which only handled
//! printable ASCII / line feed / carriage return) with a real VT/ANSI parsing
//! state machine (modeled on Paul Williams' VT500 parser state diagram:
//! ground / escape / csi / osc …). It now correctly handles:
//!   - SGR (colors plus bold/italic/underline/inverse) → applied to the current "pen", written into cells;
//!   - cursor movement CUU/CUD/CUF/CUB, CUP/HVP, CHA/VPA;
//!   - erase ED/EL/ECH, insert/delete ICH/DCH/IL/DL;
//!   - scroll region DECSTBM and IND/RI/NEL, SU/SD;
//!   - save/restore cursor DECSC/DECRC and CSI s/u;
//!   - DEC private modes `?…h/l` (autowrap, cursor visibility, alt screen, app cursor keys) are
//!     applied; bracketed paste (2004) is tracked via `bracketed_paste()` for the input layer;
//!     others are acknowledged and swallowed, no longer leaking out as literal text;
//!   - OSC title (`]0;…`) collected into `title`;
//!   - OSC 7 cwd reporting, percent-decoded;
//!   - alternate screen buffer (DECSET 47/1047/1049), used by vim/less/htop/man;
//!   - device status reports (DSR `CSI 6n`/`5n`) and device attributes (DA `CSI c`), queued via
//!     `take_replies()` for the caller to write back to the PTY;
//!   - UTF-8 multibyte character decoding.
//!
//! Still not implemented (left for later milestones): a scrollback buffer (once content scrolls
//! off the top it's gone for the session) and custom tab stops.

/// A single screen cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8, // see the BOLD/ITALIC/… bit definitions below
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

// Cell attribute bits.
pub const BOLD: u8 = 1 << 0;
pub const ITALIC: u8 = 1 << 1;
pub const UNDERLINE: u8 = 1 << 2;
pub const INVERSE: u8 = 1 << 3;
/// Trailing (right) half of a double-width character (e.g. CJK). The left cell holds the glyph;
/// this cell is a placeholder so the grid column count matches the display width. Its `ch` is `'\0'`
/// and renderers/`to_lines()` skip it. Set on the second cell of every wide glyph.
pub const WIDE_TRAILER: u8 = 1 << 4;

/// Display width of a character in terminal cells: 0 (combining/zero-width), 1 (normal), or 2
/// (East Asian wide / fullwidth). A dependency-free approximation of Unicode East Asian Width —
/// covers the CJK/Kana/Hangul/fullwidth blocks that matter in practice, not the full UAX #11 table.
pub fn char_width(ch: char) -> usize {
    let c = ch as u32;
    // Zero-width: combining marks and the common zero-width spaces/joiners.
    if matches!(c,
        0x0300..=0x036F | // combining diacritical marks
        0x200B..=0x200F | // zero-width space/joiner/marks
        0xFE00..=0xFE0F | // variation selectors
        0xFEFF            // zero-width no-break space (BOM)
    ) {
        return 0;
    }
    // Double-width: East Asian wide and fullwidth ranges.
    let wide = matches!(c,
        0x1100..=0x115F | // Hangul Jamo
        0x2E80..=0x303E | // CJK radicals, Kangxi, CJK symbols & punctuation
        0x3041..=0x33FF | // Hiragana, Katakana, Bopomofo, Hangul Compat Jamo, enclosed CJK, …
        0x3400..=0x4DBF | // CJK Unified Ideographs Extension A
        0x4E00..=0x9FFF | // CJK Unified Ideographs
        0xA000..=0xA4CF | // Yi Syllables/Radicals
        0xA960..=0xA97F | // Hangul Jamo Extended-A
        0xAC00..=0xD7A3 | // Hangul Syllables
        0xF900..=0xFAFF | // CJK Compatibility Ideographs
        0xFE10..=0xFE19 | // vertical forms
        0xFE30..=0xFE6F | // CJK compatibility forms, small form variants
        0xFF00..=0xFF60 | // fullwidth forms
        0xFFE0..=0xFFE6 | // fullwidth signs
        0x1F300..=0x1FAFF | // emoji & pictographs (mostly wide)
        0x20000..=0x3FFFD   // CJK Unified Ideographs Extension B and beyond
    );
    if wide {
        2
    } else {
        1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),     // 0-255 palette (0-7 normal, 8-15 bright, 16-255 color cube/grayscale)
    Rgb(u8, u8, u8), // true color
}

/// Parser state (a reduced subset of Williams' VT500 state diagram).
#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Escape,
    EscInt,   // intermediate bytes after ESC (charset selection, etc.), ignored wholesale
    CsiEntry, // just consumed ESC [
    CsiParam, // collecting parameters
    CsiInt,   // CSI intermediate bytes, ignored wholesale
    CsiIgnore,
    Osc,       // ESC ] string, up to BEL or ST
    DcsIgnore, // ESC P …, ignored wholesale until ST
}

/// A fixed-size screen grid plus an embedded VT parsing state machine.
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    pub cursor: (usize, usize), // (col, row)
    // Deferred wrap: after filling the last column the cursor stays put and this
    // bit is set; the actual wrap is deferred until the next printable character
    // arrives, matching xterm semantics.
    pending_wrap: bool,

    // ---- Current pen (SGR state): applied to newly written cells ----
    pen_fg: Color,
    pen_bg: Color,
    pen_flags: u8,

    // ---- Saved cursor (DECSC / CSI s): position + pen ----
    saved: Option<(usize, usize, Color, Color, u8)>,

    // ---- Alternate screen (alt screen, DECSET 47/1047/1049) ----
    // `inactive` holds the buffer not currently displayed; it is swapped with
    // `cells` when entering or leaving the alt screen.
    inactive: Vec<Cell>,
    alt: bool,
    // Cursor save dedicated to 1049 (kept separate from DECSC's `saved`, they don't interfere).
    alt_saved: Option<(usize, usize, Color, Color, u8)>,

    // ---- Scroll region (rows, inclusive on both ends), defaults to the full screen ----
    scroll_top: usize,
    scroll_bot: usize,

    // ---- Modes ----
    autowrap: bool,
    cursor_visible: bool,
    // DECCKM: application cursor keys mode. When on, arrow keys send ESC O x; when off, ESC [ x.
    app_cursor_keys: bool,
    // DECSET 2004: bracketed paste. When on, the caller should wrap pasted text in
    // ESC[200~ ... ESC[201~ so the program can tell it apart from typed keystrokes.
    bracketed_paste: bool,

    // ---- Window title received via OSC ----
    pub title: String,
    // ---- Current working directory reported via OSC 7 (local path parsed from a file:// URL) ----
    cwd: String,

    // ---- Pending replies to write back to the PTY (DSR/DA) ----
    // This layer has no I/O of its own; the caller drains this via `take_replies()` after each
    // `feed()` and writes it to the master fd. Without a reply, programs that block waiting for
    // one (cursor-position queries, device-attribute probes used by some shells/tmux/vim) hang.
    replies: Vec<u8>,

    // ---- Parser state machine internals ----
    state: State,
    params: Vec<u16>,
    csi_cur: u32,   // parameter currently being accumulated
    private: u8,    // CSI private prefix byte ('?', '>', etc.), 0 if none
    osc: Vec<u8>,   // OSC string buffer
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor: (0, 0),
            pending_wrap: false,
            pen_fg: Color::Default,
            pen_bg: Color::Default,
            pen_flags: 0,
            saved: None,
            inactive: vec![Cell::default(); cols * rows],
            alt: false,
            alt_saved: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            autowrap: true,
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            title: String::new(),
            cwd: String::new(),
            replies: Vec::new(),
            state: State::Ground,
            params: Vec::new(),
            csi_cur: 0,
            private: 0,
            osc: Vec::new(),
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    pub fn cell(&self, col: usize, row: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    pub fn cell_mut(&mut self, col: usize, row: usize) -> &mut Cell {
        &mut self.cells[row * self.cols + col]
    }

    /// Whether the cursor is shown (DECTCEM), for the rendering layer's reference.
    /// Current working directory (reported via OSC 7; empty when the shell hasn't reported it).
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Drain any DSR/DA replies queued since the last call. The caller must write these bytes
    /// back to the PTY master fd after each `feed()` — see the `replies` field's doc comment.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Whether application cursor keys mode (DECCKM) is active, so the input layer can pick the arrow-key escape prefix.
    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    /// Whether bracketed paste mode (DECSET 2004) is active, so the input layer knows whether to
    /// wrap a paste in `ESC[200~ ... ESC[201~`.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Feed bytes: advance the parsing state machine one byte at a time.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => self.ground(b),
            State::Escape => self.escape(b),
            State::EscInt => self.esc_int(b),
            State::CsiEntry => self.csi_entry(b),
            State::CsiParam => self.csi_param(b),
            State::CsiInt => self.csi_int(b),
            State::CsiIgnore => self.csi_ignore(b),
            State::Osc => self.osc(b),
            State::DcsIgnore => self.dcs(b),
        }
    }

    // ===================== Ground (normal state) =====================

    fn ground(&mut self, b: u8) {
        if self.utf8_need > 0 {
            self.utf8_collect(b);
            return;
        }
        match b {
            0x1b => self.enter_escape(),
            0x00..=0x1f => self.execute(b),
            0x20..=0x7e => self.print(b as char),
            0x7f => {} // DEL ignored
            _ => self.utf8_begin(b),
        }
    }

    fn utf8_begin(&mut self, b: u8) {
        let need = if b >= 0xf0 {
            4
        } else if b >= 0xe0 {
            3
        } else if b >= 0xc0 {
            2
        } else {
            0 // a stray continuation byte 0x80-0xbf: invalid
        };
        if need == 0 {
            self.print('\u{fffd}');
            return;
        }
        self.utf8_buf[0] = b;
        self.utf8_len = 1;
        self.utf8_need = need;
    }

    fn utf8_collect(&mut self, b: u8) {
        if b & 0xc0 == 0x80 && self.utf8_len < self.utf8_need {
            self.utf8_buf[self.utf8_len] = b;
            self.utf8_len += 1;
            if self.utf8_len == self.utf8_need {
                let ch = std::str::from_utf8(&self.utf8_buf[..self.utf8_len])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('\u{fffd}');
                self.utf8_need = 0;
                self.utf8_len = 0;
                self.print(ch);
            }
        } else {
            // Continuation broke off: treat the previous character as corrupt and reprocess the current byte in the normal state.
            self.utf8_need = 0;
            self.utf8_len = 0;
            self.print('\u{fffd}');
            self.ground(b);
        }
    }

    fn print(&mut self, ch: char) {
        let w = char_width(ch);
        if w == 0 {
            // Zero-width (combining marks, etc.): the minimal core does not compose them onto the
            // previous cell — drop them rather than let them consume a column.
            return;
        }
        if self.pending_wrap {
            self.cursor.0 = 0;
            self.linefeed();
            self.pending_wrap = false;
        }
        // A double-width glyph can't straddle the right margin: if only one column is left, wrap to
        // the next line first so the pair stays together (matches how the shell lays out CJK).
        if w == 2 && self.cursor.0 + 1 >= self.cols && self.autowrap {
            self.cursor.0 = 0;
            self.linefeed();
        }
        let (c, r) = self.cursor;
        let (fg, bg, fl) = (self.pen_fg, self.pen_bg, self.pen_flags);
        {
            let cell = self.cell_mut(c, r);
            cell.ch = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = fl;
        }
        // Second half of a wide glyph: a placeholder cell the renderer/to_lines() skip.
        if w == 2 && c + 1 < self.cols {
            let cell = self.cell_mut(c + 1, r);
            cell.ch = '\0';
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = fl | WIDE_TRAILER;
        }
        if c + w >= self.cols {
            // Stay in the last column; only set the deferred-wrap bit if autowrap is on.
            if self.autowrap {
                self.pending_wrap = true;
            }
        } else {
            self.cursor.0 = c + w;
        }
    }

    /// C0 control characters.
    fn execute(&mut self, b: u8) {
        match b {
            0x08 => {
                // BS
                self.pending_wrap = false;
                if self.cursor.0 > 0 {
                    self.cursor.0 -= 1;
                }
            }
            0x09 => {
                // HT: next 8-column tab stop
                self.pending_wrap = false;
                let next = (self.cursor.0 / 8 + 1) * 8;
                self.cursor.0 = next.min(self.cols - 1);
            }
            0x0a | 0x0b | 0x0c => self.linefeed(), // LF / VT / FF
            0x0d => {
                // CR
                self.pending_wrap = false;
                self.cursor.0 = 0;
            }
            _ => {} // BEL (0x07) and others: ignored
        }
    }

    fn enter_escape(&mut self) {
        self.state = State::Escape;
    }

    // ===================== Escape family =====================

    fn escape(&mut self, b: u8) {
        match b {
            0x1b => {}                        // consecutive ESC: stay in Escape
            0x18 | 0x1a => self.state = State::Ground, // CAN / SUB: abort
            0x00..=0x1f => self.execute(b),   // C0 embedded in an escape is still executed immediately
            b'[' => {
                self.csi_reset();
                self.state = State::CsiEntry;
            }
            b']' => {
                self.osc.clear();
                self.state = State::Osc;
            }
            b'P' => self.state = State::DcsIgnore,
            0x20..=0x2f => self.state = State::EscInt, // intermediate bytes (charset selection, etc.)
            0x30..=0x7e => {
                self.esc_dispatch(b);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn esc_int(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x20..=0x2f => {}                       // keep consuming intermediate bytes
            _ => self.state = State::Ground,        // final byte: charset selection, etc., ignored
        }
    }

    fn esc_dispatch(&mut self, b: u8) {
        match b {
            b'7' => self.save_cursor(),       // DECSC
            b'8' => self.restore_cursor(),    // DECRC
            b'D' => self.linefeed(),          // IND
            b'M' => self.reverse_index(),     // RI
            b'E' => {
                // NEL
                self.cursor.0 = 0;
                self.linefeed();
            }
            b'c' => self.hard_reset(),        // RIS
            b'=' | b'>' => {}                 // keypad application/numeric mode: ignored
            _ => {}
        }
    }

    // ===================== CSI family =====================

    fn csi_reset(&mut self) {
        self.params.clear();
        self.csi_cur = 0;
        self.private = 0;
    }

    fn push_param(&mut self) {
        self.params.push(self.csi_cur.min(65535) as u16);
        self.csi_cur = 0;
    }

    fn csi_entry(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x30..=0x39 => {
                self.csi_cur = (b - 0x30) as u32;
                self.state = State::CsiParam;
            }
            b';' => {
                self.push_param();
                self.state = State::CsiParam;
            }
            0x3c..=0x3f => {
                // private prefix < = > ?
                self.private = b;
                self.state = State::CsiParam;
            }
            0x3a => self.state = State::CsiIgnore, // ':' subparameters: ignored wholesale
            0x20..=0x2f => self.state = State::CsiInt,
            0x40..=0x7e => {
                self.finalize_and_dispatch(b);
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_param(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x30..=0x39 => {
                self.csi_cur = (self.csi_cur * 10 + (b - 0x30) as u32).min(65535);
            }
            b';' => self.push_param(),
            0x3a | 0x3c..=0x3f => self.state = State::CsiIgnore,
            0x20..=0x2f => self.state = State::CsiInt,
            0x40..=0x7e => {
                self.finalize_and_dispatch(b);
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_int(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x20..=0x2f => {}
            _ => self.state = State::Ground, // CSI with intermediate bytes: ignored
        }
    }

    fn csi_ignore(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x40..=0x7e => self.state = State::Ground,
            _ => {}
        }
    }

    fn finalize_and_dispatch(&mut self, final_byte: u8) {
        self.push_param(); // finalize: push the last parameter (possibly the default 0)
        self.csi_dispatch(final_byte);
        self.state = State::Ground;
    }

    /// Get the i-th parameter, interpreted as "1 is the default (both 0 and missing count as 1)" — used for cursor-type operations.
    fn p1(&self, i: usize) -> usize {
        match self.params.get(i) {
            Some(&v) if v != 0 => v as usize,
            _ => 1,
        }
    }

    /// Get the raw value of the i-th parameter (default 0) — used for SGR / erase modes, etc.
    fn praw(&self, i: usize) -> u16 {
        self.params.get(i).copied().unwrap_or(0)
    }

    fn csi_dispatch(&mut self, f: u8) {
        match f {
            b'A' => self.move_up(self.p1(0)),
            b'B' | b'e' => self.move_down(self.p1(0)),
            b'C' | b'a' => self.move_right(self.p1(0)),
            b'D' => self.move_left(self.p1(0)),
            b'E' => {
                self.cursor.0 = 0;
                self.move_down(self.p1(0));
            }
            b'F' => {
                self.cursor.0 = 0;
                self.move_up(self.p1(0));
            }
            b'G' | b'`' => self.set_col(self.p1(0) - 1),
            b'd' => self.set_row(self.p1(0) - 1),
            b'H' | b'f' => {
                let r = self.p1(0) - 1;
                let c = self.p1(1) - 1;
                self.set_pos(c, r);
            }
            b'J' => self.erase_display(self.praw(0)),
            b'K' => self.erase_line(self.praw(0)),
            b'm' => self.sgr(),
            b'r' => self.set_scroll_region(),
            b'L' => self.insert_lines(self.p1(0)),
            b'M' => self.delete_lines(self.p1(0)),
            b'@' => self.insert_chars(self.p1(0)),
            b'P' => self.delete_chars(self.p1(0)),
            b'X' => self.erase_chars(self.p1(0)),
            b'S' => self.scroll_up(self.scroll_top, self.scroll_bot, self.p1(0)),
            b'T' => self.scroll_down(self.scroll_top, self.scroll_bot, self.p1(0)),
            b'h' => self.set_mode(true),
            b'l' => self.set_mode(false),
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            b'n' => self.report_status(),
            b'c' => self.report_device_attrs(),
            _ => {}
        }
    }

    /// DSR (`CSI Ps n`): Ps=5 "are you OK?" → `CSI 0 n`; Ps=6 "report cursor position" →
    /// `CSI row;col R`. Some programs (vim, tmux, shell prompt themes) block waiting for one of
    /// these and would otherwise hang.
    fn report_status(&mut self) {
        match self.praw(0) {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let (row, col) = (self.cursor.1 + 1, self.cursor.0 + 1);
                self.replies.extend_from_slice(format!("\x1b[{};{}R", row, col).as_bytes());
            }
            _ => {}
        }
    }

    /// DA (`CSI c` / `CSI > c`): device attributes. Programs probe this to detect terminal
    /// capabilities; any minimally-valid reply unblocks them. `?1;2c` = VT100 with AVO, the same
    /// baseline many minimal terminal emulators report.
    fn report_device_attrs(&mut self) {
        self.replies.extend_from_slice(b"\x1b[?1;2c");
    }

    // ===================== OSC / DCS strings =====================

    fn osc(&mut self, b: u8) {
        match b {
            0x07 => {
                self.osc_dispatch();
                self.state = State::Ground;
            }
            0x1b => {
                // expecting ST (ESC \): finalize the OSC first, then let Escape consume that '\'
                self.osc_dispatch();
                self.state = State::Escape;
            }
            0x18 | 0x1a => self.state = State::Ground,
            _ => {
                if self.osc.len() < 1024 {
                    self.osc.push(b);
                }
            }
        }
    }

    fn dcs(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Escape,
            0x07 => self.state = State::Ground,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self) {
        // Of the form "n;text": n ∈ {0,1,2} sets the title; n=7 reports the cwd (file://host/path).
        let s = String::from_utf8_lossy(&self.osc);
        if let Some((num, text)) = s.split_once(';') {
            if matches!(num, "0" | "1" | "2") {
                self.title = text.to_string();
            } else if num == "7" {
                if let Some(rest) = text.strip_prefix("file://") {
                    if let Some(slash) = rest.find('/') {
                        self.cwd = percent_decode(&rest[slash..]);
                    }
                }
            }
        }
        self.osc.clear();
    }

    // ===================== Cursor movement =====================

    fn move_up(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.1 = self.cursor.1.saturating_sub(n);
    }
    fn move_down(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.1 = (self.cursor.1 + n).min(self.rows - 1);
    }
    fn move_left(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.0 = self.cursor.0.saturating_sub(n);
    }
    fn move_right(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.0 = (self.cursor.0 + n).min(self.cols - 1);
    }
    fn set_col(&mut self, c: usize) {
        self.pending_wrap = false;
        self.cursor.0 = c.min(self.cols - 1);
    }
    fn set_row(&mut self, r: usize) {
        self.pending_wrap = false;
        self.cursor.1 = r.min(self.rows - 1);
    }
    fn set_pos(&mut self, c: usize, r: usize) {
        self.pending_wrap = false;
        self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
    }

    fn save_cursor(&mut self) {
        self.saved = Some((self.cursor.0, self.cursor.1, self.pen_fg, self.pen_bg, self.pen_flags));
    }
    fn restore_cursor(&mut self) {
        if let Some((c, r, fg, bg, fl)) = self.saved {
            self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
            self.pen_fg = fg;
            self.pen_bg = bg;
            self.pen_flags = fl;
            self.pending_wrap = false;
        }
    }

    // ===================== Line feed / scrolling =====================

    /// LF / IND: scroll up if at the bottom of the scroll region, otherwise move down one row (column unchanged).
    fn linefeed(&mut self) {
        if self.cursor.1 == self.scroll_bot {
            self.scroll_up(self.scroll_top, self.scroll_bot, 1);
        } else if self.cursor.1 + 1 < self.rows {
            self.cursor.1 += 1;
        }
        // A vertical move cancels any deferred autowrap from the previous row; otherwise the
        // next printed character would force a spurious extra line feed + column reset.
        self.pending_wrap = false;
    }

    /// RI: scroll down if at the top of the scroll region, otherwise move up one row.
    fn reverse_index(&mut self) {
        if self.cursor.1 == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bot, 1);
        } else if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        }
        self.pending_wrap = false;
    }

    /// Scroll the row range [top, bot] up by n rows, filling the bottom with blanks.
    fn scroll_up(&mut self, top: usize, bot: usize, n: usize) {
        if top >= bot || n == 0 {
            return;
        }
        let cols = self.cols;
        let h = bot - top + 1;
        let n = n.min(h);
        let end = (bot + 1) * cols;
        if n < h {
            self.cells.copy_within((top + n) * cols..end, top * cols);
        }
        for r in (bot + 1 - n)..=bot {
            self.blank_row(r);
        }
    }

    /// Scroll the row range [top, bot] down by n rows, filling the top with blanks.
    fn scroll_down(&mut self, top: usize, bot: usize, n: usize) {
        if top >= bot || n == 0 {
            return;
        }
        let cols = self.cols;
        let h = bot - top + 1;
        let n = n.min(h);
        let start = top * cols;
        if n < h {
            self.cells.copy_within(start..(bot + 1 - n) * cols, (top + n) * cols);
        }
        for r in top..(top + n) {
            self.blank_row(r);
        }
    }

    fn insert_lines(&mut self, n: usize) {
        let r = self.cursor.1;
        if r < self.scroll_top || r > self.scroll_bot {
            return;
        }
        self.scroll_down(r, self.scroll_bot, n);
        self.cursor.0 = 0;
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let r = self.cursor.1;
        if r < self.scroll_top || r > self.scroll_bot {
            return;
        }
        self.scroll_up(r, self.scroll_bot, n);
        self.cursor.0 = 0;
        self.pending_wrap = false;
    }

    // ===================== In-line insert / delete / erase =====================

    fn insert_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.cells.copy_within(base + c..base + cols - n, base + c + n);
        self.blank_range(base + c, base + c + n);
        self.pending_wrap = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.cells.copy_within(base + c + n..base + cols, base + c);
        self.blank_range(base + cols - n, base + cols);
        self.pending_wrap = false;
    }

    fn erase_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.blank_range(base + c, base + c + n);
        self.pending_wrap = false;
    }

    fn erase_display(&mut self, mode: u16) {
        let (c, r) = self.cursor;
        let idx = r * self.cols + c;
        let len = self.cells.len();
        match mode {
            0 => self.blank_range(idx, len),     // cursor to end of screen
            1 => self.blank_range(0, idx + 1),   // start of screen to cursor
            2 | 3 => self.blank_range(0, len),   // whole screen (3 includes scrollback, which this layer lacks)
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (c, r) = self.cursor;
        let base = r * self.cols;
        match mode {
            0 => self.blank_range(base + c, base + self.cols), // cursor to end of line
            1 => self.blank_range(base, base + c + 1),         // start of line to cursor
            2 => self.blank_range(base, base + self.cols),     // whole line
            _ => {}
        }
    }

    fn blank_row(&mut self, r: usize) {
        let base = r * self.cols;
        self.blank_range(base, base + self.cols);
    }

    fn blank_range(&mut self, start: usize, end: usize) {
        for cell in &mut self.cells[start..end] {
            *cell = Cell::default();
        }
    }

    // ===================== SGR / modes / scroll region =====================

    fn sgr(&mut self) {
        // No parameters is equivalent to [0] (reset).
        let params = if self.params.is_empty() { vec![0u16] } else { self.params.clone() };
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => {
                    self.pen_fg = Color::Default;
                    self.pen_bg = Color::Default;
                    self.pen_flags = 0;
                }
                1 => self.pen_flags |= BOLD,
                3 => self.pen_flags |= ITALIC,
                4 => self.pen_flags |= UNDERLINE,
                7 => self.pen_flags |= INVERSE,
                21 | 22 => self.pen_flags &= !BOLD,
                23 => self.pen_flags &= !ITALIC,
                24 => self.pen_flags &= !UNDERLINE,
                27 => self.pen_flags &= !INVERSE,
                30..=37 => self.pen_fg = Color::Indexed((p - 30) as u8),
                38 => self.pen_fg = Self::ext_color(&params, &mut i).unwrap_or(self.pen_fg),
                39 => self.pen_fg = Color::Default,
                40..=47 => self.pen_bg = Color::Indexed((p - 40) as u8),
                48 => self.pen_bg = Self::ext_color(&params, &mut i).unwrap_or(self.pen_bg),
                49 => self.pen_bg = Color::Default,
                90..=97 => self.pen_fg = Color::Indexed((p - 90 + 8) as u8),
                100..=107 => self.pen_bg = Color::Indexed((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    /// Parse the extended color for 38/48: `5;n` (indexed) or `2;r;g;b` (true color).
    /// After the call, `i` points at the last subparameter consumed.
    fn ext_color(params: &[u16], i: &mut usize) -> Option<Color> {
        match params.get(*i + 1).copied() {
            Some(5) => {
                let n = params.get(*i + 2).copied().unwrap_or(0) as u8;
                *i += 2;
                Some(Color::Indexed(n))
            }
            Some(2) => {
                let r = params.get(*i + 2).copied().unwrap_or(0) as u8;
                let g = params.get(*i + 3).copied().unwrap_or(0) as u8;
                let b = params.get(*i + 4).copied().unwrap_or(0) as u8;
                *i += 4;
                Some(Color::Rgb(r, g, b))
            }
            _ => None,
        }
    }

    fn set_mode(&mut self, set: bool) {
        if self.private == b'?' {
            let params = self.params.clone();
            for p in params {
                match p {
                    1 => self.app_cursor_keys = set, // DECCKM application cursor keys
                    7 => self.autowrap = set,        // DECAWM autowrap
                    25 => self.cursor_visible = set, // DECTCEM cursor visibility
                    1048 => {
                        // save/restore cursor only
                        if set {
                            self.save_cursor();
                        } else {
                            self.restore_cursor();
                        }
                    }
                    47 | 1047 => {
                        // switch alt screen (leave cursor untouched)
                        if set {
                            self.enter_alt();
                        } else {
                            self.leave_alt();
                        }
                    }
                    1049 => {
                        // switch alt screen + save/restore cursor + clear and home on entry
                        if set {
                            self.alt_saved = Some((
                                self.cursor.0,
                                self.cursor.1,
                                self.pen_fg,
                                self.pen_bg,
                                self.pen_flags,
                            ));
                            self.enter_alt();
                            self.set_pos(0, 0);
                        } else {
                            self.leave_alt();
                            if let Some((c, r, fg, bg, fl)) = self.alt_saved.take() {
                                self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
                                self.pen_fg = fg;
                                self.pen_bg = bg;
                                self.pen_flags = fl;
                                self.pending_wrap = false;
                            }
                        }
                    }
                    2004 => self.bracketed_paste = set,
                    _ => {} // other private modes: acknowledged and ignored
                }
            }
        }
        // Non-private ANSI modes (such as IRM insert mode) are not implemented yet.
    }

    /// Enter the alt screen: swap with the undisplayed buffer, clear the newly displayed one, and reset the scroll region.
    fn enter_alt(&mut self) {
        if self.alt {
            return;
        }
        self.alt = true;
        std::mem::swap(&mut self.cells, &mut self.inactive);
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.pending_wrap = false;
    }

    /// Leave the alt screen: switch back to the main screen (whose contents were kept in the undisplayed buffer all along).
    fn leave_alt(&mut self) {
        if !self.alt {
            return;
        }
        self.alt = false;
        std::mem::swap(&mut self.cells, &mut self.inactive);
        self.pending_wrap = false;
    }

    /// Resize the screen: reallocate the main/alt buffers, top-anchored (content keeps its
    /// position from the top; growing just adds blank rows at the bottom). Only when shrinking
    /// below the cursor are the oldest top rows dropped, so the cursor / most recent output stays
    /// visible, with the cursor position following along.
    /// No automatic reflow; after receiving SIGWINCH the shell redraws the current line itself.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let (oc, or) = (self.cols, self.rows);
        // Top-anchored: content keeps its position from the top (so a taller screen just gains blank
        // rows at the bottom). Only when shrinking below the cursor do we drop the oldest (top) rows,
        // so the cursor / most-recent output stays visible.
        let drop_top = self.cursor.1.saturating_sub(rows - 1);
        self.cells = Self::resize_buf(&self.cells, oc, or, cols, rows, drop_top);
        self.inactive = Self::resize_buf(&self.inactive, oc, or, cols, rows, drop_top);
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.cursor.1 = (self.cursor.1 - drop_top).min(rows - 1);
        self.cursor.0 = self.cursor.0.min(cols - 1);
        self.pending_wrap = false;
    }

    /// Move a buffer into the new dimensions: columns left-aligned, rows top-anchored after dropping
    /// `drop_top` oldest rows (nonzero only when shrinking below the cursor).
    fn resize_buf(old: &[Cell], oc: usize, or: usize, nc: usize, nr: usize, drop_top: usize) -> Vec<Cell> {
        let mut v = vec![Cell::default(); nc * nr];
        let copy_c = oc.min(nc);
        let copy_r = or.saturating_sub(drop_top).min(nr);
        for i in 0..copy_r {
            let old_row = drop_top + i;
            let new_row = i; // top-anchored
            for c in 0..copy_c {
                v[new_row * nc + c] = old[old_row * oc + c];
            }
        }
        v
    }

    fn set_scroll_region(&mut self) {
        let top = self.p1(0) - 1;
        let bot = match self.praw(1) {
            0 => self.rows - 1,
            v => (v as usize).saturating_sub(1).min(self.rows - 1),
        };
        if top < bot {
            self.scroll_top = top;
            self.scroll_bot = bot;
        }
        // DECSTBM resets the cursor to the top-left corner of the screen.
        self.set_pos(0, 0);
    }

    fn hard_reset(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        for cell in &mut self.inactive {
            *cell = Cell::default();
        }
        self.alt = false;
        self.alt_saved = None;
        self.cursor = (0, 0);
        self.pending_wrap = false;
        self.pen_fg = Color::Default;
        self.pen_bg = Color::Default;
        self.pen_flags = 0;
        self.saved = None;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.autowrap = true;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.state = State::Ground;
        self.params.clear();
        self.csi_cur = 0;
        self.private = 0;
        self.osc.clear();
        self.utf8_need = 0;
        self.utf8_len = 0;
    }

    /// For dumb rendering / tests: export as per-row text (with trailing whitespace stripped).
    pub fn to_lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|r| {
                (0..self.cols)
                    .map(|c| self.cell(c, r).ch)
                    .filter(|&ch| ch != '\0') // drop wide-char trailer placeholders
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// Percent-decode an OSC 7 path (e.g. %20 → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Decode the two hex digits directly from bytes (not via str slicing): a '%' can be
        // immediately followed by a multibyte UTF-8 character whose bytes don't fall on a char
        // boundary at i+1/i+3, which would panic if sliced as a &str.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a single ASCII hex digit byte (0-9/a-f/A-F) to its numeric value.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_and_wrap() {
        let mut g = Grid::new(5, 2);
        g.feed(b"hello worl");
        assert_eq!(g.to_lines(), vec!["hello", " worl"]);
    }

    #[test]
    fn linefeed_after_wrap_pending_does_not_skip_a_row() {
        // Regression test: a bare LF right after filling the last column used to leave
        // pending_wrap set, so the NEXT printed character forced an extra spurious linefeed +
        // column reset, silently skipping a row instead of the plain "move down one row, same
        // column" that a bare LF should do.
        let mut g = Grid::new(5, 3);
        g.feed(b"hello"); // fills the last column, sets pending_wrap
        g.feed(b"\n"); // bare LF: should just move down one row, same column
        g.feed(b"X");
        assert_eq!(g.to_lines(), vec!["hello", "    X", ""]);
    }

    #[test]
    fn reverse_index_after_wrap_pending_does_not_skip_a_row() {
        // Same bug as above, via RI (ESC M) instead of a bare LF: move the cursor down first so
        // RI moves up (not scrolling), fill the last column, then RI + print.
        let mut g = Grid::new(5, 3);
        g.feed(b"\n"); // cursor to row 1
        g.feed(b"hello"); // fills row 1's last column, sets pending_wrap
        g.feed(b"\x1bM"); // RI: should just move up one row, same column
        g.feed(b"X");
        assert_eq!(g.to_lines(), vec!["    X", "hello", ""]);
    }

    #[test]
    fn crlf_moves_cursor() {
        let mut g = Grid::new(10, 3);
        g.feed(b"ab\r\ncd");
        assert_eq!(g.to_lines(), vec!["ab", "cd", ""]);
        assert_eq!(g.cursor, (2, 1));
    }

    #[test]
    fn scroll_at_bottom() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3");
        assert_eq!(g.to_lines(), vec!["2", "3"]);
    }

    #[test]
    fn scroll_three_lines() {
        let mut g = Grid::new(3, 3);
        g.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(g.to_lines(), vec!["2", "3", "4"]);
    }

    #[test]
    fn cup_positions_cursor() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[3;5HX"); // row 3, column 5 (1-based) → (col=4, row=2)
        assert_eq!(g.cell(4, 2).ch, 'X');
        assert_eq!(g.cursor, (5, 2));
    }

    #[test]
    fn cursor_movement_relative() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[2B\x1b[3CY"); // down 2, right 3 → (3,2)
        assert_eq!(g.cell(3, 2).ch, 'Y');
    }

    #[test]
    fn sgr_sets_pen() {
        let mut g = Grid::new(10, 2);
        g.feed(b"\x1b[1;31mA\x1b[0mB");
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(1));
        assert_ne!(g.cell(0, 0).flags & BOLD, 0);
        assert_eq!(g.cell(1, 0).fg, Color::Default);
        assert_eq!(g.cell(1, 0).flags, 0);
    }

    #[test]
    fn sgr_truecolor() {
        let mut g = Grid::new(4, 1);
        g.feed(b"\x1b[38;2;10;20;30mZ");
        assert_eq!(g.cell(0, 0).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn private_mode_is_swallowed() {
        // Private modes such as bracketed paste should not leak out as literal text.
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b[?2004hhi\x1b[?2004l");
        assert_eq!(g.to_lines()[0], "hi");
    }

    #[test]
    fn bracketed_paste_mode_is_tracked() {
        let mut g = Grid::new(10, 1);
        assert!(!g.bracketed_paste());
        g.feed(b"\x1b[?2004h");
        assert!(g.bracketed_paste());
        g.feed(b"\x1b[?2004l");
        assert!(!g.bracketed_paste());
    }

    #[test]
    fn erase_line_to_end() {
        let mut g = Grid::new(5, 1);
        g.feed(b"abc\r\x1b[K"); // carriage return to start of line, then erase to end of line
        assert_eq!(g.to_lines()[0], "");
    }

    #[test]
    fn erase_display_all() {
        let mut g = Grid::new(3, 2);
        g.feed(b"abcdef\x1b[2J");
        assert_eq!(g.to_lines(), vec!["", ""]);
    }

    #[test]
    fn delete_chars_shifts_left() {
        let mut g = Grid::new(6, 1);
        g.feed(b"abcdef\x1b[1G\x1b[2P"); // back to column 1, delete 2 characters
        assert_eq!(g.to_lines()[0], "cdef");
    }

    #[test]
    fn scroll_region_confines_scroll() {
        let mut g = Grid::new(3, 3);
        g.feed(b"\x1b[1;2r"); // scroll region confined to the first two rows
        g.feed(b"a\r\nb\r\nc\r\nd");
        assert_eq!(g.to_lines(), vec!["c", "d", ""]);
    }

    #[test]
    fn osc_sets_title() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b]0;hello\x07X");
        assert_eq!(g.title, "hello");
        assert_eq!(g.cell(0, 0).ch, 'X');
    }

    #[test]
    fn dsr_reports_cursor_position() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[3;5H"); // move to row 3, col 5 (1-based)
        g.feed(b"\x1b[6n"); // DSR: report cursor position
        assert_eq!(g.take_replies(), b"\x1b[3;5R");
    }

    #[test]
    fn dsr_reports_ok_status() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[5n"); // DSR: "are you OK?"
        assert_eq!(g.take_replies(), b"\x1b[0n");
    }

    #[test]
    fn da_reports_device_attributes() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[c"); // DA: primary device attributes
        assert_eq!(g.take_replies(), b"\x1b[?1;2c");
    }

    #[test]
    fn take_replies_drains_and_does_not_leak_into_the_grid() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[6n");
        assert!(!g.take_replies().is_empty());
        assert!(g.take_replies().is_empty()); // second call: nothing left
        assert_eq!(g.to_lines()[0], ""); // the query never printed as visible text
    }

    #[test]
    fn osc7_sets_cwd() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b]7;file://host/Users/me/My%20Code\x07");
        assert_eq!(g.cwd(), "/Users/me/My Code");
    }

    #[test]
    fn osc7_percent_before_multibyte_does_not_panic() {
        // Regression test: a '%' immediately followed by a multibyte UTF-8 character (here "€",
        // 3 bytes) used to panic in percent_decode's raw &str byte-offset slicing, because the
        // slice end landed mid-character instead of on a char boundary.
        let mut g = Grid::new(10, 1);
        let mut msg = b"\x1b]7;file://host/%\xe2\x82\xac".to_vec(); // '%' + '€' (U+20AC)
        msg.push(0x07);
        g.feed(&msg); // must not panic
    }

    #[test]
    fn utf8_decoding() {
        let mut g = Grid::new(5, 1);
        g.feed("héλ".as_bytes());
        assert_eq!(g.cell(0, 0).ch, 'h');
        assert_eq!(g.cell(1, 0).ch, 'é');
        assert_eq!(g.cell(2, 0).ch, 'λ');
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        // Each CJK glyph takes two columns: the lead cell holds the char, the next is a '\0' trailer
        // flagged WIDE_TRAILER. The cursor advances by 2, keeping the grid in sync with the shell.
        let mut g = Grid::new(10, 1);
        g.feed("你a".as_bytes());
        assert_eq!(g.cell(0, 0).ch, '你');
        assert_eq!(g.cell(1, 0).ch, '\0');
        assert_ne!(g.cell(1, 0).flags & WIDE_TRAILER, 0);
        assert_eq!(g.cell(2, 0).ch, 'a'); // 'a' lands after the 2-cell wide char, not at col 1
        assert_eq!(g.cursor, (3, 0));
        assert_eq!(g.to_lines()[0], "你a"); // trailer placeholder is not exported
    }

    #[test]
    fn wide_char_wraps_at_right_margin() {
        // Two columns wide, only one left: the pair must not straddle the margin — it wraps whole.
        let mut g = Grid::new(3, 2);
        g.feed("ab你".as_bytes()); // 'a','b' fill cols 0,1; one col left → 你 wraps to row 1
        assert_eq!(g.cell(0, 0).ch, 'a');
        assert_eq!(g.cell(1, 0).ch, 'b');
        assert_eq!(g.cell(0, 1).ch, '你');
        assert_eq!(g.cell(1, 1).ch, '\0');
    }

    #[test]
    fn alt_screen_preserves_main() {
        let mut g = Grid::new(4, 2);
        g.feed(b"main");
        g.feed(b"\x1b[?1049h"); // enter alt screen
        assert_eq!(g.to_lines(), vec!["", ""]); // the alt screen is empty
        g.feed(b"XY");
        assert_eq!(g.to_lines(), vec!["XY", ""]);
        g.feed(b"\x1b[?1049l"); // leave alt screen
        assert_eq!(g.to_lines()[0], "main"); // main screen contents restored as-is
    }

    #[test]
    fn resize_shrink_keeps_top_when_cursor_fits() {
        // Unlike naive bottom-anchoring (always keep the last N rows), a shrink that still fits
        // the cursor should NOT drop anything — content stays anchored to the top.
        let mut g = Grid::new(4, 3);
        g.feed(b"ab\r\ncd"); // cursor ends on row 1 (0-indexed), row 2 was never written
        g.resize(4, 2);
        assert_eq!(g.to_lines(), vec!["ab", "cd"]);
    }

    #[test]
    fn resize_shrink_drops_rows_above_cursor() {
        let mut g = Grid::new(4, 3);
        g.feed(b"ab\r\ncd\r\nef"); // cursor ends on the last row
        g.resize(4, 2); // shrink to 2 rows: drop the oldest row so the cursor stays visible
        assert_eq!(g.to_lines(), vec!["cd", "ef"]);
    }

    #[test]
    fn resize_grow_keeps_content() {
        let mut g = Grid::new(4, 2);
        g.feed(b"ab\r\ncd");
        g.resize(4, 3);
        // Top-anchored: the original rows stay at the top; the extra row is blank at the bottom.
        assert_eq!(g.to_lines(), vec!["ab", "cd", ""]);
    }

    #[test]
    fn save_restore_cursor() {
        let mut g = Grid::new(10, 3);
        g.feed(b"\x1b[2;3H\x1b7\x1b[1;1HX\x1b8Y"); // save at (2,1), go to origin and write X, restore and write Y
        assert_eq!(g.cell(0, 0).ch, 'X');
        assert_eq!(g.cell(2, 1).ch, 'Y');
    }
}
