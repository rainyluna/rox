//! The two catalog-shaped pieces the lyrics matcher and the lyrics panel
//! both need: what a provider gets asked for a track, and where a found
//! sheet is saved. Neither renders anything, so both are down here where
//! the panel and the matcher window can each get at them.
//!
//! A path is not enough to name either one. A Subsonic song has no file to
//! sit beside, and a radio station is one URL playing a different song
//! every three minutes, so its library row names the station and the song
//! only exists in what the stream announces. [`LyricsTarget`] carries both
//! halves of the answer, the subject a sheet is filed under and the query a
//! provider is asked, and every lyrics surface works on one of those.

use std::path::Path;

use gpui::{App, Entity};

use rox_core::settings::{LyricsSave, Settings, lyrics_dir};
use rox_library::cue::{Origin, TrackKey};
use rox_library::lyrics::{self, Source, Subject};
use rox_net::providers::TrackQuery;
use rox_playback::IcyTitle;

use crate::catalog::Library;

/// What a lyrics surface works on: the subject a sheet is filed under and
/// the tags a provider is asked for. The two are built together because
/// for a station they come from the same place, the song it just
/// announced, and building either off the library row alone would file a
/// sheet under the station or ask a provider for the words to one.
#[derive(Clone, Debug, PartialEq)]
pub struct LyricsTarget {
    pub subject: Subject,
    pub query: TrackQuery,
}

impl LyricsTarget {
    /// The file behind the target, for the reads and writes that need one.
    pub fn file(&self) -> Option<&Path> {
        self.subject.file()
    }

    /// The track as a window header names it: the title, the artist
    /// trailing it when there is one.
    pub fn label(&self) -> String {
        if self.query.artist.is_empty() {
            self.query.title.clone()
        } else {
            format!("{} - {}", self.query.title, self.query.artist)
        }
    }
}

/// The provider query for a track: its tags off the catalog, and its
/// duration off the projection so the score doesn't depend on the track
/// being the one playing.
pub fn query_for(library: &Entity<Library>, key: &TrackKey, cx: &App) -> TrackQuery {
    let catalog = library.read(cx);
    let resolved = catalog.resolve_key(key);
    let duration_ms = resolved
        .as_ref()
        .and_then(|(id, _)| duration_ms_for(library, *id, cx))
        .unwrap_or(0);
    let meta = resolved.map(|(_, meta)| meta);
    let (artist, title, album) = meta
        .map(|m| (m.artist, m.title, m.album))
        .unwrap_or_default();
    TrackQuery {
        artist,
        title,
        album,
        duration_secs: (duration_ms > 0).then(|| duration_ms as f64 / 1000.0),
    }
}

/// The lyrics target for a track. `live` is the song the stream is
/// announcing, which the caller passes only when this track is the
/// playing station: a station's row names the station and holds that name
/// for the whole broadcast, so without the announcement there is no song
/// here to look up or file anything under.
///
/// None where there is nothing to file a sheet under, which is a station
/// that hasn't named a song yet.
pub fn target_for(
    library: &Entity<Library>,
    key: &TrackKey,
    live: Option<&IcyTitle>,
    cx: &App,
) -> Option<LyricsTarget> {
    let mut query = query_for(library, key, cx);

    if let Some(live) = live {
        query.artist = live.artist.clone();
        query.title = live.title.clone();
        // The row's title is the station's name, which reads as the album
        // for the song it happens to be playing and scores against nothing.
        // A stream has no length either, so there is no duration to weigh.
        query.album = String::new();
        query.duration_secs = None;
    }

    Some(LyricsTarget {
        subject: subject_for(key, live)?,
        query,
    })
}

/// The subject half of [`target_for`] on its own, without the catalog
/// lookup the query needs. Cheap enough for a playback tick, which is
/// what the edit window compares against to tell whether the track it is
/// open on is the one playing.
pub fn subject_for(key: &TrackKey, live: Option<&IcyTitle>) -> Option<Subject> {
    if key.is_local() {
        return Some(Subject::File(key.path.clone()));
    }

    if matches!(key.origin(), Origin::Radio) {
        let live = live?;

        return Subject::song(&live.artist, &live.title);
    }

    // A server hands the same id back for the same song every time, which
    // is the whole of what the store needs.
    Some(Subject::remote(&key.to_fragment()))
}

/// The song the playing track's words belong to, or None when nothing is
/// playing and when a station hasn't announced anything yet.
pub fn playing_subject(player: &crate::player::Player) -> Option<Subject> {
    let now = player.now_playing()?;
    let live = now.live.then(|| player.live_title()).flatten();

    subject_for(&now.key, live.as_ref())
}

/// Where a saved sheet goes, per the Providers page's tag/sidecar/store
/// choice. Shared by the matcher's Apply and the panel's auto-search so
/// both honor the one destination setting.
///
/// A subject with no file behind it has neither a sidecar to write beside
/// nor a tag to write into, so the store is the only home the setting
/// could have named and it takes that one whatever the page says.
pub fn save_target(subject: &Subject) -> Source {
    let Some(path) = subject.file() else {
        return Source::Store(lyrics::store_file(&lyrics_dir(), subject));
    };

    match Settings::load().accounts.providers.lyrics_save {
        LyricsSave::Tag => Source::Tag,
        LyricsSave::Sidecar => Source::Sidecar(lyrics::default_sidecar(path)),
        LyricsSave::Store => Source::Store(lyrics::store_file(&lyrics_dir(), subject)),
    }
}

/// The track's duration in ms off the projection, resolved from its id.
fn duration_ms_for(library: &Entity<Library>, id: i64, cx: &App) -> Option<u32> {
    let catalog = library.read(cx);
    let projection = catalog.projection()?;
    let row = (0..projection.len() as u32)
        .find(|&row| projection.db_id[row as usize] == id && !projection.is_dead(row))?;
    Some(projection.resolve(row).duration_ms)
}
