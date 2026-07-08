//! Notes button, live note search, and in-app plain-text note editing.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSBezierPath, NSColor, NSEvent, NSFont, NSMenu, NSMenuItem,
    NSRectFill, NSScrollView, NSStringDrawing, NSTextView, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::app::{AppController, NoteSnap};
use crate::view::{draw_symbol, draw_truncated, make_attrs, ns_color, rect};

pub const NOTE_W: f64 = 24.0;

pub struct NoteIvars {
    controller: Cell<*const AppController>,
}

declare_class!(
    pub struct NoteButton;

    unsafe impl ClassType for NoteButton {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "NoteButton";
    }

    impl DeclaredClass for NoteButton {
        type Ivars = NoteIvars;
    }

    unsafe impl NSObjectProtocol for NoteButton {}

    unsafe impl NoteButton {
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
            self.show_menu(event);
        }

        #[method(newNote:)]
        fn new_note(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.controller() {
                c.new_note();
            }
        }

        #[method(searchNotes:)]
        fn search_notes(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.controller() {
                c.search_notes();
            }
        }

        #[method(openNotesFolder:)]
        fn open_notes_folder(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.controller() {
                c.open_notes_folder();
            }
        }
    }
);

impl NoteButton {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(NoteIvars {
            controller: Cell::new(std::ptr::null()),
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

    fn show_menu(&self, event: &NSEvent) {
        let mtm = MainThreadMarker::new().expect("notes menu on main thread");
        let menu = NSMenu::new(mtm);
        unsafe { menu.setTitle(&NSString::from_str("Notes")) };

        add_item(mtm, &menu, "New Note", sel!(newNote:), self);
        add_item(mtm, &menu, "Search Notes...", sel!(searchNotes:), self);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add_item(mtm, &menu, "Open TabT Notes Folder", sel!(openNotesFolder:), self);

        unsafe { NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self) };
    }

    fn render(&self) {
        let b = self.bounds();
        let s = 16.0;
        draw_symbol(
            "square.and.pencil",
            rect((b.size.width - s) / 2.0, (b.size.height - s) / 2.0, s, s),
            (107.0 / 255.0, 107.0 / 255.0, 116.0 / 255.0),
        );
    }
}

fn add_item(mtm: MainThreadMarker, menu: &NSMenu, title: &str, action: objc2::runtime::Sel, target: &NoteButton) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    unsafe {
        let _: () = msg_send![&item, setTarget: target];
    }
    menu.addItem(&item);
}

const SEARCH_W: f64 = 540.0;
const SEARCH_H: f64 = 420.0;
const SEARCH_PAD: f64 = 22.0;
const SEARCH_BOX_H: f64 = 36.0;
const RESULT_TOP: f64 = 76.0;
const RESULT_H: f64 = 48.0;

pub struct SearchPanelIvars {
    controller: Cell<*const AppController>,
    window: RefCell<Option<Retained<NSWindow>>>,
    view: RefCell<Option<Retained<NoteSearchView>>>,
}

declare_class!(
    pub struct NoteSearchPanel;

    unsafe impl ClassType for NoteSearchPanel {
        type Super = objc2::runtime::NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "NoteSearchPanel";
    }

    impl DeclaredClass for NoteSearchPanel {
        type Ivars = SearchPanelIvars;
    }

    unsafe impl NSObjectProtocol for NoteSearchPanel {}
);

impl NoteSearchPanel {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(SearchPanelIvars {
            controller: Cell::new(std::ptr::null()),
            window: RefCell::new(None),
            view: RefCell::new(None),
        });
        unsafe { msg_send_id![super(this), init] }
    }

    pub fn set_controller(&self, c: *const AppController) {
        self.ivars().controller.set(c);
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_controller(c);
        }
    }

    pub fn show(&self, mtm: MainThreadMarker) {
        if self.ivars().window.borrow().is_none() {
            self.build(mtm);
        }
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.reset();
        }
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.center();
            window.makeKeyAndOrderFront(None);
            if let Some(view) = self.ivars().view.borrow().as_ref() {
                window.makeFirstResponder(Some(view));
            }
        }
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    pub fn refresh_results(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.refresh_results();
            unsafe { view.setNeedsDisplay(true) };
        }
    }

    fn build(&self, mtm: MainThreadMarker) {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SEARCH_W, SEARCH_H));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame,
                style,
                NSBackingStoreType::NSBackingStoreBuffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Search Notes"));
        if let Some(ap) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            let _: () = unsafe { msg_send![&*window, setAppearance: &*ap] };
        }

        let view = NoteSearchView::new(mtm, frame);
        view.set_controller(self.ivars().controller.get());
        window.setContentView(Some(&view));
        *self.ivars().view.borrow_mut() = Some(view);
        *self.ivars().window.borrow_mut() = Some(window);
    }
}

