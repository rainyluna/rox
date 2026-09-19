//! The table a remote track's request headers come out of. A row in the
//! library says where a track's bytes live (`remote_url`) and whether the
//! stream ever ends, but not how to authorize the request, because the
//! answer changes per session: a Subsonic token is salted per call, a
//! bearer expires, and a password has no business sitting in a database
//! file the user can copy off the machine.
//!
//! So the store hands back a locator with empty headers and something asks
//! the live source to fill them in. That something can't be the store: the
//! library crate knows nothing about a source object, and a source lives a
//! layer above it. The shape is `rox-panel-api`'s `openers` table, for the
//! same reason: the binary fills it once at startup and everything below
//! calls through it, so the lower layer never calls up.
//!
//! Header values are credentials. They're never logged and never written
//! to disk, here or anywhere downstream of here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// What a source answers when asked to authorize a request. Called on
/// whatever thread is resolving a queue, so it has to be cheap and it has
/// to be safe to call from more than one at once.
pub type HeaderFn = Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

fn table() -> &'static Mutex<HashMap<String, HeaderFn>> {
    static TABLE: OnceLock<Mutex<HashMap<String, HeaderFn>>> = OnceLock::new();

    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the header builder for one source. Installed when a source
/// comes up and replaced when its settings change, so a re-login swaps the
/// credentials without anything else noticing.
pub fn install(source: &str, f: HeaderFn) {
    if let Ok(mut table) = table().lock() {
        table.insert(source.to_string(), f);
    }
}

/// Forget a source's headers, for a source the user turned off. A locator
/// resolved after this carries none, so the request goes out bare and the
/// server refuses it, which is the honest outcome.
pub fn forget(source: &str) {
    if let Ok(mut table) = table().lock() {
        table.remove(source);
    }
}

/// The headers to send for this source, empty for one nothing registered.
/// An empty answer is not an error: local tracks never go through here, and
/// a source that needs no authorization has nothing to add.
pub fn headers_for(source: &str) -> Vec<(String, String)> {
    match table().lock() {
        Ok(table) => table.get(source).map(|f| f()).unwrap_or_default(),

        // A poisoned lock means a header builder panicked. Nothing here is
        // worth taking the app down over; the request goes out bare.
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_reads_back_what_it_installed() {
        install(
            "test:read-back",
            Box::new(|| vec![("Authorization".to_string(), "Bearer abc".to_string())]),
        );

        assert_eq!(
            headers_for("test:read-back"),
            vec![("Authorization".to_string(), "Bearer abc".to_string())]
        );

        forget("test:read-back");
        assert!(headers_for("test:read-back").is_empty());
    }

    #[test]
    fn an_unknown_source_has_no_headers() {
        assert!(headers_for("test:never-installed").is_empty());
    }
}
