# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A native macOS terminal emulator written in Rust, built as a sequence of numbered milestones. The project is currently at milestone 0 (empty AppKit window) + 1 (standalone PTY echo loop). All copy (UI strings) and code comments/docs are in English — keep new comments in English.

Milestone roadmap (per `README.md`): step 2 = "dumb rendering" (mount a custom `NSView`, read the PTY master fd via a GCD dispatch source, feed `Grid::feed`, draw `to_lines()` with Core Text); step 3 = a real VT parser state machine inside `tabt-core`.

## Commands

All commands run from `src/` (the workspace root). Use the Makefile as the canonical entry point:

- `make test` — run `tabt-core` unit tests (`cargo test -p tabt-core`). Pure logic, runs on any platform.
- `make echo` — run the milestone-1 PTY echo loop (`cargo run --release --bin pty-echo`). **Must be run in a real terminal** (Terminal.app/iTerm); it calls `tcgetattr` on stdin and dies immediately if there is no tty (e.g. an IDE output panel).
- `make run` — build, bundle into `TabT.app`, ad-hoc codesign, and `open` it. **GUI must run as a `.app` bundle** — a bare `target/release/tabt` won't get focus or a menu bar (normal macOS behavior for non-bundled processes).
- `make bloat` — size audit (`cargo bloat`, requires `cargo install cargo-bloat`); the stripped binary should be ~1 MB.
- Single test: `cargo test -p tabt-core <test_name>` (e.g. `cargo test -p tabt-core print_and_wrap`).

The release profile is aggressively size-tuned (`opt-level="z"`, `lto`, `panic="abort"`, `strip`). `panic="abort"` means panics terminate the process rather than unwind.

## Architecture

Two-crate Cargo workspace with a strict dependency direction: `tabt-app` → `tabt-core`, never the reverse.

- **`tabt-core`** (`tabt-core/src/lib.rs`) — the terminal emulation core. **Deliberately zero-dependency** so it compiles and tests on any platform, independent of macOS. Defines `Cell`, `Color`, and `Grid` (a fixed-size screen grid stored as a flat `Vec<Cell>`, indexed `row * cols + col`). `Grid::feed(&[u8])` is currently a minimal byte handler (printable ASCII / `\n` / `\r` only; escape sequences are dropped) — it will be replaced by the full VT state machine in step 3. `to_lines()` exports rows as trimmed strings for rendering and tests.
- **`tabt-app`** (`tabt-app/`) — the macOS/AppKit layer, two binaries:
  - `src/main.rs` (bin `tabt`) — the GUI shell using `objc2` bindings. Everything runs on the main thread, enforced at the type level via `MainThreadMarker`. Note the manual memory-management detail: `setReleasedWhenClosed(false)` so Rust keeps ownership of the window and AppKit doesn't double-free it.
  - `src/bin/pty_echo.rs` (bin `pty-echo`) — a self-contained PTY loop using **raw `libc`** (no AppKit). It exercises every low-level PTY detail — `setsid`, controlling terminal, fd inheritance, raw mode, `SIGWINCH`/window-resize forwarding — that will later be lifted into the GUI. It is kept permanently as a debugging tool.

### objc2 version pinning (important)

`objc2 0.5` + `objc2-foundation 0.2` + `objc2-app-kit 0.2` are a **matched set** of APIs (newer major versions exist but are intentionally not used). If a type/method fails to compile, first check whether cargo upgraded a crate out of the set (`cargo tree | grep objc2`) and whether `objc2-app-kit`'s feature list is missing an entry — that crate splits features per Objective-C class, so each `NS*` type used must be enabled as a feature in `tabt-app/Cargo.toml`.

## Bundle

`bundle/Info.plist` is the app manifest (bundle id `dev.local.tabt`, min macOS 12.0). `make run` copies it plus the binary into `TabT.app/Contents/`.
