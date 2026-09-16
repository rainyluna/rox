//! Playback bookmarks: a saved position inside a track, with an optional
//! name and color. A row is what a press of M drops while a track plays,
//! and what the seek strip, the waveform, and the bookmarks panel draw
//! back: the audiobook and long-mix feature, the one thing the session
//! restore's single "where was I" can't cover.
//!
//! Keyed by track id like the sort-name tables, with the path fragment
//! snapshotted beside it the way playlist members and listens do it, so a
//! file that leaves the library and comes back under a fresh id gets its
//! marks back through [`reattach`]. The position is milliseconds into the
//! track (a cue track's own clock, not its image's), which is the unit
//! the cue sheet and the player already speak.

use rusqlite::{Connection, OptionalExtension, params};

/// The table beside the tracks it points at. No foreign key: a deleted
/// track keeps its marks dangling for the reattach, like listens.
pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bookmarks (
            id          INTEGER PRIMARY KEY,
            track_id    INTEGER NOT NULL,
            position_ms INTEGER NOT NULL,
            name        TEXT NOT NULL DEFAULT '',
            color       TEXT NOT NULL DEFAULT '',
            created     INTEGER NOT NULL,
            path        TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS bookmarks_track ON bookmarks (track_id, position_ms);",
    )
}

/// One saved position.
#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub id: i64,
    pub track_id: i64,
    /// Milliseconds into the track.
    pub position_ms: u32,
    /// The label, empty for a mark dropped without one; views show the
    /// time in its place.
    pub name: String,
    /// `#rrggbb`, or None for the theme accent, which follows the palette.
    pub color: Option<String>,
    /// Unix seconds.
    pub created: i64,
}

/// A bookmark with the track it sits in, for views that list every mark
/// rather than one track's. Tracks come off the live catalog, so a mark
/// whose track is gone doesn't list until the reattach brings it back.
#[derive(Clone, Debug)]
pub struct BookmarkRow {
    pub bookmark: Bookmark,
    pub track_id: i64,
    pub path: String,
    pub sub: u16,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: u16,
    pub track_no: u16,
    pub duration_ms: u32,
    pub rating: u8,
}

/// The color column's shape on the way in and out: blank is the accent.
fn color_of(raw: String) -> Option<String> {
    let raw = raw.trim().to_string();
    (!raw.is_empty()).then_some(raw)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Drop a mark on a track. `path` is the key's fragment form, the join
/// hint [`reattach`] matches on. Returns the new row's id.
pub fn add(
    conn: &Connection,
    track_id: i64,
    path: &str,
    position_ms: u32,
    name: &str,
    color: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO bookmarks (track_id, position_ms, name, color, created, path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            track_id,
            position_ms as i64,
            name.trim(),
            color.unwrap_or("").trim(),
            now_secs(),
            path
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Every mark on one track, earliest first.
pub fn for_track(conn: &Connection, track_id: i64) -> rusqlite::Result<Vec<Bookmark>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, track_id, position_ms, name, color, created
         FROM bookmarks WHERE track_id = ?1 ORDER BY position_ms, id",
    )?;
    let rows = stmt.query_map([track_id], row_bookmark)?;
    rows.collect()
}

fn row_bookmark(row: &rusqlite::Row) -> rusqlite::Result<Bookmark> {
    Ok(Bookmark {
        id: row.get(0)?,
        track_id: row.get(1)?,
        position_ms: row.get::<_, i64>(2)?.max(0) as u32,
        name: row.get(3)?,
        color: color_of(row.get(4)?),
        created: row.get(5)?,
    })
}

/// One mark by id, for a caller editing it that holds only the id.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Bookmark>> {
    conn.query_row(
        "SELECT id, track_id, position_ms, name, color, created FROM bookmarks WHERE id = ?1",
        [id],
        row_bookmark,
    )
    .optional()
}

