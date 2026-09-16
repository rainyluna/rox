//! Turning OS-handed paths into playable ones. Both the command line
//! (`rox song.flac`) and an external file drop onto the window come through
//! here to get filtered down to files the engine can decode before they hit
//! the path-based player queue.
//!
//! Playlist files count as openable too: an `.m3u8`, `.pls`, or `.xspf`
//! handed over expands to the audio files it names, in its own order,
//! relative entries resolved against the playlist's folder. That is an open,
//! not an import - nothing lands in the library, the tracks just play.
//!
//! One rough edge, deliberately left: a cue subsong entry (`image.flac#3`)
//! opens the whole image, because the return here is a `Vec<PathBuf>` and a
//! subsong needs a `TrackKey`. Teaching both callers to take keys is the
//! follow-up that fixes it.

use std::path::{Path, PathBuf};

/// Every audio file directly under a directory, sorted so a dropped folder
/// enqueues in a stable order. Shallow: a folder drop grabs the tracks
/// directly in it, not a whole recursive tree. The test is the scanner's
/// own [`crate::scanner::is_audio`], so a drop and a scan agree on what
/// counts, and a folder full of macOS `._name` sidecars enqueues the real
/// tracks instead of twice as many files, half of which will not decode.
fn audio_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && crate::scanner::is_audio(p))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();
    files
}

/// The audio files a playlist file names, in its order. Entries are read with
/// the format sniffed off the content, relative ones resolve against the
/// playlist's own folder, and anything that is not a decodable file sitting
/// on disk drops: a stream URL, a track the sender has and the receiver does
/// not, a stale path.
fn audio_files_in_playlist(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in crate::playlist_file::parse(&text) {
        let resolve = |name: &str| {
            let name = Path::new(name);
            if name.is_absolute() {
                name.to_path_buf()
            } else {
                base.join(name)
            }
        };
        let mut full = resolve(&entry);
        if !full.is_file() {
            // A cue subsong, most likely. The literal name wins when it
            // really exists (a file called `track#2`), so this only runs
            // once that has been ruled out.
            let Some(image) = entry
                .rsplit_once('#')
                .filter(|(_, sub)| sub.parse::<u16>().is_ok_and(|sub| sub > 0))
                .map(|(image, _)| resolve(image))
            else {
                continue;
            };
            // Every subsong of a rip points at the same image, and pushing it
            // once per track would replay the album from the top each time.
            if out.last() == Some(&image) {
                continue;
            }
            full = image;
        }
        if full.is_file() && crate::scanner::is_audio(&full) {
            out.push(full);
        }
    }
    out
}

/// Resolve OS-handed paths into a flat, ordered list the player can take:
/// existing audio files pass through, existing directories expand to the
/// audio files sitting in them, playlist files expand to the tracks they
/// name, everything else drops. Order is preserved so `rox a.flac b.flac`
/// plays a then b.
pub fn resolve_audio_paths<I, P>(paths: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
{
    let mut out = Vec::new();
    for path in paths {
        let path = path.into();
        if path.is_dir() {
            out.extend(audio_files_in_dir(&path));
        } else if path.is_file() && crate::playlist_file::is_playlist_file(&path) {
            out.extend(audio_files_in_playlist(&path));
        } else if path.is_file() && crate::scanner::is_audio(&path) {
            out.push(path);
        }
    }
    out
}

/// What the OS asked us to do with the files on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Play the files now, replacing what was loaded. The .desktop default
    /// action and a bare `rox song.flac`.
    Play,
    /// Append the files to the up-next queue. The .desktop "Add to Queue"
    /// action passes `--enqueue`.
    Enqueue,
}

/// The launch mode and audio files the app was opened with, parsed off the real
/// argv. A leading `--enqueue`/`-e` flips to queue mode; everything else is
/// treated as a path, filtered to decodable audio and expanded. Files are
/// empty on a plain launch, so nothing routes into playback then.
pub fn from_args() -> (LaunchMode, Vec<PathBuf>) {
    let mut mode = LaunchMode::Play;
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if arg == "--enqueue" || arg == "-e" {
            mode = LaunchMode::Enqueue;
            continue;
        }
        args.push(arg);
    }
    (mode, resolve_audio_paths(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder dropped off a macOS-formatted stick is full of AppleDouble
    /// sidecars that carry the real extension. Enqueueing those would double
    /// the queue with files the engine cannot open, so the drop runs the
    /// scanner's own audio test.
    #[test]
    fn a_dropped_folder_skips_os_junk() {
        let dir = std::env::temp_dir().join("rox-open-files-junk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "one.flac",
            "._one.flac",
            "two.flac",
            "._two.flac",
            ".DS_Store",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        assert_eq!(
            resolve_audio_paths([&dir]),
            [dir.join("one.flac"), dir.join("two.flac")],
            "only the real tracks, in sorted order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dropped playlist expands to the tracks it names, in its order, with
    /// a relative entry read against the playlist's own folder and a stale
    /// one dropped rather than handed to the engine.
    #[test]
    fn a_playlist_expands_to_its_tracks() {
        let dir = std::env::temp_dir().join("rox-open-files-playlist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Extension is the whole test here, so empty files are enough; the
        // engine never sees these.
        for name in ["one.flac", "two.flac"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let absolute = dir.join("two.flac");
        let pls = dir.join("set.pls");
        std::fs::write(
            &pls,
            format!(
                "[playlist]\n\
                 File1=one.flac\n\
                 File2={}\n\
                 File3={}\n\
                 NumberOfEntries=3\nVersion=2\n",
                absolute.display(),
                dir.join("gone.flac").display(),
            ),
        )
        .unwrap();

        assert_eq!(
            resolve_audio_paths([&pls]),
            [dir.join("one.flac"), absolute],
            "playlist order, relative against the file's folder, misses dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
