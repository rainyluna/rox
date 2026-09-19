//! Non-local sources, and the sync that turns one into library rows. The
//! first is Subsonic: a server the user runs, whose catalog rox reads over
//! HTTP and files under its own source id. Headless like everything else
//! here, so nothing in this module knows a panel exists.
//!
//! A sync is a reconcile rather than an import. The server is asked what it
//! has, every song upserts under `subsonic:<digest>`, and anything still
//! filed under that id the server no longer lists gets pruned. Both halves
//! are scoped to the source string, so a sync can never reach a local row
//! no matter what the server sends back.
//!
//! The other half is the header table. A remote row stores its stream URL
//! and nothing else, because a credential stored in SQLite is a credential
//! somebody can lift back out of it. When playback resolves a row it asks
//! this module what headers that source needs and the live source object
//! answers from settings. Subsonic authorizes in the query string, so
//! today the answer is empty; the table exists because the contract takes
//! headers and the next source will have some.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, Entity, Task};

use rox_core::settings::Settings;
use rox_library::TrackRow;
use rox_library::playlists;
use rox_library::replaygain::ReplayGain;
use rox_library::rusqlite::Connection;
use rox_library::stations::{self, Station};
use rox_library::store;
use rox_net::sources::subsonic::{Server, ServerInfo};
use rox_net::sources::{SourceStation, SourceTrack};

use crate::catalog::Library;
use crate::sources_registry;

/// How wide a cover the server is asked to scale to. The thumbnail store
/// downscales again on the way in, so this only has to beat a grid tile
/// and stay well short of pulling a full-resolution scan down.
const COVER_SIZE: u32 = 512;

/// A running sync, as the settings row reads it. Atomics rather than an
/// entity and an event: the work is on the background executor, the reader
/// repaints on its own clock, and nothing else in the app cares.
struct Progress {
    running: AtomicBool,
    done: AtomicUsize,
    total: AtomicUsize,
}

static PROGRESS: Progress = Progress {
    running: AtomicBool::new(false),
    done: AtomicUsize::new(0),
    total: AtomicUsize::new(0),
};

/// What a finished sync did, for the line the settings section shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Rows written, which is every song the server listed.
    pub tracks: usize,
    /// Rows dropped because the server no longer lists them.
    pub pruned: usize,
    /// Server playlists created here for the first time.
    pub playlists: usize,
    /// Internet radio stations the server lists, written to the radio source.
    pub stations: usize,
}

/// Albums walked and albums to walk, while a sync runs. None when none is.
pub fn progress() -> Option<(usize, usize)> {
    if !PROGRESS.running.load(Ordering::Relaxed) {
        return None;
    }

    Some((
        PROGRESS.done.load(Ordering::Relaxed),
        PROGRESS.total.load(Ordering::Relaxed),
    ))
}

/// Whether a sync is in flight, so a second Sync Now doesn't start one on
/// top of the first.
pub fn syncing() -> bool {
    PROGRESS.running.load(Ordering::Relaxed)
}

/// The configured Subsonic server, or None when there isn't one worth
/// talking to. A disabled account still answers here, because Connect in
/// the settings row has to work before the switch goes on.
pub fn server() -> Option<Server> {
    let account = Settings::load().accounts.subsonic;
    if account.url.trim().is_empty() {
        return None;
    }

    Some(Server::new(&account.url, &account.user, &account.password))
}

/// Put the Subsonic server's header builder in the registry, under the
/// source id its rows are keyed by. Called at startup, before anything can
/// resolve a row, so a locator built during the first frame already has
/// somewhere to ask. Safe to call again after the settings change, which
/// is how a re-pointed server swaps its row.
pub fn install_registry() {
    let Some(configured) = server() else {
        // Nothing configured, so there's no source id to file under. A
        // locator resolved before a server exists carries no headers,
        // which is the right answer.
        return;
    };

    sources_registry::install(
        &configured.source_id(),
        // Rebuilt from settings on each call rather than captured here, so
        // a changed password takes effect without reinstalling the row.
        Box::new(|| server().map(|s| s.stream_headers()).unwrap_or_default()),
    );
}

