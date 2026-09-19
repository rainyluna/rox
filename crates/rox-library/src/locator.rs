//! Where a track's bytes come from. Until sources, that answer was always a
//! file on disk and a `PathBuf` carried it everywhere, from the queue down
//! into the decoder's `File::open`. A source that serves its catalog over
//! HTTP breaks that: the bytes arrive over the wire, the request needs
//! credentials the source holds, and a URL carries no extension for the
//! probe to read a container off.
//!
//! The type lives in `rox-library` because `rox-playback` depends on this
//! crate and nothing else, so the store side and the engine side share one
//! definition instead of converting across a seam.
//!
//! `path()` answering `Option` is the point of the module, not a
//! convenience. Every path-only operation in the app (the tag writer,
//! rename, convert, ReplayGain measurement, fingerprinting, the decode
//! window behind the visualizer) takes a `&Path` and has no meaning for a
//! track that isn't a file. Making the accessor fallible means a remote
//! track can't reach any of them without someone writing the match and
//! deciding what happens instead.

use std::path::Path;
use std::path::PathBuf;

/// Where a track's bytes come from. `Local` is a file on disk, the first
/// source and the only one before this feature. `Remote` is an HTTP URL the
/// transport opens, with whatever headers the source needs to authorize it
/// and a container hint for the probe, since a URL carries no extension.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Locator {
    Local(PathBuf),
    Remote(Remote),
}

/// The remote half: everything the transport needs to open one stream.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Remote {
    pub url: String,
    /// Header name and value pairs, sent on every request for this track,
    /// range retries included.
    pub headers: Vec<(String, String)>,
    /// Container extension without the dot ("flac", "mp3"), for the probe
    /// hint. Empty means let the transport read it off Content-Type.
    pub hint: String,
    /// The stream has no end and no seek: internet radio. The transport
    /// wraps it unseekable and the engine reports no duration.
    pub live: bool,
}

impl Locator {
    /// The file behind a local track, None for a remote one. Every path-only
    /// operation (the tag writer, rename, convert, ReplayGain measurement,
    /// fingerprinting, a decode window) goes through this and decides for
    /// itself what to do with None.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Locator::Local(path) => Some(path.as_path()),

            Locator::Remote(_) => None,
        }
    }

    /// Display fallback when tags are missing: the file name for a local
    /// track, the last URL segment for a remote one. A stream URL that ends
    /// in a slash or carries a query string still has to show something, so
    /// the query comes off first and the host stands in when no segment
    /// survives.
    pub fn label(&self) -> String {
        match self {
            Locator::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),

            Locator::Remote(remote) => remote_label(&remote.url),
        }
    }
}

impl From<PathBuf> for Locator {
    fn from(path: PathBuf) -> Self {
        Locator::Local(path)
    }
}

/// The last meaningful piece of a URL. Scheme and query are noise for a
/// label, and a trailing slash means the segment before it is the name.
fn remote_label(url: &str) -> String {
    // Drop the fragment and the query, neither of which names the stream.
    let trimmed = url.split(['#', '?']).next().unwrap_or(url);

    // Past the scheme, the first segment is the host: the fallback when no
    // path segment survives.
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);

    let mut segments = after_scheme.split('/').filter(|s| !s.is_empty());
    let host = segments.next().unwrap_or("");

    match segments.next_back() {
        Some(last) => last.to_string(),

        None => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(url: &str) -> Locator {
        Locator::Remote(Remote {
            url: url.to_string(),
            headers: Vec::new(),
            hint: String::new(),
            live: false,
        })
    }

    #[test]
    fn path_is_some_for_local_and_none_for_remote() {
        let local = Locator::from(PathBuf::from("/music/a.flac"));
        assert_eq!(local.path(), Some(Path::new("/music/a.flac")));

        assert_eq!(remote("https://host/stream.mp3").path(), None);
    }

    #[test]
    fn label_falls_back_to_the_last_url_segment() {
        assert_eq!(remote("https://host/rest/stream.mp3").label(), "stream.mp3");

        // A trailing slash means the segment before it is the name.
        assert_eq!(remote("https://host/radio/jazz/").label(), "jazz");

        // The query is not part of the name, and neither is the fragment.
        assert_eq!(remote("https://host/stream?id=7&fmt=raw").label(), "stream");

        // Nothing but a host still has to show something.
        assert_eq!(
            remote("http://stream.example.com").label(),
            "stream.example.com"
        );
    }

    #[test]
    fn label_of_a_local_track_is_its_file_name() {
        let local = Locator::from(PathBuf::from("/music/artist/01_song.flac"));
        assert_eq!(local.label(), "01_song.flac");
    }
}
