//! Settings dialog: a small native panel with standard AppKit controls (theme + font
//! family pop-ups, a font-size stepper/field, and a "sidebar on right" checkbox).
//!
//! It replaces the old bottom-of-sidebar pop-up menu. The controls read/write through the
//! [`AppController`], so every change applies live and is persisted immediately. The panel
//! object is kept alive by the controller (it holds the `Retained<SettingsDialog>`); the
//! dialog holds only a raw pointer back to the controller, so there is no reference cycle.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSPopUpButton,
    NSStepper, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::app::AppController;
use crate::settings;
use crate::theme;

const W: f64 = 340.0;
const H: f64 = 228.0;
const LABEL_X: f64 = 22.0;
const CTRL_X: f64 = 96.0;
const CTRL_W: f64 = 200.0;

pub struct DialogIvars {
    controller: Cell<*const AppController>,
    window: RefCell<Option<Retained<NSWindow>>>,
    theme_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    fam_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    size_field: RefCell<Option<Retained<NSTextField>>>,
    size_stepper: RefCell<Option<Retained<NSStepper>>>,
    side_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    border_pop: RefCell<Option<Retained<NSPopUpButton>>>,
}

declare_class!(
    pub struct SettingsDialog;

    unsafe impl ClassType for SettingsDialog {
        type Super = objc2::runtime::NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "SettingsDialog";
    }

    impl DeclaredClass for SettingsDialog {
        type Ivars = DialogIvars;
    }

    unsafe impl NSObjectProtocol for SettingsDialog {}

    unsafe impl SettingsDialog {
        #[method(themeChanged:)]
        fn theme_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let Some(c) = self.controller() {
                c.set_style(idx);
            }
        }

        #[method(fontFamilyChanged:)]
        fn font_family_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let Some(c) = self.controller() {
                c.set_font_family(idx);
            }
        }

        // Stepper arrows: push the new value into the text field, then apply.
        #[method(sizeStepped:)]
        fn size_stepped(&self, sender: &NSStepper) {
            let v = unsafe { sender.doubleValue() };
            if let Some(f) = self.ivars().size_field.borrow().as_ref() {
                unsafe { f.setStringValue(&NSString::from_str(&format!("{}", v as i64))) };
            }
            self.apply_size(v);
        }

        // Text field edited (Enter): clamp, sync the stepper, then apply.
        #[method(sizeEdited:)]
        fn size_edited(&self, sender: &NSTextField) {
            let v = unsafe { sender.doubleValue() }.clamp(8.0, 40.0);
            unsafe { sender.setStringValue(&NSString::from_str(&format!("{}", v as i64))) };
            if let Some(s) = self.ivars().size_stepper.borrow().as_ref() {
                unsafe { s.setDoubleValue(v) };
            }
            self.apply_size(v);
        }

        #[method(sidebarChanged:)]
        fn sidebar_changed(&self, sender: &NSPopUpButton) {
            let on_right = unsafe { sender.indexOfSelectedItem() } == 1; // 0 = Left, 1 = Right
            if let Some(c) = self.controller() {
                c.set_sidebar_side(on_right);
            }
        }

        #[method(borderChanged:)]
        fn border_changed(&self, sender: &NSPopUpButton) {
            let shown = unsafe { sender.indexOfSelectedItem() } == 1; // 0 = Hidden, 1 = Shown
            if let Some(c) = self.controller() {
                c.set_show_border(shown);
            }
        }
    }
);

impl SettingsDialog {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(DialogIvars {
            controller: Cell::new(std::ptr::null()),
            window: RefCell::new(None),
            theme_pop: RefCell::new(None),
            fam_pop: RefCell::new(None),
            size_field: RefCell::new(None),
            size_stepper: RefCell::new(None),
            side_pop: RefCell::new(None),
            border_pop: RefCell::new(None),
        });
        unsafe { msg_send_id![super(this), init] }
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

    fn apply_size(&self, size: f64) {
        if let Some(c) = self.controller() {
            c.set_font_size(size);
        }
    }