/// Finish a stored stream URL so it can actually be fetched: the row's URL
/// with a fresh salt and token on the end. What the resolve path calls
/// once it has a locator in hand.
pub fn sign_stream(source: &str, url: &str) -> String {
    match server() {
        Some(server) if server.source_id() == source => server.sign(url),

        _ => url.to_string(),
    }
}

/// Reach the server and report what it says about itself. What the Connect
/// button runs, off the UI thread.
pub fn ping(cx: &App) -> Task<Result<ServerInfo, String>> {
    let server = server();

    cx.background_executor().spawn(async move {
        let server = server.ok_or_else(|| "no server configured".to_string())?;

        server.ping()
    })
}

/// Pull the whole catalog in and reconcile the library against it. The
/// work runs on the background executor on a connection of its own, the
/// way every other pass that writes the database does, and the projection
/// reloads once at the end rather than per album.
pub fn sync(library: Entity<Library>, cx: &mut App) -> Task<Result<SyncOutcome, String>> {
    // One sync at a time. Two would race each other's prune, and the loser
    // would delete what the winner had just written.
    if PROGRESS.running.swap(true, Ordering::SeqCst) {
        return Task::ready(Err("a sync is already running".to_string()));
    }

    PROGRESS.done.store(0, Ordering::Relaxed);
    PROGRESS.total.store(0, Ordering::Relaxed);

    let db_path = library.read(cx).db_path();
    let server = server();

    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let server = server.ok_or_else(|| "no server configured".to_string())?;
                let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;

                run(&server, &mut conn)
            })
            .await;

        PROGRESS.running.store(false, Ordering::Relaxed);

        // Rows moved, so the in-memory projection is stale until it's
        // rebuilt from SQLite and swapped whole.
        if outcome.is_ok() {
            let now = now_secs();
            Settings::update(move |s| s.accounts.subsonic.last_sync = now);

            library
                .update(cx, |library, cx| library.reload_projection(cx))
                .ok();
        }

        outcome
    })
}

/// The sync itself: blocking, and off any gpui context so it reads as one
/// piece. Fetch the catalog, write it, drop what's gone, then playlists.
fn run(server: &Server, conn: &mut Connection) -> Result<SyncOutcome, String> {
    let source = server.source_id();

    let tracks = server.catalog(|done, total| {
        PROGRESS.done.store(done, Ordering::Relaxed);
        PROGRESS.total.store(total, Ordering::Relaxed);
    })?;

    let now = now_secs();
    let rows: Vec<TrackRow> = tracks.iter().map(|track| row_for(track, now)).collect();

    store::upsert_source_rows(conn, &source, &rows).map_err(|e| e.to_string())?;

    // What the server still lists is what survives. Everything else under
    // this source id went away on the server's side, so it goes here too.
    let keep: HashSet<String> = tracks.iter().map(|track| track.id.clone()).collect();
    let pruned = store::prune_source(conn, &source, &keep).map_err(|e| e.to_string())?;

    let playlists = sync_playlists(server, conn, &source, now);
    let stations = sync_stations(server, conn);

    Ok(SyncOutcome {
        tracks: rows.len(),
        pruned,
        playlists,
        stations,
    })
}

