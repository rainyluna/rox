//! Rows by membership, the plugin source shape from ADR 29's second
//! amendment. A plugin has no whole catalog to reconcile against: its rows
//! come from the collections the user syncs and the tracks they pick one at a
//! time. `source_members` records which collections hold each row, picks
//! living in a collection of their own ([`PICKED`]), and a row is pruned only
//! when nothing holds it or the user removes it.
//!
//! Writes here refuse local files, stations and Subsonic servers outright,
//! and every statement is scoped to the source it was handed. Nothing
//! expires a picked row on its own; ADR 29 leaves that open.

use std::path::PathBuf;

use rusqlite::{Connection, params};

use crate::cue::{self, TrackKey};
use crate::replaygain::ReplayGain;
use crate::{TrackRow, listens, playlists, stations, store};

/// The collection single picks live in. Node ids from a plugin are never
/// empty, so it can't collide with a synced one.
pub const PICKED: &str = "";

/// One track as a plugin lists it. Every field is set, empty or zero when the
/// plugin doesn't know it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginTrack {
    /// Opaque to rox and stable across sessions, since it becomes the row's
    /// path.
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub genre: String,
    pub year: u16,
    pub disc_no: u16,
    pub track_no: u16,
    pub duration_ms: u32,
    pub codec: String,
    pub bitrate_kbps: u16,
    /// The stream never ends: no duration, no gapless boundary.
    pub live: bool,
}

/// `(source, path)` backs the orphan prune, which asks per row whether any
/// collection still holds it.
pub(crate) fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_members (
            source     TEXT NOT NULL,
            collection TEXT NOT NULL,
            path       TEXT NOT NULL,
            position   INTEGER NOT NULL,
            PRIMARY KEY (source, collection, path)
        );
        CREATE INDEX IF NOT EXISTS source_members_path ON source_members (source, path);",
    )
}

/// The row a plugin track becomes. It stores no stream URL: a plugin row
/// plays through the plugin, keyed by its path.
pub fn row_for(track: &PluginTrack, now: i64) -> TrackRow {
    TrackRow {
        path: track.key.clone(),
        sub: 0,
        cue: None,
        remote_url: String::new(),
        remote_live: track.live,
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
        size: 0,
        // No file to stat; scans only ever walk local roots.
        mtime: now,
    }
}

/// Upsert `tracks` and hold each in [`PICKED`], answering their keys so the
/// caller can queue them. Never prunes: writing one row never removes
/// another, the rule `stations::put` follows.
pub fn pick(
    conn: &mut Connection,
    source: &str,
    tracks: &[PluginTrack],
) -> rusqlite::Result<Vec<TrackKey>> {
    refuse_other_shapes(source)?;

    if tracks.is_empty() {
        return Ok(Vec::new());
    }

    let tx = conn.transaction()?;
    upsert(&tx, source, tracks)?;
    hold(&tx, source, PICKED, tracks)?;
    playlists::reattach(&tx)?;
    listens::reattach(&tx)?;
    tx.commit()?;

    let id = cue::source_id(source);
    Ok(tracks
        .iter()
        .map(|track| TrackKey {
            source: id.clone(),
            path: PathBuf::from(&track.key),
            sub: 0,
        })
        .collect())
}

/// Make `collection` hold exactly `tracks`, in order, then prune the rows
/// nothing holds any more. Answers how many went.
pub fn set_collection(
    conn: &mut Connection,
    source: &str,
    collection: &str,
    tracks: &[PluginTrack],
) -> rusqlite::Result<usize> {
    refuse_other_shapes(source)?;

    let tx = conn.transaction()?;
    upsert(&tx, source, tracks)?;
    tx.execute(
        "DELETE FROM source_members WHERE source = ?1 AND collection = ?2",
        params![source, collection],
    )?;
    hold(&tx, source, collection, tracks)?;

    let pruned = prune_orphans(&tx, source)?;
    playlists::reattach(&tx)?;
    listens::reattach(&tx)?;
    tx.commit()?;

    Ok(pruned)
}