pub struct SearchViewIvars {
    controller: Cell<*const AppController>,
    query: RefCell<String>,
    results: RefCell<Vec<NoteSnap>>,
    selected: Cell<usize>,
    scroll: Cell<usize>,
    font: Retained<NSFont>,
    font_small: Retained<NSFont>,
}

declare_class!(
    pub struct NoteSearchView;

    unsafe impl ClassType for NoteSearchView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "NoteSearchView";
    }

    impl DeclaredClass for NoteSearchView {
        type Ivars = SearchViewIvars;
    }

    unsafe impl NSObjectProtocol for NoteSearchView {}

    unsafe impl NoteSearchView {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            self.on_key(event);
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            self.on_mouse_down(event);
        }

        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.on_scroll(event);
        }
    }
);

impl NoteSearchView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(SearchViewIvars {
            controller: Cell::new(std::ptr::null()),
            query: RefCell::new(String::new()),
            results: RefCell::new(Vec::new()),
            selected: Cell::new(0),
            scroll: Cell::new(0),
            font: unsafe { NSFont::systemFontOfSize(13.0) },
            font_small: unsafe { NSFont::systemFontOfSize(11.0) },
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

    fn reset(&self) {
        self.ivars().query.borrow_mut().clear();
        self.refresh_results();
        unsafe { self.setNeedsDisplay(true) };
    }

    fn refresh_results(&self) {
        let results = self
            .controller()
            .map(|c| c.note_search_results(&self.ivars().query.borrow()))
            .unwrap_or_default();
        *self.ivars().results.borrow_mut() = results;
        self.ivars().selected.set(0);
        self.ivars().scroll.set(0);
    }

    fn render(&self) {
        let bounds = self.bounds();
        unsafe {
            rgba(0.075, 0.075, 0.085, 1.0).set();
            NSRectFill(bounds);
        }
        self.draw_search_box(bounds.size.width);
        self.draw_results(bounds.size.width, bounds.size.height);
    }

    fn draw_search_box(&self, w: f64) {
        let box_rect = rect(SEARCH_PAD, SEARCH_PAD, w - 2.0 * SEARCH_PAD, SEARCH_BOX_H);
        round_fill(box_rect, 9.0, &rgba(1.0, 1.0, 1.0, 0.08));
        round_stroke(box_rect, 9.0, 1.0, &rgba(240.0 / 255.0, 177.0 / 255.0, 90.0 / 255.0, 0.55));
        draw_symbol(
            "magnifyingglass",
            rect(SEARCH_PAD + 12.0, SEARCH_PAD + 11.0, 14.0, 14.0),
            (0.62, 0.62, 0.67),
        );

        let query = self.ivars().query.borrow();
        let (text, color) = if query.is_empty() {
            ("Search notes".to_string(), (0.50, 0.50, 0.56))
        } else {
            (query.clone(), (0.92, 0.92, 0.94))
        };
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(color)));
        let ns = NSString::from_str(&text);
        unsafe {
            ns.drawAtPoint_withAttributes(NSPoint::new(SEARCH_PAD + 34.0, SEARCH_PAD + 9.0), Some(&attrs));
            ns_color((0.92, 0.92, 0.94)).set();
            let caret_x = SEARCH_PAD + 34.0 + if query.is_empty() { 0.0 } else { ns.sizeWithAttributes(Some(&attrs)).width };
            NSRectFill(rect(caret_x + 2.0, SEARCH_PAD + 9.0, 1.0, 18.0));
        }
    }

    fn draw_results(&self, w: f64, h: f64) {
        let results = self.ivars().results.borrow();
        if results.is_empty() {
            let query = self.ivars().query.borrow();
            let loading = self.controller().map(|c| c.note_search_loading()).unwrap_or(false);
            let label = if loading {
                "Indexing notes..."
            } else if query.is_empty() {
                "No notes found"
            } else {
                "No matching notes"
            };
            let attrs = make_attrs(&self.ivars().font, Some(&ns_color((0.52, 0.52, 0.58))));
            unsafe {
                NSString::from_str(label).drawAtPoint_withAttributes(NSPoint::new(SEARCH_PAD, RESULT_TOP + 8.0), Some(&attrs));
            }
            return;
        }

        let max_visible = self.visible_count(h);
        let selected = self.ivars().selected.get();
        let scroll = self.ivars().scroll.get();
        for (row, hit) in results.iter().skip(scroll).take(max_visible).enumerate() {
            let idx = scroll + row;
            let y = RESULT_TOP + row as f64 * RESULT_H;
            let row_rect = rect(SEARCH_PAD - 8.0, y + 3.0, w - 2.0 * (SEARCH_PAD - 8.0), RESULT_H - 6.0);
            if idx == selected {
                round_fill(row_rect, 8.0, &rgba(1.0, 1.0, 1.0, 0.13));
                round_stroke(row_rect, 8.0, 1.0, &rgba(1.0, 1.0, 1.0, 0.17));
            }
            let fg = if idx == selected { (0.95, 0.95, 0.97) } else { (0.78, 0.78, 0.83) };
            draw_symbol("doc.text", rect(SEARCH_PAD, y + 14.0, 14.0, 15.0), fg);
            draw_truncated(
                &hit.title,
                rect(SEARCH_PAD + 22.0, y + 8.0, (w - SEARCH_PAD * 2.0 - 22.0).max(0.0), 18.0),
                &self.ivars().font,
                fg,
            );
            draw_truncated(
                &note_subtitle(&hit.path),
                rect(SEARCH_PAD + 22.0, y + 27.0, (w - SEARCH_PAD * 2.0 - 22.0).max(0.0), 15.0),
                &self.ivars().font_small,
                (0.48, 0.48, 0.54),
            );
        }
    }

    fn on_key(&self, event: &NSEvent) {
        match unsafe { event.keyCode() } {
            53 => self.close_window(),          // Esc
            36 | 76 => self.open_selected(),    // Return / Enter
            126 => self.move_selection(-1),     // Up
            125 => self.move_selection(1),      // Down
            51 => self.backspace(),             // Backspace
            _ => self.insert_event_text(event),
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    fn on_scroll(&self, event: &NSEvent) {
        let len = self.ivars().results.borrow().len();
        let visible = self.visible_count(self.bounds().size.height);
        let max_scroll = len.saturating_sub(visible);
        if max_scroll == 0 {
            return;
        }
        let dy = unsafe { event.scrollingDeltaY() };
        let mut rows = (-dy / 24.0).round() as isize;
        if rows == 0 && dy != 0.0 {
            rows = if dy < 0.0 { 1 } else { -1 };
        }
        if rows == 0 {
            return;
        }
        let next = (self.ivars().scroll.get() as isize + rows).clamp(0, max_scroll as isize) as usize;
        self.ivars().scroll.set(next);
        unsafe { self.setNeedsDisplay(true) };
    }

    fn on_mouse_down(&self, event: &NSEvent) {
        let point = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        let Some(idx) = self.result_at_y(point.y) else {
            return;
        };
        self.ivars().selected.set(idx);
        self.ensure_selected_visible();
        if unsafe { event.clickCount() } >= 2 {
            self.open_selected();
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    fn insert_event_text(&self, event: &NSEvent) {
        let Some(text) = (unsafe { event.characters() }) else {
            return;
        };
        let mut changed = false;
        {
            let mut query = self.ivars().query.borrow_mut();
            for ch in text.to_string().chars().filter(|ch| is_typable(*ch)) {
                query.push(ch);
                changed = true;
            }
        }
        if changed {
            self.refresh_results();
        }
    }

    fn backspace(&self) {
        if self.ivars().query.borrow_mut().pop().is_some() {
            self.refresh_results();
        }
    }

    fn move_selection(&self, delta: isize) {
        let len = self.ivars().results.borrow().len();
        if len == 0 {
            return;
        }
        let current = self.ivars().selected.get() as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.ivars().selected.set(next);
        self.ensure_selected_visible();
    }

    fn ensure_selected_visible(&self) {
        let visible = self.visible_count(self.bounds().size.height);
        if visible == 0 {
            self.ivars().scroll.set(0);
            return;
        }
        let selected = self.ivars().selected.get();
        let mut scroll = self.ivars().scroll.get();
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll + visible {
            scroll = selected + 1 - visible;
        }
        self.ivars().scroll.set(scroll);
    }

    fn result_at_y(&self, y: f64) -> Option<usize> {
        if y < RESULT_TOP {
            return None;
        }
        let row = ((y - RESULT_TOP) / RESULT_H).floor() as usize;
        if row >= self.visible_count(self.bounds().size.height) {
            return None;
        }
        let idx = self.ivars().scroll.get() + row;
        (idx < self.ivars().results.borrow().len()).then_some(idx)
    }

    fn visible_count(&self, h: f64) -> usize {
        ((h - RESULT_TOP - 16.0).max(0.0) / RESULT_H).floor() as usize
    }

    fn open_selected(&self) {
        let hit = self
            .ivars()
            .results
            .borrow()
            .get(self.ivars().selected.get())
            .cloned();
        if let (Some(hit), Some(ctrl)) = (hit, self.controller()) {
            ctrl.open_note_path(PathBuf::from(hit.path));
            self.close_window();
        }
    }

    fn close_window(&self) {
        if let Some(window) = self.window() {
            unsafe {
                let _: () = msg_send![&*window, orderOut: None::<&AnyObject>];
            }
        }
    }
}

pub struct NoteEditor {
    path: PathBuf,
    scroll: Retained<NSScrollView>,
    text: Retained<NSTextView>,
}

impl NoteEditor {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, path: PathBuf) -> std::io::Result<Self> {
        const MAX_NOTE_BYTES: u64 = 2 * 1024 * 1024;
        let meta = std::fs::metadata(&path)?;
        if meta.len() > MAX_NOTE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "This file is too large to edit safely in TabT.",
            ));
        }
        let bytes = std::fs::read(&path)?;
        let content = String::from_utf8(bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "This file is not valid UTF-8 plain text.",
            )
        })?;
        let scroll = unsafe { NSScrollView::initWithFrame(mtm.alloc(), frame) };
        let text_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(frame.size.width, frame.size.height));
        let text = unsafe { NSTextView::initWithFrame(mtm.alloc(), text_frame) };

        unsafe {
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.setDrawsBackground(true);
            scroll.setBackgroundColor(&NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.08, 0.09, 1.0));
            scroll.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );

            text.setString(&NSString::from_str(&content));
            text.setEditable(true);
            text.setSelectable(true);
            text.setRichText(false);
            text.setImportsGraphics(false);
            text.setDrawsBackground(true);
            text.setBackgroundColor(&NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.08, 0.09, 1.0));
            text.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(0.90, 0.90, 0.92, 1.0)));
            text.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(13.0, 0.0)));
            text.setTextContainerInset(NSSize::new(16.0, 16.0));
            text.setVerticallyResizable(true);
            text.setHorizontallyResizable(false);
            text.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
            scroll.setDocumentView(Some(&text));
        }

        Ok(Self { path, scroll, text })
    }

    pub fn view(&self) -> &NSScrollView {
        &self.scroll
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = unsafe { self.text.string() }.to_string();
        std::fs::write(&self.path, text)
    }

    pub fn focus(&self) {
        if let Some(window) = self.scroll.window() {
            window.makeFirstResponder(Some(&self.text));
        }
    }
}

fn note_subtitle(path: &str) -> String {
    PathBuf::from(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_typable(ch: char) -> bool {
    !ch.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&ch)
}

fn rgba(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a) }
}

fn round_fill(r: NSRect, radius: f64, color: &NSColor) {
    unsafe {
        color.set();
        let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
        p.fill();
    }
}

fn round_stroke(r: NSRect, radius: f64, width: f64, color: &NSColor) {
    unsafe {
        color.set();
        let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
        p.setLineWidth(width);
        p.stroke();
    }
}