/// Bring the server's playlists across, creating each one the first time
/// it's seen. A playlist that already exists by name is left alone: rox
/// can't tell a stale import from a list somebody has since edited here,
/// and clobbering the edit is the worse of the two mistakes.
///
/// Playlist trouble doesn't fail the sync. The tracks are already in by
/// the time this runs, and a server that refuses `getPlaylists` has still
/// handed over a library.
fn sync_playlists(server: &Server, conn: &mut Connection, source: &str, now: i64) -> usize {
    let Ok(remote) = server.playlists() else {
        return 0;
    };

    let existing: HashSet<String> = playlists::list(conn)
        .unwrap_or_default()
        .into_iter()
        .map(|playlist| playlist.name)
        .collect();

    let mut created = 0;
    for list in remote {
        if list.name.is_empty() || existing.contains(&list.name) {
            continue;
        }

        // Server ids to row ids, in the playlist's own order. A song the
        // catalog didn't return (unreadable on the server, filtered out of
        // a share) is skipped rather than failing the list.
        let track_ids: Vec<i64> = list
            .track_ids
            .iter()
            .filter_map(|id| store::id_for_path(conn, source, id).ok().flatten())
            .collect();

        if track_ids.is_empty() {
            continue;
        }

        let Ok(playlist_id) = playlists::create(conn, &list.name, now) else {
            continue;
        };

        if playlists::add(conn, playlist_id, &track_ids, now).is_ok() {
            created += 1;
        }
    }

    created
}

/// The server's internet radio list, written as stations. They land in the
/// radio source beside the ones typed into the panel, keyed on the stream
/// URL like any station, so a re-sync updates a renamed one in place. What
/// this can't do is drop one the server removed: the radio list has no
/// memory of where a station came from, and pruning it would take the
/// typed ones with it. Removing is the panel's job.
///
/// Like playlists, trouble here doesn't fail the sync. A plain Subsonic
/// server without the endpoint has still handed over its library.
fn sync_stations(server: &Server, conn: &mut Connection) -> usize {
    let Ok(remote) = server.radio_stations() else {
        return 0;
    };

    let found: Vec<Station> = remote.iter().map(station_for).collect();
    if found.is_empty() {
        return 0;
    }

    match stations::put(conn, &found) {
        Ok(()) => found.len(),
        Err(e) => {
            log::warn!("subsonic: writing {} stations failed: {e}", found.len());
            0
        }
    }
}

/// A server's station as the radio source stores it. The server's id is
/// dropped: the URL is the identity there, which is what lets the same
/// stream typed by hand and listed by the server be one row.
fn station_for(station: &SourceStation) -> Station {
    Station {
        url: station.stream_url.clone(),
        name: station.name.clone(),
        genre: String::new(),
    }
}

/// One remote track's cover, fetched from the server and stored on the way
/// through so the next ask is a lookup. Blocking, and deliberately not
/// part of the sync: a library's worth of art is a download nobody asked
/// for, and the row that needs a picture is the one on screen.
///
/// `thumbs` is the thumbnail database, not the library one.
pub fn cover(thumbs: &Connection, song_id: &str, cover_id: &str) -> Option<Vec<u8>> {
    if cover_id.is_empty() {
        return None;
    }

    let bytes = server()?.cover(cover_id, COVER_SIZE).ok()?;

    rox_library::thumbs::store_bytes(thumbs, &bytes, song_id)
}

/// One song from the server as a library row. The empty fields are the
/// honest answer rather than a placeholder: Subsonic reports no sort
/// names, no ReplayGain, no tempo and no sample format, so those sit the
/// way they would for a file whose tags carry none.
fn row_for(track: &SourceTrack, now: i64) -> TrackRow {
    TrackRow {
        // The server's song id stands in for a path. It's what identity is
        // keyed on for this source and what `id_for_path` resolves back.
        path: track.id.clone(),
        sub: 0,
        cue: None,
        remote_url: track.stream_url.clone(),
        remote_live: false,
        title: track.title.clone(),
        artist: track.artist.clone(),
        album_artist: track.album_artist.clone(),
        album: track.album.clone(),
        title_sort: String::new(),
        artist_sort: String::new(),
        album_artist_sort: String::new(),
        album_sort: String::new(),
        genre: track.genre.clone(),
        year: track.year,
        disc_no: track.disc_no,
        track_no: track.track_no,
        duration_ms: track.duration_ms,
        codec: track.codec.clone(),
        bitrate_kbps: track.bitrate_kbps,
        sample_rate_hz: 0,
        bit_depth: 0,
        rating: 0,
        replay_gain: ReplayGain::default(),
        bpm: None,
        size: track.size.max(0) as u64,
        // There's no file to stat, so the sync's own clock stands in.
        // Nothing reads it to decide whether to re-read a file, since
        // scans only ever walk local roots.
        mtime: now,
    }
}

