//! Where a workspace window sits and whether it stays above other windows:
//! read when the mini toggle leaves a layout and at persist, written back
//! when a layout returns and at launch. gpui does the work on X11, Windows
//! and macOS. Wayland gives a client neither, so there it goes through
//! KWin's scripting (`kwin.rs`), and other Wayland compositors go without.
//! So does the Flatpak on Wayland: see the trust note in `kwin.rs`.

// Some parameters only reach the KWin arms, which only Linux builds have.
#![cfg_attr(not(target_os = "linux"), allow(unused_variables))]

use gpui::{App, Pixels, Point, Size, Window};

#[cfg(target_os = "linux")]
use super::kwin;

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    Native,
    #[cfg(target_os = "linux")]
    Kwin,
    /// Wayland without a way in: windows open wherever the compositor puts
    /// them and can't be pinned.
    #[cfg(target_os = "linux")]
    None,
}

fn backend() -> Backend {
    #[cfg(target_os = "linux")]
    if gpui::guess_compositor() == "Wayland" {
        return match rox_core::install::kind() {
            rox_core::install::Kind::Flatpak => Backend::None,
            _ => Backend::Kwin,
        };
    }
    Backend::Native
}

/// Before the first window opens, so the script is loading while it maps.
pub(crate) fn start(cx: &mut App) {
    #[cfg(target_os = "linux")]
    if backend() == Backend::Kwin {
        kwin::start(cx);
    }
}

/// Whether windows can be moved and pinned here. On Wayland that waits on
/// KWin running the script, and never comes on any other compositor.
pub(crate) fn available(cx: &App) -> bool {
    match backend() {
        Backend::Native => true,
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::live(cx),
        #[cfg(target_os = "linux")]
        Backend::None => false,
    }
}

/// Whether gpui's own bounds carry a real position. Wayland's origin is
/// whatever the window was created with, so it's never worth saving.
pub(crate) fn bounds_have_origin() -> bool {
    backend() == Backend::Native
}

pub(crate) fn origin(window: &Window, cx: &App) -> Option<Point<Pixels>> {
    match backend() {
        Backend::Native => window.screen_origin(),
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::origin(window, cx),
        #[cfg(target_os = "linux")]
        Backend::None => None,
    }
}

/// The window manager's word on the pin, where it gives one, so a pin
/// changed from its own menu shows. None leaves rox's record as the truth.
pub(crate) fn pinned(window: &Window, cx: &App) -> Option<bool> {
    match backend() {
        Backend::Native => None,
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::above(window, cx),
        #[cfg(target_os = "linux")]
        Backend::None => None,
    }
}

pub(crate) fn set_pinned(window: &mut Window, pinned: bool, cx: &App) {
    match backend() {
        Backend::Native => window.set_keep_above(pinned),
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::place(window, None, None, Some(pinned), cx),
        #[cfg(target_os = "linux")]
        Backend::None => {}
    }
}

/// Move to `origin`, a point [`origin`] read. `size` is the content size a
/// resize just asked for, which the window may not have reached yet.
pub(crate) fn move_to(
    window: &mut Window,
    origin: Point<Pixels>,
    size: Option<Size<Pixels>>,
    cx: &App,
) {
    if window.is_maximized() || window.is_fullscreen() {
        return;
    }

    match backend() {
        Backend::Native => window.move_to(origin),
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::place(window, Some(origin), size, None, cx),
        #[cfg(target_os = "linux")]
        Backend::None => {}
    }
}

/// Put a just-opened workspace window back where the last one sat, pinned
/// if it was. On KWin this also ties the window to its compositor side,
/// so it runs for every workspace window, with or without a position.
pub(crate) fn restore(
    window: &mut Window,
    origin: Option<Point<Pixels>>,
    pinned: bool,
    cx: &mut App,
) {
    let origin = origin.filter(|_| !window.is_maximized() && !window.is_fullscreen());
    match backend() {
        // An X11 window manager can ignore a move for a window it hasn't
        // taken on yet, so this waits for the first frame.
        Backend::Native => {
            window.on_next_frame(move |window, _| {
                if let Some(origin) = origin {
                    window.move_to(origin);
                }
                if pinned {
                    window.set_keep_above(true);
                }
            });
        }
        #[cfg(target_os = "linux")]
        Backend::Kwin => kwin::bind(window, origin, pinned.then_some(true), cx),
        #[cfg(target_os = "linux")]
        Backend::None => {}
    }
}

pub(crate) fn forget(window: &Window, cx: &mut App) {
    #[cfg(target_os = "linux")]
    if backend() == Backend::Kwin {
        kwin::forget(window, cx);
    }
}
