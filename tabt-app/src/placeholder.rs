//! Empty-state placeholder for the terminal pane, shown when there are no open sessions
//! (e.g. after ⌘W closes the last tab). Fills the theme background so the pane stays visually
//! consistent, and shows a hint on how to create a new terminal/group.

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSFont, NSRectFill, NSStringDrawing, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSString};

use crate::theme;
use crate::view::{make_attrs, ns_color};

declare_class!(
    pub struct PlaceholderView;

    unsafe impl ClassType for PlaceholderView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "PlaceholderView";
    }

    impl DeclaredClass for PlaceholderView {
        type Ivars = ();
    }

    unsafe impl NSObjectProtocol for PlaceholderView {}

    unsafe impl PlaceholderView {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }
    }
);

impl PlaceholderView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(());
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    fn render(&self) {
        let b = self.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let t = theme::current();
        unsafe {
            ns_color(t.bg).set();
            NSRectFill(b);
        }

        // Colors derive from the theme so the hint reads on light and dark alike.
        let title_col = theme::mix(t.fg, t.bg, 0.45);
        let hint_col = theme::mix(t.fg, t.bg, 0.58);
        let title_font = unsafe { NSFont::systemFontOfSize(15.0) };
        let hint_font = unsafe { NSFont::systemFontOfSize(13.0) };

        let cx = w / 2.0;
        let cy = h / 2.0;
        self.draw_centered("No open sessions", cx, cy - 46.0, &title_font, title_col);
        self.draw_centered("New Terminal      ⌘T", cx, cy - 6.0, &hint_font, hint_col);
        self.draw_centered("New Group      ⇧⌘N", cx, cy + 22.0, &hint_font, hint_col);
    }

    /// Draw `text` horizontally centered at `cx`, with its top at `y`.
    fn draw_centered(&self, text: &str, cx: f64, y: f64, font: &Retained<NSFont>, color: (f64, f64, f64)) {
        let attrs = make_attrs(font, Some(&ns_color(color)));
        let ns = NSString::from_str(text);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(cx - tw / 2.0, y), Some(&attrs)) };
    }
}