/// What the library holds for one source, for the settings readout. Zero
/// when the source has never synced.
pub fn row_count(conn: &Connection, source: &str) -> usize {
    store::sources(conn)
        .unwrap_or_default()
        .into_iter()
        .find(|(name, _)| name == source)
        .map(|(_, count)| count)
        .unwrap_or(0)
}

/// Wall clock in unix seconds, the stamp every write here shares.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> SourceTrack {
        SourceTrack {
            id: "sg-1".into(),
            title: "Jynweythek".into(),
            artist: "Aphex Twin".into(),
            album_artist: "Aphex Twin".into(),
            album: "Drukqs".into(),
            genre: "Electronic".into(),
            year: 2001,
            disc_no: 1,
            track_no: 1,
            duration_ms: 109_000,
            codec: "flac".into(),
            bitrate_kbps: 533,
            size: 7_261_184,
            stream_url: "https://music.example.com/rest/stream.view?id=sg-1&format=raw".into(),
            cover_id: "al-1".into(),
        }
    }

    #[test]
    fn a_song_becomes_a_row_keyed_on_its_server_id() {
        let row = row_for(&track(), 1_700_000_000);

        assert_eq!(row.path, "sg-1");
        assert_eq!(row.sub, 0);
        assert_eq!(row.title, "Jynweythek");
        assert_eq!(row.album_artist, "Aphex Twin");
        assert_eq!(row.year, 2001);
        assert_eq!(row.disc_no, 1);
        assert_eq!(row.duration_ms, 109_000);
        assert_eq!(row.codec, "flac");
        assert_eq!(row.size, 7_261_184);
        assert_eq!(row.mtime, 1_700_000_000);

        // The stream URL rides the row; the credentials never do.
        assert_eq!(
            row.remote_url,
            "https://music.example.com/rest/stream.view?id=sg-1&format=raw"
        );
        assert!(!row.remote_url.contains("&t="));
        assert!(!row.remote_live);
    }

    #[test]
    fn a_sparse_song_maps_the_way_an_untagged_file_would() {
        // No year, no disc, no genre, no bitrate: what real servers return
        // constantly.
        let mut sparse = track();
        sparse.year = 0;
        sparse.disc_no = 0;
        sparse.genre = String::new();
        sparse.bitrate_kbps = 0;

        let row = row_for(&sparse, 0);

        assert_eq!(row.year, 0);
        assert_eq!(row.disc_no, 0);
        assert_eq!(row.genre, "");
        assert_eq!(row.bitrate_kbps, 0);

        // Nothing invents a measurement the server never made.
        assert!(!row.replay_gain.any());
        assert!(row.bpm.is_none());
        assert_eq!(row.sample_rate_hz, 0);
        assert_eq!(row.bit_depth, 0);
        assert!(row.cue.is_none());
    }

    #[test]
    fn a_server_station_keeps_its_url_as_the_identity() {
        let station = station_for(&SourceStation {
            id: "ir-2".into(),
            name: "HBR1.com - Dream Factory".into(),
            stream_url: "http://ubuntu.hbr1.com:19800/ambient.aac".into(),
            home_page: "http://www.hbr1.com/".into(),
        });

        assert_eq!(station.url, "http://ubuntu.hbr1.com:19800/ambient.aac");
        assert_eq!(station.name, "HBR1.com - Dream Factory");
        assert!(station.genre.is_empty());
    }

    #[test]
    fn a_negative_size_reads_as_nothing_rather_than_wrapping() {
        let mut odd = track();
        odd.size = -1;

        assert_eq!(row_for(&odd, 0).size, 0);
    }
}