/// Let go of a collection, pruning the rows only it held. Answers how many
/// went.
pub fn drop_collection(
    conn: &mut Connection,
    source: &str,
    collection: &str,
) -> rusqlite::Result<usize> {
    refuse_other_shapes(source)?;

    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM source_members WHERE source = ?1 AND collection = ?2",
        params![source, collection],
    )?;

    let pruned = prune_orphans(&tx, source)?;
    playlists::reattach(&tx)?;
    listens::reattach(&tx)?;
    tx.commit()?;

    Ok(pruned)
}

/// The user's explicit remove: the row goes whatever still holds it.
pub fn remove_track(conn: &mut Connection, source: &str, path: &str) -> rusqlite::Result<()> {
    refuse_other_shapes(source)?;

    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM source_members WHERE source = ?1 AND path = ?2",
        params![source, path],
    )?;
    tx.execute(
        "DELETE FROM tracks WHERE source = ?1 AND path = ?2",
        params![source, path],
    )?;
    playlists::reattach(&tx)?;
    listens::reattach(&tx)?;
    tx.commit()
}

/// Removing the plugin: every row and membership of the source goes. Answers
/// how many rows went.
pub fn remove_source(conn: &mut Connection, source: &str) -> rusqlite::Result<usize> {
    refuse_other_shapes(source)?;

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM source_members WHERE source = ?1", [source])?;
    let removed = tx.execute("DELETE FROM tracks WHERE source = ?1", [source])?;
    playlists::reattach(&tx)?;
    listens::reattach(&tx)?;
    tx.commit()?;

    Ok(removed)
}

/// A collection's tracks in the order it was last synced or picked.
pub fn list(conn: &Connection, source: &str, collection: &str) -> rusqlite::Result<Vec<TrackKey>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM source_members
          WHERE source = ?1 AND collection = ?2
          ORDER BY position",
    )?;

    let id = cue::source_id(source);
    let rows = stmt.query_map(params![source, collection], |r| {
        Ok(TrackKey {
            source: id.clone(),
            path: PathBuf::from(r.get::<_, String>(0)?),
            sub: 0,
        })
    })?;

    rows.collect()
}

/// Every collection of the source with how many tracks it holds, [`PICKED`]
/// included when anything was picked.
pub fn collections(conn: &Connection, source: &str) -> rusqlite::Result<Vec<(String, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT collection, COUNT(*) FROM source_members
          WHERE source = ?1 GROUP BY collection ORDER BY collection",
    )?;

    let rows = stmt.query_map([source], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
    })?;

    rows.collect()
}

/// A membership write aimed at another source shape would prune rows its own
/// sync or directory owns, so it never gets as far as a statement.
fn refuse_other_shapes(source: &str) -> rusqlite::Result<()> {
    let other = source == cue::LOCAL
        || source == stations::SOURCE
        || source.starts_with(cue::SUBSONIC_PREFIX);

    if other {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
            Some(format!("{source} doesn't hold its rows by membership")),
        ));
    }

    Ok(())
}

/// Runs inside the caller's transaction, so the rows land or fail together
/// with the membership that holds them. A row committed without its
/// membership would sit unheld until some later prune.
fn upsert(conn: &Connection, source: &str, tracks: &[PluginTrack]) -> rusqlite::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let rows: Vec<TrackRow> = tracks.iter().map(|track| row_for(track, now)).collect();
    store::upsert_source_rows_in(conn, source, &rows)
}

/// Append `tracks` to the collection after its last member. A key already
/// held keeps its place, so a repeat pick or a key listed twice doesn't
/// duplicate.
fn hold(
    conn: &Connection,
    source: &str,
    collection: &str,
    tracks: &[PluginTrack],
) -> rusqlite::Result<()> {
    let mut next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM source_members
          WHERE source = ?1 AND collection = ?2",
        params![source, collection],
        |r| r.get(0),
    )?;

    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO source_members (source, collection, path, position)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for track in tracks {
        // Advance only when a member landed, so a repeat leaves no gap.
        if insert.execute(params![source, collection, track.key, next])? > 0 {
            next += 1;
        }
    }

    Ok(())
}

