//! A station's own picture: its logo, fetched once and filed in the
//! thumbnail store under the station's URL, which is the row's path and so
//! the key every surface already asks art by. Nothing here knows what a
//! panel looks like; it downloads bytes and writes them where
//! [`rox_library::thumbs`] will find them again.
//!
//! Two callers, one path. The station directory has a logo URL out of
//! radio-browser and files it the moment a hit is added, and a station
//! played from anywhere else has only what its own headers said, which is
//! a homepage at best. Both end in the same store write, and having them
//! as two copies is how the two drift: one caps the download and the other
//! doesn't, one checks the content type and the other stores a login page
//! as a cover.
//!
//! This is the opposite discipline from [`crate::radio_art`], which holds
//! a guessed cover for the song on air in memory and never writes it. A
//! station's logo is the station's, it doesn't change between songs, and
//! it came from a link the station itself published, so it's worth keeping
//! on disk. Everything here is best effort and silent: a dead logo link is
//! an ordinary answer, not a failure anyone needs telling about.

use std::io::Read;
use std::sync::Mutex;

use rox_library::rusqlite::Connection;

/// The biggest logo worth storing. A favicon is a few kilobytes and a
/// station's own PNG is tens of them; past this the URL is answering with
/// something that isn't a picture, whatever its content type claims.
pub const MAX_BYTES: u64 = 512 * 1024;

/// The path a site's icon sits at when nothing else names one. Stations
/// publish a homepage in `icy-url` and never a logo, so this is the one
/// guess available, and it's the guess every browser makes too.
const FAVICON_PATH: &str = "/favicon.ico";

/// Download a station's logo over the shared provider agent, which carries
/// the app User-Agent and its ten second timeout. Anything that isn't a
/// plain image answer is dropped rather than stored: a logo URL is
/// something a station's owner typed years ago, so a login page, a
/// redirect to a parked domain, or a megabyte of HTML are all ordinary
/// answers here.
///
/// Blocking. Background executor only.
pub fn fetch(url: &str) -> Option<Vec<u8>> {
    let response = rox_net::providers::agent().get(url).call().ok()?;
    if !(200..300).contains(&response.status()) {
        return None;
    }
    if !response.content_type().starts_with("image/") {
        return None;
    }

    // One byte past the cap, so a body that fills it is over the limit
    // rather than silently truncated into a corrupt image.
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;

    (!bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES).then_some(bytes)
}

/// Fetch `image_url` and file it under `key`, the station's stream URL.
/// True when something landed, which is the caller's cue to forget the
/// key in the texture cache: a row that painted before this ran was told
/// there was no art, and that answer is cached as definitive.
///
/// Blocking, and it takes the store lock. Background executor only.
pub fn fetch_and_store(image_url: &str, key: &str, conn: &Mutex<Connection>) -> bool {
    let Some(bytes) = fetch(image_url) else {
        return false;
    };
    let Ok(conn) = conn.lock() else {
        return false;
    };

    rox_library::thumbs::store_bytes(&conn, &bytes, key).is_some()
}

/// Where a station's logo would live given only its homepage: the origin
/// with [`FAVICON_PATH`] on it. None for anything that isn't an http URL
/// with a host, which is most of what ends up in `icy-url` on a station
/// that fills the field in with a slogan.
///
/// Hand-parsed rather than through a URL crate: this is a scheme, a host
/// and an optional port, and the one thing that would justify the
/// dependency (percent-encoding, query strings, relative resolution) is
/// exactly what gets thrown away here.
pub fn favicon_url(homepage: &str) -> Option<String> {
    let homepage = homepage.trim();
    let (scheme, rest) = homepage.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    // The authority is everything up to the first path, query or fragment.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('.');

    // A userinfo half would make the guess point at a credentialed URL,
    // which is not a link worth fetching unasked.
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    Some(format!(
        "{}://{authority}{FAVICON_PATH}",
        scheme.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guess is the origin and nothing else: a path, a query and a
    /// fragment all belong to a page, not to the site's icon.
    #[test]
    fn a_homepage_reduces_to_its_origin() {
        assert_eq!(
            favicon_url("https://jazzforever.example/schedule?day=2"),
            Some("https://jazzforever.example/favicon.ico".to_string())
        );
        assert_eq!(
            favicon_url("http://radio.example:8000"),
            Some("http://radio.example:8000/favicon.ico".to_string())
        );
        assert_eq!(
            favicon_url("  HTTPS://Radio.Example/  "),
            Some("https://Radio.Example/favicon.ico".to_string())
        );
    }

    /// `icy-url` is a free text field and stations put anything in it.
    /// Nothing that isn't an ordinary web address gets fetched.
    #[test]
    fn anything_that_is_not_a_web_address_is_no_guess_at_all() {
        assert_eq!(favicon_url(""), None);
        assert_eq!(favicon_url("The best jazz on the internet"), None);
        assert_eq!(favicon_url("ftp://files.example/logo.png"), None);
        assert_eq!(favicon_url("https://"), None);
        assert_eq!(favicon_url("https://user:pass@radio.example/"), None);
    }
}