    /// Build the panel (if needed) and bring it to front, seeded with the current settings.
    pub fn show(&self, mtm: MainThreadMarker) {
        // Already built: just refresh values and re-show.
        if self.ivars().window.borrow().is_some() {
            self.seed_values();
            if let Some(w) = self.ivars().window.borrow().as_ref() {
                w.center();
                w.makeKeyAndOrderFront(None);
            }
            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            return;
        }

        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, H));
        let window: Retained<NSWindow> = unsafe {
            msg_send_id![
                mtm.alloc::<NSWindow>(),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::NSBackingStoreBuffered,
                defer: false,
            ]
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Settings"));
        // Dark appearance so the standard controls render to match the app's dark UI.
        if let Some(ap) = objc2_app_kit::NSAppearance::appearanceNamed(unsafe {
            objc2_app_kit::NSAppearanceNameDarkAqua
        }) {
            let _: () = unsafe { msg_send![&*window, setAppearance: &*ap] };
        }

        let content = window.contentView().expect("content view");

        // Row y-positions (content view is non-flipped: y grows upward).
        let theme_y = H - 52.0;
        let font_y = theme_y - 38.0;
        let size_y = font_y - 38.0;
        let side_y = size_y - 40.0;
        let border_y = side_y - 38.0;

        // ---- Theme pop-up ----
        add_label(&content, "Theme", theme_y, mtm);
        let theme_pop = self.make_popup(theme::NAMES.iter().copied(), theme_y - 3.0, sel!(themeChanged:), mtm);
        unsafe { content.addSubview(&theme_pop) };

        // ---- Font family pop-up ----
        add_label(&content, "Font", font_y, mtm);
        let fam_titles = settings::FAMILIES
            .iter()
            .map(|f| if *f == "system" { "System Monospace" } else { *f });
        let fam_pop = self.make_popup(fam_titles, font_y - 3.0, sel!(fontFamilyChanged:), mtm);
        unsafe { content.addSubview(&fam_pop) };

        // ---- Font size: editable field + stepper ----
        add_label(&content, "Size", size_y, mtm);
        let field: Retained<NSTextField> = unsafe {
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: NSRect::new(
                NSPoint::new(CTRL_X, size_y - 2.0), NSSize::new(52.0, 22.0))]
        };
        unsafe {
            field.setEditable(true);
            field.setBezeled(true);
            let _: () = msg_send![&field, setTarget: self];
            field.setAction(Some(sel!(sizeEdited:)));
        }
        let stepper: Retained<NSStepper> = unsafe {
            msg_send_id![mtm.alloc::<NSStepper>(), initWithFrame: NSRect::new(
                NSPoint::new(CTRL_X + 58.0, size_y - 3.0), NSSize::new(19.0, 25.0))]
        };
        unsafe {
            stepper.setMinValue(8.0);
            stepper.setMaxValue(40.0);
            stepper.setIncrement(1.0);
            stepper.setValueWraps(false);
            let _: () = msg_send![&stepper, setTarget: self];
            stepper.setAction(Some(sel!(sizeStepped:)));
        }
        unsafe { content.addSubview(&field) };
        unsafe { content.addSubview(&stepper) };
        *self.ivars().size_field.borrow_mut() = Some(field);
        *self.ivars().size_stepper.borrow_mut() = Some(stepper);

        // ---- Sidebar position pop-up (Left / Right) ----
        add_label(&content, "Sidebar", side_y, mtm);
        let side_pop = self.make_popup(["Left", "Right"].into_iter(), side_y - 3.0, sel!(sidebarChanged:), mtm);
        unsafe { content.addSubview(&side_pop) };

        // ---- Border visibility pop-up (Hidden / Shown) ----
        add_label(&content, "Border", border_y, mtm);
        let border_pop = self.make_popup(["Hidden", "Shown"].into_iter(), border_y - 3.0, sel!(borderChanged:), mtm);
        unsafe { content.addSubview(&border_pop) };

        // No explicit Done button: the window's title-bar close button dismisses the panel.

        // Remember control references we need to re-seed later.
        *self.ivars().theme_pop.borrow_mut() = Some(theme_pop);
        *self.ivars().fam_pop.borrow_mut() = Some(fam_pop);
        *self.ivars().side_pop.borrow_mut() = Some(side_pop);
        *self.ivars().border_pop.borrow_mut() = Some(border_pop);

        *self.ivars().window.borrow_mut() = Some(window.clone());
        self.seed_values();

        window.center();
        window.makeKeyAndOrderFront(None);
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    /// Build a pop-up button filled with `titles`, wired to `action`, positioned at `y`.
    fn make_popup<'a>(
        &self,
        titles: impl Iterator<Item = &'a str>,
        y: f64,
        action: objc2::runtime::Sel,
        mtm: MainThreadMarker,
    ) -> Retained<NSPopUpButton> {
        let frame = NSRect::new(NSPoint::new(CTRL_X, y), NSSize::new(CTRL_W, 26.0));
        let pop: Retained<NSPopUpButton> =
            unsafe { msg_send_id![mtm.alloc::<NSPopUpButton>(), initWithFrame: frame, pullsDown: false] };
        for t in titles {
            unsafe { pop.addItemWithTitle(&NSString::from_str(t)) };
        }
        unsafe {
            let _: () = msg_send![&pop, setTarget: self];
            pop.setAction(Some(action));
        }
        pop
    }

    /// Re-read the current settings into the controls.
    fn seed_values(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let store = self.ivars();
        if let Some(pop) = store.theme_pop.borrow().as_ref() {
            unsafe { pop.selectItemAtIndex(ctrl.snapshot().style as isize) };
        }
        if let Some(pop) = store.fam_pop.borrow().as_ref() {
            let cur = settings::family();
            if let Some(i) = settings::FAMILIES.iter().position(|f| *f == cur) {
                unsafe { pop.selectItemAtIndex(i as isize) };
            }
        }
        let size = settings::size();
        if let Some(f) = store.size_field.borrow().as_ref() {
            unsafe { f.setStringValue(&NSString::from_str(&format!("{}", size as i64))) };
        }
        if let Some(s) = store.size_stepper.borrow().as_ref() {
            unsafe { s.setDoubleValue(size) };
        }
        if let Some(p) = store.side_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(if ctrl.sidebar_on_right() { 1 } else { 0 }) };
        }
        if let Some(p) = store.border_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(if settings::show_border() { 1 } else { 0 }) };
        }
    }
}

/// A non-editable, borderless label added to `content` at `y`.
fn add_label(content: &NSView, text: &str, y: f64, mtm: MainThreadMarker) {
    let label = unsafe { NSTextField::labelWithString(&NSString::from_str(text), mtm) };
    unsafe {
        label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.72, 0.72, 0.76, 1.0,
        )));
        label.setFrame(NSRect::new(NSPoint::new(LABEL_X, y - 1.0), NSSize::new(64.0, 18.0)));
        content.addSubview(&label);
    }
}
