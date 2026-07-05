//! Terminal-pane header bar (full-height layout).
//!
//! A thin bar across the top of the terminal host showing the active session name +
//! a `~ · zsh`-style meta string on the left, and a green "running" indicator on the
//! right. It also provides a clean, draggable top strip for the right pane now that the
//! window title bar is transparent/full-size. The controller pushes the title via
//! [`HeaderView::set_title`].

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSEvent, NSFont, NSRectFill, NSStringDrawing, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSString};

use crate::theme;
use crate::view::{make_attrs, ns_color, rect};

pub const HEADER_H: f64 = 40.0; // taller title bar; traffic lights are centered to match

pub struct HeaderIvars {
    title: RefCell<String>,
    font: Retained<NSFont>,     // session name (semibold)
    font_sub: Retained<NSFont>, // meta / "running"
    // Left inset for the title. When the sidebar is collapsed the traffic lights + toggle
    // sit at the window's top-left over this pane, so the title must clear them.
    left_inset: Cell<f64>,
}

declare_class!(
    pub struct HeaderView;

    unsafe impl ClassType for HeaderView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "HeaderView";
    }

    impl DeclaredClass for HeaderView {
        type Ivars = HeaderIvars;
    }

    unsafe impl NSObjectProtocol for HeaderView {}

    unsafe impl HeaderView {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        // Title-bar behavior: double-click zooms (maximize/restore) the window like the
        // native title bar; a single click/drag moves the window (the header covers the
        // title-bar strip, so AppKit's own handling never sees these clicks).
        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            let window = match self.window() {
                Some(w) => w,
                None => return,
            };
            if unsafe { event.clickCount() } >= 2 {
                window.zoom(None);
            } else {
                let _: () = unsafe { msg_send![&*window, performWindowDragWithEvent: event] };
            }
        }
    }
);

impl HeaderView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let font = unsafe { NSFont::systemFontOfSize(12.0) }; // regular weight (not bold)
        let font_sub = unsafe { NSFont::systemFontOfSize(10.5) };
        let this = mtm.alloc();
        let this = this.set_ivars(HeaderIvars {
            title: RefCell::new(String::new()),
            font,
            font_sub,
            left_inset: Cell::new(16.0),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    /// Update the active session name shown on the left.
    pub fn set_title(&self, name: &str) {
        *self.ivars().title.borrow_mut() = name.to_string();
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Set the title's left inset (larger when the sidebar is collapsed, to clear the
    /// traffic lights + toggle that now overlay this pane).
    pub fn set_left_inset(&self, x: f64) {
        if self.ivars().left_inset.get() != x {
            self.ivars().left_inset.set(x);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn render(&self) {
        let b = self.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let t = theme::current();
        unsafe {
            // Background matches the terminal exactly (theme bg); an optional 1px bottom border (Settings → Border).
            ns_color(t.bg).set();
            NSRectFill(b);
            if crate::settings::show_border() {
                ns_color(t.border()).set();
                NSRectFill(rect(0.0, h - 1.0, w, 1.0));
            }
        }

        // Colors derive from the theme so the title stays legible on light and dark themes.
        let lx = self.ivars().left_inset.get();
        let title = self.ivars().title.borrow().clone();
        // Session name (theme foreground).
        let name_attrs = make_attrs(&self.ivars().font, Some(&ns_color(t.fg)));
        let name = NSString::from_str(&title);
        let ny = (h - 16.0) / 2.0;
        unsafe { name.drawAtPoint_withAttributes(NSPoint::new(lx, ny), Some(&name_attrs)) };
        let name_w = unsafe { name.sizeWithAttributes(Some(&name_attrs)).width };

        // Meta string (`~ · zsh`) after the name (dimmed toward the background).
        let meta_attrs = make_attrs(&self.ivars().font_sub, Some(&ns_color(theme::mix(t.fg, t.bg, 0.50))));
        let meta = NSString::from_str("~ · zsh");
        unsafe { meta.drawAtPoint_withAttributes(NSPoint::new(lx + name_w + 10.0, (h - 15.0) / 2.0), Some(&meta_attrs)) };
    }
}
