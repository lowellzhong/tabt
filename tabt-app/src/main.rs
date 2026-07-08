//! macOS shell + multi-tab terminal.
//!
//! The left of the window is a custom-drawn sidebar (groups + tabs, see `sidebar.rs`), the right is the terminal container host.
//! [`AppController`](app::AppController) manages multiple sessions: one PTY + one
//! `TermView` per tab, the active tab's view is mounted into the host. Group layout is persisted in ~/Documents/TabT/AppData (see `config.rs`).
//!
//! Note: must be run as a .app bundle (see the Makefile's `make run`).

#![allow(unexpected_cfgs)] // the objc2 macros trigger this lint on some versions

mod app;
mod config;
mod divider;
mod header;
mod menu;
mod note;
mod placeholder;
mod pty;
mod settings;
mod settings_dialog;
mod sidebar;
mod theme;
mod toggle;
mod view;

use objc2::msg_send;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use divider::DIVIDER_W;
use sidebar::{SidebarView, SIDEBAR_W};
use toggle::{ToggleButton, TOGGLE_W};

const CONTENT_W: f64 = 1000.0;
const CONTENT_H: f64 = 620.0;

fn main() {
    // The child shell relies on TERM to recognize the terminal type; set it before spawning any thread, so forked children inherit it directly.
    std::env::set_var("TERM", "xterm-256color");
    // Present TabT's own terminal identity instead of inheriting whatever launched us. When TabT is
    // started from Apple Terminal (or a bare binary in one), the shell exports TERM_PROGRAM=Apple_Terminal
    // plus a TERM_SESSION_ID UUID; a child zsh would then source /etc/zshrc_Apple_Terminal and use that
    // *inherited, shared* session id to manage ~/.zsh_sessions/<uuid>.session — so every tab (and the real
    // Terminal.app) fights over one file, printing "rm: …session: No such file or directory". Overriding
    // TERM_PROGRAM (!= Apple_Terminal) disables that script; clearing the stale session id/version avoids
    // leaking another program's identity to the shell. Done in the parent (not post-fork) to stay clear of
    // async-signal-safety limits, and before AppKit starts any thread so every child inherits the clean env.
    std::env::set_var("TERM_PROGRAM", "TabT");
    std::env::remove_var("TERM_PROGRAM_VERSION");
    std::env::remove_var("TERM_SESSION_ID");

    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // ---- Font settings (default Menlo; bootstrap overrides it from ~/Documents/TabT/AppData) ----
    settings::set(settings::DEFAULT_FAMILY, settings::DEFAULT_SIZE);

    // ---- Window ----
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CONTENT_W, CONTENT_H));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect,
            style,
            NSBackingStoreType::NSBackingStoreBuffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    // Disable window state restoration, otherwise macOS overrides this contentRect with the last saved window size.
    let _: () = unsafe { msg_send![&*window, setRestorable: false] };
    window.setTitle(&NSString::from_str("TabT"));
    // Full-height content: transparent title bar + hidden title so the content view spans the
    // whole window and the traffic lights float over the sidebar's top-left.
    window.setTitlebarAppearsTransparent(true);
    let _: () = unsafe { msg_send![&*window, setTitleVisibility: 1isize] }; // NSWindowTitleVisibilityHidden

    // ---- Layout: container = left sidebar + right terminal host ----
    let container: objc2::rc::Retained<NSView> = unsafe { NSView::initWithFrame(mtm.alloc(), rect) };

    let sidebar_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIDEBAR_W, CONTENT_H));
    let sidebar = SidebarView::new(mtm, sidebar_frame);
    let host_frame = NSRect::new(
        NSPoint::new(SIDEBAR_W, 0.0),
        NSSize::new(CONTENT_W - SIDEBAR_W, CONTENT_H),
    );
    let host: objc2::rc::Retained<NSView> = unsafe { NSView::initWithFrame(mtm.alloc(), host_frame) };

    // Draggable divider bar: sits astride the sidebar/terminal seam.
    let divider = divider::Divider::new(
        mtm,
        NSRect::new(
            NSPoint::new(SIDEBAR_W - DIVIDER_W / 2.0, 0.0),
            NSSize::new(DIVIDER_W, CONTENT_H),
        ),
    );

    // Collapse button: a floating view repositioned by the controller (sidebar top-right when
    // expanded, window top-left over the terminal when collapsed).
    let toggle_btn = ToggleButton::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(TOGGLE_W + 12.0, TOGGLE_W)));

    unsafe {
        // Sidebar: fixed width, pinned left, resizes with height.
        sidebar.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewHeightSizable
                | NSAutoresizingMaskOptions::NSViewMaxXMargin,
        );
        // Terminal host: fills the rest, resizes with width and height.
        host.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewWidthSizable
                | NSAutoresizingMaskOptions::NSViewHeightSizable,
        );
        // Divider bar: fixed on the seam (HeightSizable resizes with height, MaxXMargin lets its right side track width).
        divider.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewHeightSizable
                | NSAutoresizingMaskOptions::NSViewMaxXMargin,
        );
        // Toggle: pinned to the top, left-anchored (the controller sets its exact frame in relayout).
        toggle_btn.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewMinYMargin | NSAutoresizingMaskOptions::NSViewMaxXMargin,
        );
        container.addSubview(&sidebar);
        container.addSubview(&host);
        container.addSubview(&divider); // above the seam, takes over dragging
        container.addSubview(&toggle_btn); // topmost, floats in the title-bar zone
    }
    window.setContentView(Some(&container));

    // ---- Controller: load layout, bring up sessions ----
    let controller = app::AppController::new(
        mtm,
        window.clone(),
        sidebar.clone(),
        host.clone(),
        toggle_btn.clone(),
        divider.clone(),
    );
    controller.bootstrap();

    // ---- Menu bar + shortcuts (target must stay alive for the app's runtime) ----
    let menu_target = menu::MenuTarget::new(mtm);
    menu_target.set_controller(std::rc::Rc::as_ptr(&controller));
    menu::build_menu(mtm, &app, &menu_target);
    // The same object serves as both the app delegate (save on quit) and the window delegate
    // (re-center traffic lights on resize).
    let _: () = unsafe { msg_send![&*app, setDelegate: &*menu_target] };
    let _: () = unsafe { msg_send![&*window, setDelegate: &*menu_target] };

    window.center();
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    controller.reposition_traffic_lights(); // center the traffic lights in the taller title bar

    // controller (Rc) and menu_target must live until the event loop ends; app.run() never returns.
    let _controller = controller;
    let _menu_target = menu_target;
    unsafe { app.run() };
}