/// Every mark whose track is in the catalog, grouped by track in browse
/// order (album artist, album, disc, track) and earliest mark first
/// within a track. What the bookmarks panel lists.
pub fn all(conn: &Connection) -> rusqlite::Result<Vec<BookmarkRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b.id, b.track_id, b.position_ms, b.name, b.color, b.created,
                t.path, t.sub, t.title, t.artist, t.album, t.duration_ms,
                t.genre, t.year, t.track_no, t.rating
         FROM bookmarks b JOIN tracks t ON t.id = b.track_id
         WHERE t.source = 'local'
         ORDER BY t.album_artist, t.album, t.disc_no, t.track_no, t.title, t.id,
                  b.position_ms, b.id",
    )?;
    let rows = stmt.query_map([], |row| {
        let bookmark = row_bookmark(row)?;
        Ok(BookmarkRow {
            track_id: bookmark.track_id,
            bookmark,
            path: row.get(6)?,
            sub: row.get::<_, i64>(7)?.max(0) as u16,
            title: row.get(8)?,
            artist: row.get(9)?,
            album: row.get(10)?,
            duration_ms: row.get::<_, i64>(11)?.max(0) as u32,
            genre: row.get(12)?,
            year: row.get::<_, i64>(13)?.clamp(0, u16::MAX as i64) as u16,
            track_no: row.get::<_, i64>(14)?.clamp(0, u16::MAX as i64) as u16,
            rating: row.get::<_, i64>(15)?.clamp(0, 100) as u8,
        })
    })?;
    rows.collect()
}

/// Whether any mark exists at all, for a view deciding between its list
/// and its empty-state line without reading the rows.
pub fn count(conn: &Connection) -> rusqlite::Result<u64> {
    conn.query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))
}

pub fn rename(conn: &Connection, id: i64, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE bookmarks SET name = ?2 WHERE id = ?1",
        params![id, name.trim()],
    )?;
    Ok(())
}

/// Set the color, None going back to the theme accent.
pub fn set_color(conn: &Connection, id: i64, color: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE bookmarks SET color = ?2 WHERE id = ?1",
        params![id, color.unwrap_or("").trim()],
    )?;
    Ok(())
}

/// Slide a mark to a new position on the same track.
pub fn set_position(conn: &Connection, id: i64, position_ms: u32) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE bookmarks SET position_ms = ?2 WHERE id = ?1",
        params![id, position_ms as i64],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
    Ok(())
}

/// How many marks sit on these tracks together, for a menu deciding
/// whether to offer clearing them.
pub fn count_for_tracks(conn: &Connection, track_ids: &[i64]) -> rusqlite::Result<u64> {
    let mut stmt = conn.prepare_cached("SELECT COUNT(*) FROM bookmarks WHERE track_id = ?1")?;
    let mut total = 0u64;
    for &id in track_ids {
        total += stmt.query_row([id], |row| row.get::<_, u64>(0))?;
    }
    Ok(total)
}

/// Drop every mark on these tracks. Returns how many went.
pub fn remove_for_tracks(conn: &Connection, track_ids: &[i64]) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare_cached("DELETE FROM bookmarks WHERE track_id = ?1")?;
    let mut gone = 0;
    for &id in track_ids {
        gone += stmt.execute([id])?;
    }
    Ok(gone)
}