/// One statement rather than `store::prune_source`'s read-then-delete: that
/// one reads paths into memory to stay under the bound-parameter ceiling,
/// and a subquery binds nothing but the source.
fn prune_orphans(conn: &Connection, source: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM tracks WHERE source = ?1
           AND NOT EXISTS (SELECT 1 FROM source_members m
                            WHERE m.source = ?1 AND m.path = tracks.path)",
        [source],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = "plugin:demo";

    fn track(key: &str, title: &str) -> PluginTrack {
        PluginTrack {
            key: key.into(),
            title: title.into(),
            artist: "Artist".into(),
            album_artist: "Artist".into(),
            album: "Album".into(),
            ..Default::default()
        }
    }

    fn a_and_b() -> [PluginTrack; 2] {
        [track("a", "A"), track("b", "B")]
    }

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    fn rows(conn: &Connection, source: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT path FROM tracks WHERE source = ?1 ORDER BY path")
            .unwrap();
        stmt.query_map([source], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn paths(keys: &[TrackKey]) -> Vec<String> {
        keys.iter()
            .map(|key| key.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn row_for_keys_the_row_and_stores_no_url() {
        let row = row_for(
            &PluginTrack {
                live: true,
                ..track("k1", "One")
            },
            42,
        );

        assert_eq!(row.path, "k1");
        assert_eq!(row.sub, 0);
        assert_eq!(row.remote_url, "");
        assert!(row.remote_live);
        assert_eq!(row.mtime, 42);
    }

    #[test]
    fn pick_holds_rows_once_however_often_they_arrive() {
        let mut conn = store();

        let keys = pick(&mut conn, DEMO, &[track("a", "A"), track("b", "B")]).unwrap();
        assert_eq!(paths(&keys), ["a", "b"]);
        assert!(keys.iter().all(|key| &*key.source == DEMO && key.sub == 0));

        pick(&mut conn, DEMO, &[track("a", "A again")]).unwrap();
        assert_eq!(
            rows(&conn, DEMO),
            ["a", "b"],
            "the upsert refreshed a in place"
        );
        assert_eq!(paths(&list(&conn, DEMO, PICKED).unwrap()), ["a", "b"]);
        assert_eq!(collections(&conn, DEMO).unwrap(), [(PICKED.to_string(), 2)]);

        let title: String = conn
            .query_row(
                "SELECT title FROM tracks WHERE source = ?1 AND path = 'a'",
                [DEMO],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "A again");
    }

    #[test]
    fn a_resync_prunes_what_nothing_else_holds() {
        let mut conn = store();

        set_collection(&mut conn, DEMO, "liked", &a_and_b()).unwrap();
        let pruned = set_collection(&mut conn, DEMO, "liked", &[track("b", "B")]).unwrap();

        assert_eq!(pruned, 1);
        assert_eq!(rows(&conn, DEMO), ["b"]);
        assert_eq!(paths(&list(&conn, DEMO, "liked").unwrap()), ["b"]);
    }

    #[test]
    fn a_picked_row_survives_leaving_a_collection() {
        let mut conn = store();

        set_collection(&mut conn, DEMO, "liked", &a_and_b()).unwrap();
        pick(&mut conn, DEMO, &[track("a", "A")]).unwrap();
        let pruned = set_collection(&mut conn, DEMO, "liked", &[track("b", "B")]).unwrap();

        assert_eq!(pruned, 0);
        assert_eq!(rows(&conn, DEMO), ["a", "b"]);
    }

    #[test]
    fn syncing_one_collection_never_prunes_another() {
        let mut conn = store();

        set_collection(&mut conn, DEMO, "playlist-1", &[track("p", "P")]).unwrap();
        set_collection(&mut conn, DEMO, "liked", &[track("a", "A")]).unwrap();
        let pruned = set_collection(&mut conn, DEMO, "liked", &[]).unwrap();

        assert_eq!(pruned, 1, "only a went");
        assert_eq!(rows(&conn, DEMO), ["p"]);
    }

    #[test]
    fn a_collection_keeps_the_order_it_was_synced_in() {
        let mut conn = store();

        set_collection(
            &mut conn,
            DEMO,
            "liked",
            &[
                track("c", "C"),
                track("a", "A"),
                track("c", "C"),
                track("b", "B"),
            ],
        )
        .unwrap();

        assert_eq!(
            paths(&list(&conn, DEMO, "liked").unwrap()),
            ["c", "a", "b"],
            "a key listed twice holds its first place"
        );
    }

    #[test]
    fn a_sync_that_fails_after_the_upsert_leaves_nothing_behind() {
        let mut conn = store();
        set_collection(&mut conn, DEMO, "liked", &[track("a", "A")]).unwrap();

        // Any membership insert fails, which lands between the upsert and
        // the prune.
        conn.execute_batch(
            "CREATE TRIGGER refuse_members BEFORE INSERT ON source_members
             BEGIN SELECT RAISE(ABORT, 'forced'); END;",
        )
        .unwrap();

        assert!(set_collection(&mut conn, DEMO, "liked", &[track("b", "B")]).is_err());
        assert!(pick(&mut conn, DEMO, &[track("c", "C")]).is_err());

        assert_eq!(rows(&conn, DEMO), ["a"], "neither write landed a row");
        assert_eq!(
            paths(&list(&conn, DEMO, "liked").unwrap()),
            ["a"],
            "and the old membership stands"
        );
    }

    #[test]
    fn drop_collection_prunes_exactly_the_rows_only_it_held() {
        let mut conn = store();

        set_collection(&mut conn, DEMO, "liked", &a_and_b()).unwrap();
        set_collection(&mut conn, DEMO, "playlist-1", &[track("b", "B")]).unwrap();
        pick(&mut conn, DEMO, &[track("c", "C")]).unwrap();

        assert_eq!(drop_collection(&mut conn, DEMO, "liked").unwrap(), 1);
        assert_eq!(rows(&conn, DEMO), ["b", "c"]);
        assert!(list(&conn, DEMO, "liked").unwrap().is_empty());
        assert_eq!(
            collections(&conn, DEMO).unwrap(),
            [(PICKED.to_string(), 1), ("playlist-1".to_string(), 1)]
        );
    }

    #[test]
    fn remove_track_goes_whatever_holds_it() {
        let mut conn = store();

        set_collection(&mut conn, DEMO, "liked", &a_and_b()).unwrap();
        pick(&mut conn, DEMO, &[track("a", "A")]).unwrap();
        remove_track(&mut conn, DEMO, "a").unwrap();

        assert_eq!(rows(&conn, DEMO), ["b"]);
        assert!(list(&conn, DEMO, PICKED).unwrap().is_empty());
        assert_eq!(paths(&list(&conn, DEMO, "liked").unwrap()), ["b"]);
    }

    #[test]
    fn remove_source_touches_no_other_source() {
        let mut conn = store();
        let other = "plugin:other";

        set_collection(&mut conn, DEMO, "liked", &a_and_b()).unwrap();
        pick(&mut conn, other, &[track("a", "A")]).unwrap();
        stations::put(
            &mut conn,
            &[stations::Station {
                url: "a".into(),
                name: "Station".into(),
                genre: String::new(),
            }],
        )
        .unwrap();

        assert_eq!(remove_source(&mut conn, DEMO).unwrap(), 2);
        assert!(rows(&conn, DEMO).is_empty());
        assert!(collections(&conn, DEMO).unwrap().is_empty());
        assert_eq!(rows(&conn, other), ["a"]);
        assert_eq!(paths(&list(&conn, other, PICKED).unwrap()), ["a"]);
        assert_eq!(rows(&conn, stations::SOURCE), ["a"]);
    }

    #[test]
    fn every_write_refuses_another_source_shape() {
        let mut conn = store();
        stations::put(
            &mut conn,
            &[stations::Station {
                url: "a".into(),
                name: "Station".into(),
                genre: String::new(),
            }],
        )
        .unwrap();

        for source in [cue::LOCAL, stations::SOURCE, "subsonic:abc123"] {
            let a = [track("a", "A")];

            assert!(pick(&mut conn, source, &a).is_err(), "{source}");
            assert!(
                set_collection(&mut conn, source, "liked", &a).is_err(),
                "{source}"
            );
            assert!(
                set_collection(&mut conn, source, "liked", &[]).is_err(),
                "{source}"
            );
            assert!(
                drop_collection(&mut conn, source, "liked").is_err(),
                "{source}"
            );
            assert!(remove_track(&mut conn, source, "a").is_err(), "{source}");
            assert!(remove_source(&mut conn, source).is_err(), "{source}");
        }

        assert_eq!(rows(&conn, stations::SOURCE), ["a"], "the station stands");
        assert!(rows(&conn, cue::LOCAL).is_empty());
    }
}
