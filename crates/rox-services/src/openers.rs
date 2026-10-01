//! The one slot a plugin stream opener sits in, beside
//! [`sources_registry`](crate::sources_registry). The plugin host installs
//! into it, and each queue the player starts takes a clone along to the
//! engine, so `rox-playback` never calls up into the host (ADR 29, ADR 30).
//!
//! Empty, a plugin entry refuses to open with "no plugin opener".

use std::sync::LazyLock;
use std::sync::RwLock;

use rox_playback::plugin::Opener;

static SLOT: LazyLock<RwLock<Option<Opener>>> = LazyLock::new(|| RwLock::new(None));

/// Replaces whatever was installed. Queues already playing keep the opener
/// they started with.
pub fn install(opener: Opener) {
    if let Ok(mut slot) = SLOT.write() {
        *slot = Some(opener);
    }
}

pub fn clear() {
    if let Ok(mut slot) = SLOT.write() {
        *slot = None;
    }
}

/// A clone, so no caller ever runs the opener under the lock.
pub fn current() -> Option<Opener> {
    SLOT.read().ok().and_then(|slot| slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rox_library::locator::PluginStream;

    #[test]
    fn the_slot_hands_back_what_was_installed_until_cleared() {
        install(Arc::new(|stream: &PluginStream| {
            Err(format!("asked for {}", stream.key))
        }));

        let stream = PluginStream {
            source: "plugin:demo".into(),
            key: "a.flac".into(),
            live: false,
            duration_ms: None,
        };
        let opener = current().expect("installed");
        assert_eq!(opener(&stream).err().as_deref(), Some("asked for a.flac"));

        clear();
        assert!(current().is_none());
    }
}
