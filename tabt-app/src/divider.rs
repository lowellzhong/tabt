//! Drag divider between the sidebar and the terminal (adjusts the sidebar width).
//!
//! A thin transparent strip (~6px) straddling the seam; dragging the mouse over it
//! changes the sidebar width. Hovering shows the left-right resize cursor. The actual
//! width change / relayout is delegated to [`AppController::set_sidebar_width`].

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSCursor, NSEvent, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSRect};

use crate::app::AppController;

pub const DIVIDER_W: f64 = 6.0;

pub struct DividerIvars {
    controller: Cell<*const AppController>,
}

declare_class!(
    pub struct Divider;

    unsafe impl ClassType for Divider {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "Divider";
    }

    impl DeclaredClass for Divider {
        type Ivars = DividerIvars;
    }

    unsafe impl NSObjectProtocol for Divider {}

    unsafe impl Divider {
        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            let x = unsafe { event.locationInWindow() }.x; // window coordinate x (left edge = 0)
            if let Some(c) = self.controller() {
                c.drag_sidebar_width(x);
            }
        }

        // On drag end, persist the new width once (no file writes during the drag).
        #[method(mouseUp:)]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(c) = self.controller() {
                c.save_layout();
            }
        }

        // Show the left-right resize cursor on hover.
        #[method(resetCursorRects)]
        fn reset_cursor_rects(&self) {
            self.addCursorRect_cursor(self.bounds(), &NSCursor::resizeLeftRightCursor());
        }
    }
);

impl Divider {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(DividerIvars { controller: Cell::new(std::ptr::null()) });
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
}
