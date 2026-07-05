//! Sidebar collapse button: a system-style icon in the title bar (SF `sidebar.left`,
//! matching the "show/hide sidebar" in Finder/Mail, etc.). Its position is repositioned
//! by [`AppController::toggle_sidebar`] on toggle: when expanded it sits at the top-right
//! of the sidebar; when collapsed it moves to the top-left of the window, overlaying the terminal.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSEvent, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSRect};

use crate::app::AppController;
use crate::view::{draw_symbol, rect};

pub const TOGGLE_W: f64 = 22.0;

pub struct ToggleIvars {
    controller: Cell<*const AppController>,
}

declare_class!(
    pub struct ToggleButton;

    unsafe impl ClassType for ToggleButton {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ToggleButton";
    }

    impl DeclaredClass for ToggleButton {
        type Ivars = ToggleIvars;
    }

    unsafe impl NSObjectProtocol for ToggleButton {}

    unsafe impl ToggleButton {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(c) = self.controller() {
                c.toggle_sidebar();
            }
        }
    }
);

impl ToggleButton {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(ToggleIvars { controller: Cell::new(std::ptr::null()) });
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

    fn render(&self) {
        let b = self.bounds();
        // No background, blends into the title bar; uses the system "sidebar" icon (matching native apps, same icon for expand/collapse).
        let s = 17.0;
        draw_symbol(
            "sidebar.left",
            rect((b.size.width - s) / 2.0, (b.size.height - s) / 2.0, s, s),
            (107.0 / 255.0, 107.0 / 255.0, 116.0 / 255.0), // #6b6b74 (design spec, dimmed)
        );
    }
}
