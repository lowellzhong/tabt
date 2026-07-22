//! Empty-state placeholder for the terminal pane, shown when there are no open sessions
//! (e.g. after ⌘W closes the last tab). Fills the theme background so the pane stays visually
//! consistent, and shows a hint on how to create a new terminal/group.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSCompositingOperation, NSFont, NSRectFill, NSStringDrawing, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSZeroRect,
};

use crate::theme;
use crate::view::{make_attrs, ns_color};

/// App-icon size in the empty state, and its gap above the title.
const LOGO: f64 = 64.0;
const LOGO_GAP: f64 = 16.0;

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
        // The logo adds LOGO + LOGO_GAP above the former title position, so shift the whole block
        // down by half of that to keep it optically centered in the pane.
        let shift = (LOGO + LOGO_GAP) / 2.0;
        self.draw_logo(cx, cy - 46.0 - LOGO - LOGO_GAP + shift);
        // The product name rather than a state description ("No open sessions"): this view is only
        // ever shown when there is nothing to show, so naming the state adds nothing, and the name
        // keeps reading right whatever empty state routes here.
        self.draw_centered("TabT", cx, cy - 46.0 + shift, &title_font, title_col);
        self.draw_centered("New Terminal      ⌘T", cx, cy - 6.0 + shift, &hint_font, hint_col);
        self.draw_centered("New Group      ⇧⌘N", cx, cy + 22.0 + shift, &hint_font, hint_col);
    }

    /// Draw the app icon horizontally centered at `cx`, with its top at `y`.
    ///
    /// Sourced from the running application rather than a bundled copy, so it always matches the
    /// icon in the Dock and there is no second asset to keep in sync. An unbundled run (the app is
    /// normally launched as TabT.app) just gets the generic executable icon, which is harmless.
    fn draw_logo(&self, cx: f64, y: f64) {
        let mtm = MainThreadMarker::from(self);
        let Some(icon) = (unsafe { NSApplication::sharedApplication(mtm).applicationIconImage() })
        else {
            return;
        };
        let r = NSRect::new(NSPoint::new(cx - LOGO / 2.0, y), NSSize::new(LOGO, LOGO));
        // This view is flipped, and plain `drawInRect:` draws in the context's coordinate system —
        // which lands the icon upside down. `respectFlipped:` is what re-orients it (the text above
        // goes through NSString drawing, which already handles this). That variant is not wrapped
        // by objc2-app-kit 0.2, so it goes out as a raw message rather than moving off the pinned
        // objc2 set for one selector.
        let hints: *const AnyObject = std::ptr::null();
        unsafe {
            let _: () = msg_send![
                &*icon,
                drawInRect: r,
                fromRect: NSZeroRect,
                operation: NSCompositingOperation::SourceOver,
                fraction: 1.0f64,
                respectFlipped: true,
                hints: hints,
            ];
        }
    }

    /// Draw `text` horizontally centered at `cx`, with its top at `y`.
    fn draw_centered(&self, text: &str, cx: f64, y: f64, font: &Retained<NSFont>, color: (f64, f64, f64)) {
        let attrs = make_attrs(font, Some(&ns_color(color)));
        let ns = NSString::from_str(text);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(cx - tw / 2.0, y), Some(&attrs)) };
    }
}
