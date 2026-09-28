//! The waveform peak cache's location. The format and the reads and writes
//! live in [`rox_library::peaks`]. A track with no file is keyed on its
//! source and key, and its waveform is decoded from the download that plays
//! it ([`rox_playback::download`]).

use std::path::{Path, PathBuf};

use rox_core::settings;

pub use rox_library::peaks::{PeakBin, PeakLanes, identity};

pub fn cache_dir() -> PathBuf {
    settings::data_dir().join("waveforms")
}

/// Blocking; run off the UI thread.
pub fn clear() {
    rox_library::peaks::clear(&cache_dir());
}

pub fn load(track: &Path) -> Option<PeakLanes> {
    rox_library::peaks::load(&cache_dir(), track)
}

/// Stamped with the identity the track had going into the decode.
pub fn store(track: &Path, stamped: Option<(u64, u64)>, lanes: &[Vec<PeakBin>]) {
    rox_library::peaks::store(&cache_dir(), track, stamped, lanes);
}

/// Blocking; run off the UI thread.
pub fn load_remote(source: &str, key: &str) -> Option<PeakLanes> {
    rox_library::peaks::load_remote(&cache_dir(), source, key)
}

/// Blocking; run off the UI thread.
pub fn store_remote(source: &str, key: &str, lanes: &[Vec<PeakBin>]) {
    rox_library::peaks::store_remote(&cache_dir(), source, key, lanes);
}