/// Match marks back to the catalog after a scan, the move
/// [`crate::listens::reattach`] makes for events: a pruned-and-returned
/// file comes back under a fresh id, and the marks that pointed at its
/// old row relink through the path fragment recorded when they were set.
/// Live rows just keep their path current.
///
/// Returns how many marks relinked, or None when nothing was dangling.
pub fn reattach(conn: &Connection) -> rusqlite::Result<Option<usize>> {
    conn.execute(
        "UPDATE bookmarks SET path =
             CASE WHEN t.sub = 0 THEN t.path ELSE t.path || '#' || t.sub END
         FROM tracks t
         WHERE t.id = bookmarks.track_id AND t.source = 'local'
           AND bookmarks.path <>
             CASE WHEN t.sub = 0 THEN t.path ELSE t.path || '#' || t.sub END",
        [],
    )?;
    let dangling: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM bookmarks
            WHERE NOT EXISTS (SELECT 1 FROM tracks x WHERE x.id = bookmarks.track_id))",
        [],
        |row| row.get(0),
    )?;
    if !dangling {
        return Ok(None);
    }
    let relinked = conn.execute(
        "UPDATE bookmarks SET track_id = t.id FROM tracks t
         WHERE bookmarks.path <> '' AND t.source = 'local'
           AND CASE WHEN t.sub = 0 THEN t.path ELSE t.path || '#' || t.sub END = bookmarks.path
           AND NOT EXISTS (SELECT 1 FROM tracks x WHERE x.id = bookmarks.track_id)",
        [],
    )?;
    Ok(Some(relinked))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn
    }

    fn track(conn: &Connection, id: i64, path: &str, sub: u16) {
        conn.execute(
            "INSERT INTO tracks (id, source, path, sub, title, artist, album_artist, album, genre,
                                 year, disc_no, track_no, duration_ms, size, mtime)
             VALUES (?1, 'local', ?2, ?3, 'T', 'A', 'A', 'L', '', 2000, 1, 1, 200000, 1, 1)",
            params![id, path, sub],
        )
        .unwrap();
    }

    #[test]
    fn marks_list_per_track_in_position_order() {
        let conn = db();
        track(&conn, 1, "/a.flac", 0);
        track(&conn, 2, "/b.flac", 0);
        let late = add(&conn, 1, "/a.flac", 90_000, "Chorus", None).unwrap();
        let early = add(&conn, 1, "/a.flac", 5_000, "", Some("#ff0000")).unwrap();
        add(&conn, 2, "/b.flac", 1_000, "", None).unwrap();
        let marks = for_track(&conn, 1).unwrap();
        assert_eq!(
            marks.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![early, late]
        );
        assert_eq!(marks[0].color.as_deref(), Some("#ff0000"));
        assert_eq!(marks[1].color, None);
        assert_eq!(marks[1].name, "Chorus");
        assert_eq!(count(&conn).unwrap(), 3);
    }

    #[test]
    fn edits_land_and_a_blank_color_reads_as_accent() {
        let conn = db();
        track(&conn, 1, "/a.flac", 0);
        let id = add(&conn, 1, "/a.flac", 5_000, "", None).unwrap();
        rename(&conn, id, "  Drop  ").unwrap();
        set_color(&conn, id, Some("#00ff00")).unwrap();
        set_position(&conn, id, 7_500).unwrap();
        let mark = get(&conn, id).unwrap().unwrap();
        assert_eq!(mark.name, "Drop");
        assert_eq!(mark.color.as_deref(), Some("#00ff00"));
        assert_eq!(mark.position_ms, 7_500);
        set_color(&conn, id, None).unwrap();
        assert_eq!(get(&conn, id).unwrap().unwrap().color, None);
        remove(&conn, id).unwrap();
        assert!(get(&conn, id).unwrap().is_none());
    }

    #[test]
    fn a_track_s_marks_count_and_clear_together() {
        let conn = db();
        track(&conn, 1, "/a.flac", 0);
        track(&conn, 2, "/b.flac", 0);
        add(&conn, 1, "/a.flac", 1_000, "", None).unwrap();
        add(&conn, 1, "/a.flac", 2_000, "", None).unwrap();
        add(&conn, 2, "/b.flac", 3_000, "", None).unwrap();
        assert_eq!(count_for_tracks(&conn, &[1]).unwrap(), 2);
        assert_eq!(count_for_tracks(&conn, &[1, 2]).unwrap(), 3);
        assert_eq!(count_for_tracks(&conn, &[9]).unwrap(), 0);
        assert_eq!(remove_for_tracks(&conn, &[1]).unwrap(), 2);
        assert!(for_track(&conn, 1).unwrap().is_empty());
        assert_eq!(for_track(&conn, 2).unwrap().len(), 1);
    }

    #[test]
    fn all_joins_the_catalog_and_skips_dangling_marks() {
        let conn = db();
        track(&conn, 1, "/a.flac", 0);
        add(&conn, 1, "/a.flac", 5_000, "", None).unwrap();
        add(&conn, 9, "/gone.flac", 5_000, "", None).unwrap();
        let rows = all(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/a.flac");
        assert_eq!(rows[0].title, "T");
        assert_eq!(rows[0].duration_ms, 200_000);
    }

    #[test]
    fn a_returned_file_gets_its_marks_back_by_path() {
        let conn = db();
        track(&conn, 1, "/a.flac", 0);
        track(&conn, 2, "/img.flac", 3);
        add(&conn, 1, "/a.flac", 5_000, "", None).unwrap();
        add(&conn, 2, "/img.flac#3", 5_000, "", None).unwrap();
        // Nothing dangling: the sweep is a no-op.
        assert_eq!(reattach(&conn).unwrap(), None);
        conn.execute("DELETE FROM tracks", []).unwrap();
        track(&conn, 11, "/a.flac", 0);
        track(&conn, 12, "/img.flac", 3);
        assert_eq!(reattach(&conn).unwrap(), Some(2));
        assert_eq!(for_track(&conn, 11).unwrap().len(), 1);
        assert_eq!(for_track(&conn, 12).unwrap().len(), 1);
        assert!(for_track(&conn, 1).unwrap().is_empty());
    }
}
