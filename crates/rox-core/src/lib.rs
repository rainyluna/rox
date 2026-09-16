//! The app's data floor: the settings file and the small services
//! underneath everything else in rox. Nothing here draws anything, and
//! nothing here depends on the app above it, so the UI crate can rebuild
//! without taking the settings model with it.

pub mod acoustic;
pub mod continuation;
pub mod fmt;
pub mod install;
pub mod logging;
pub mod pace;
pub mod settings;

/// The Wayland/X11 app id, set on every window we open. Windows share it so
/// the compositor groups them as one app and, on Wayland, will consider an
/// xdg-activation request from one window to raise another (bringing an
/// already-open settings or customize window to the front). Without it the
/// backend's activate is a no-op.
pub const APP_ID: &str = "rox";

/// Play from a double-clicked row: at most this many tracks are queued
/// behind it. Every surface that plays out of a list caps the same way,
/// the quick-play modal and the stats window included.
pub const QUEUE_CAP: usize = 1000;

/// Play a view shuffled: this many tracks are drawn at random across the
/// whole view to seed the session, and continuation keeps drawing from
/// whatever the draw left. A window off the top of the view would only ever
/// shuffle the first few artists of a big library, so the seed samples the
/// list instead of slicing it.
pub const SHUFFLE_SEED: usize = 100;
