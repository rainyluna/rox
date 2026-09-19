//! Web radio stations, which are ordinary track rows and deliberately
//! nothing more. A station is one row under `source = 'radio'` with the
//! stream URL in `path` and in `remote_url`, and `remote_live` set, so the
//! queue, the playlists, the play history, search and every existing panel
//! carry it without a line of new code. The alternative was a side table
//! and a second play path, and that buys a schema at the cost of
//! reimplementing all of the above.
//!
//! What a station doesn't get falls out of what it is. There's no duration,
//! because the stream has no end, so the row lands at `duration_ms = 0` and
//! the seek strip prints `-:--`. There's no gapless boundary, because
//! nothing follows a stream that never finishes. There's no ReplayGain
//! figure, because nothing measured it and nothing can. The codec and
//! bitrate columns start empty too: those come off the response headers at
//! open time, not off a catalog, so they stay blank until the station has
//! been played once and [`fill_empty`] writes back what it said.
//!
//! The URL is the identity. Adding the same stream twice updates the name
//! rather than growing a second row, which is what
//! `ON CONFLICT (source, path, sub)` already does for every source.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::TrackRow;
use crate::locator::{Locator, Remote};
use crate::playlist_file::Format;
use crate::store;

/// The source string every station row is filed under. One source for all
/// of them: unlike a Subsonic server, there's no account behind a station
/// and nothing to tell two of them apart by.
pub const SOURCE: &str = "radio";

/// One station as the user entered it. The URL is the identity; the name
/// is what the library shows, and the genre is the one tag a station
/// directory ever really carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Station {
    pub url: String,
    pub name: String,
    pub genre: String,
}

/// What a stream says about itself past its name: the genre it announces,
/// the codec its content type implies, and the bitrate it states. These
/// are the three columns a station row cannot have until something has
/// connected to it, which is why the header says they stay empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Heard {
    pub genre: String,
    pub codec: String,
    pub bitrate_kbps: u32,
}

/// Add or update stations. The URL goes in twice, as the row's path (its
/// identity) and as `remote_url` (where the bytes come from), because a
/// remote row's path is whatever its source calls the track and for radio
/// those are the same string.
///
/// A station with no name takes the URL's last segment, the same fallback
/// [`Locator::label`] gives every other untagged remote track, so nothing
/// ever lands in the library as a blank row.
pub fn put(conn: &mut Connection, stations: &[Station]) -> rusqlite::Result<()> {
    let rows: Vec<TrackRow> = stations.iter().map(row_for).collect();

    store::upsert_source_rows(conn, SOURCE, &rows)
}

/// Drop one station. Through [`store::prune_source`] rather than a delete
/// of its own: that call is already scoped to the source, so a URL that
/// happens to read like a path on disk can't reach a local row from here.
pub fn remove(conn: &mut Connection, url: &str) -> rusqlite::Result<()> {
    let keep: HashSet<String> = all(conn)?
        .into_iter()
        .map(|station| station.url)
        .filter(|held| held != url)
        .collect();

    store::prune_source(conn, SOURCE, &keep)?;
    Ok(())
}

/// Every station in the library, by name so the list doesn't reshuffle
/// between reads. The URL breaks ties, since two stations are allowed to
/// share a name and only the URL is unique.
pub fn all(conn: &Connection) -> rusqlite::Result<Vec<Station>> {
    let mut stmt = conn.prepare(
        "SELECT path, title, genre FROM tracks
          WHERE source = ?1 ORDER BY title, path",
    )?;

    let rows = stmt.query_map([SOURCE], |r| {
        Ok(Station {
            url: r.get(0)?,
            name: r.get(1)?,
            genre: r.get(2)?,
        })
    })?;

    rows.collect()
}

/// Every station with what's known about its stream, in the same order
/// [`all`] gives. Two reads rather than one because most callers want the
/// list and nothing else: only the panel that draws a row's second line
/// cares which codec came back off the wire.
pub fn detailed(conn: &Connection) -> rusqlite::Result<Vec<(Station, Heard)>> {
    let mut stmt = conn.prepare(
        "SELECT path, title, genre, codec, bitrate FROM tracks
          WHERE source = ?1 ORDER BY title, path",
    )?;

    let rows = stmt.query_map([SOURCE], |r| {
        Ok((
            Station {
                url: r.get(0)?,
                name: r.get(1)?,
                genre: r.get(2)?,
            },
            Heard {
                genre: r.get(2)?,
                codec: r.get(3)?,
                bitrate_kbps: r.get(4)?,
            },
        ))
    })?;

    rows.collect()
}

/// Write what the stream said into the columns that are still empty, and
/// leave every column that isn't. True when something actually landed, so
/// the caller knows whether the projection needs rebuilding.
///
/// The precedence is the whole point of this being an update rather than a
/// [`put`]. A name typed into the add box, a genre a directory filled in,
/// a codec corrected by hand: all of those are somebody's decision, and a
/// station that announces "Various" every time it connects would overwrite
/// them on every play. The stream only gets to answer the questions the
/// row has no answer to.
pub fn fill_empty(conn: &Connection, url: &str, heard: &Heard) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE tracks
            SET genre = CASE WHEN genre = '' THEN ?3 ELSE genre END,
                codec = CASE WHEN codec = '' THEN ?4 ELSE codec END,
                bitrate = CASE WHEN bitrate = 0 THEN ?5 ELSE bitrate END
          WHERE source = ?1 AND path = ?2
            AND ((genre = '' AND ?3 <> '')
              OR (codec = '' AND ?4 <> '')
              OR (bitrate = 0 AND ?5 <> 0))",
        rusqlite::params![SOURCE, url, heard.genre, heard.codec, heard.bitrate_kbps],
    )?;

    Ok(changed > 0)
}

/// Read stations out of a playlist file, which is how everyone already has
/// their stations: a `.pls` or an `.m3u` of stream URLs, handed around or
/// downloaded from a station's own site. Typing them in one at a time is
/// the reason a feature like this goes unused.
///
/// The shared readers in [`crate::playlist_file`] answer with URLs alone,
/// and a station without its name is half an import, so the walk here is
/// its own: an `#EXTINF` title or a `Title<n>` key names the entry that
/// follows it. Anything that isn't an http URL is dropped rather than
/// turned into a station, so a normal playlist of local files imports as
/// nothing instead of as a list of streams that can't play.
pub fn import(text: &str) -> Vec<Station> {
    // Windows tools save UTF-8 with a BOM, and left on it clings to the
    // first line, which is the same trap the m3u and pls readers strip for.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    match Format::sniff(text) {
        Format::Pls => from_pls(text),

        Format::M3u => from_m3u(text),

        // XSPF carries its titles inside the XML, so there's no name pass
        // to run here. The shared reader's URLs are the honest answer.
        Format::Xspf => crate::xspf::parse(text)
            .iter()
            .filter_map(|url| station(url, ""))
            .collect(),
    }
}

/// An extended M3U, read as name-then-URL pairs. A title only counts for
/// the next entry, so a dropped local path never lends its name to the URL
/// after it.
fn from_m3u(text: &str) -> Vec<Station> {
    let mut out = Vec::new();
    let mut pending = String::new();

    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }

        // `#EXTINF:<secs>,<name>`: the duration is meaningless for a
        // stream (it writes -1) and the name is the whole point.
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = rest
                .split_once(',')
                .map(|(_, name)| name.trim().to_string())
                .unwrap_or_default();
            continue;
        }

        if line.starts_with('#') {
            continue;
        }

        out.extend(station(line, &pending));
        pending.clear();
    }

    out
}

/// A PLS file, read as `File<n>`/`Title<n>` pairs in entry-number order.
/// The numbers are what the format says to trust, not the line order, so a
/// hand-written file that lists entry 2 first still pairs correctly.
fn from_pls(text: &str) -> Vec<Station> {
    let mut urls: Vec<(u32, String)> = Vec::new();
    let mut names: Vec<(u32, String)> = Vec::new();

    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };

        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }

        // The Winamp lineage wrote File1, file1 and FILE1 interchangeably,
        // so both key readings are case-insensitive.
        if let Some(index) = numbered(key, "file") {
            urls.push((index, value.to_string()));
        } else if let Some(index) = numbered(key, "title") {
            names.push((index, value.to_string()));
        }
    }

    urls.sort_by_key(|(index, _)| *index);

    urls.iter()
        .filter_map(|(index, url)| {
            let name = names
                .iter()
                .find(|(at, _)| at == index)
                .map(|(_, name)| name.as_str())
                .unwrap_or("");

            station(url, name)
        })
        .collect()
}

/// The entry number behind a `File7` or `Title7` key, None for anything
/// else in the file.
fn numbered(key: &str, prefix: &str) -> Option<u32> {
    let head = key.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }

    key[prefix.len()..].trim().parse().ok()
}

/// One import entry as a station, or None for a line that isn't a stream.
/// A local path, a `file://` URL and an `mms://` one all come back None:
/// rox opens stations over HTTP, so anything else would import as a row
/// that can never play.
fn station(url: &str, name: &str) -> Option<Station> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return None;
    }

    Some(Station {
        url: url.to_string(),
        name: name.trim().to_string(),
        genre: String::new(),
    })
}

/// The row a station lands as. Everything a file row carries off its tags
/// is empty here, including the album, which keeps stations out of the
/// album rollups that count distinct non-empty albums.
fn row_for(station: &Station) -> TrackRow {
    let title = if station.name.trim().is_empty() {
        label_of(&station.url)
    } else {
        station.name.trim().to_string()
    };

    TrackRow {
        path: station.url.clone(),
        sub: 0,
        cue: None,
        remote_url: station.url.clone(),
        remote_live: true,
        title,
        artist: String::new(),
        album_artist: String::new(),
        album: String::new(),
        title_sort: String::new(),
        artist_sort: String::new(),
        album_artist_sort: String::new(),
        album_sort: String::new(),
        genre: station.genre.clone(),
        year: 0,
        disc_no: 0,
        track_no: 0,
        // No end, so no length. The seek strip reads the missing duration
        // and prints `-:--` without being told anything about radio.
        duration_ms: 0,
        codec: String::new(),
        bitrate_kbps: 0,
        sample_rate_hz: 0,
        bit_depth: 0,
        rating: 0,
        replay_gain: Default::default(),
        bpm: None,
        size: 0,
        mtime: 0,
    }
}

/// The name a URL stands in with, through the locator's own fallback so a
/// station reads the same way any other untagged remote track does.
fn label_of(url: &str) -> String {
    Locator::Remote(Remote {
        url: url.to_string(),
        headers: Vec::new(),
        hint: String::new(),
        live: true,
    })
    .label()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    fn station_named(url: &str, name: &str) -> Station {
        Station {
            url: url.to_string(),
            name: name.to_string(),
            genre: String::new(),
        }
    }

    /// The URL is the identity, so the same stream added again is an edit
    /// of the row that's already there.
    #[test]
    fn the_same_url_twice_is_one_row_with_the_newer_name() {
        let mut conn = db();

        put(&mut conn, &[station_named("https://host/jazz", "Jazz")]).unwrap();
        put(
            &mut conn,
            &[station_named("https://host/jazz", "Jazz Forever")],
        )
        .unwrap();

        let held = all(&conn).unwrap();
        assert_eq!(held.len(), 1, "one row per URL");
        assert_eq!(held[0].name, "Jazz Forever");
    }

    /// Removing one station leaves the rest where they were.
    #[test]
    fn remove_takes_only_its_own_row() {
        let mut conn = db();

        put(
            &mut conn,
            &[
                station_named("https://host/jazz", "Jazz"),
                station_named("https://host/soul", "Soul"),
            ],
        )
        .unwrap();

        remove(&mut conn, "https://host/jazz").unwrap();

        let held = all(&conn).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].url, "https://host/soul");
    }

    /// A station's path is a URL and a file's is a path, but nothing stops
    /// the two strings from being equal. The source on the key is what
    /// keeps them apart, seen from the stations side.
    #[test]
    fn a_station_and_a_local_file_can_share_a_path_string() {
        let mut conn = db();

        let shared = "https://host/jazz";
        store::insert_batch(
            &mut conn,
            &[TrackRow {
                title: "A local file that happens to be named this".into(),
                ..row_for(&station_named(shared, "Jazz"))
            }],
        )
        .unwrap();

        put(&mut conn, &[station_named(shared, "Jazz")]).unwrap();

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracks WHERE path = ?1",
                [shared],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "one row per source");

        // And the stations side sees only its own.
        let held = all(&conn).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].name, "Jazz");
    }

    /// A station row carries the live flag and no duration, which is what
    /// every downstream reader keys off.
    #[test]
    fn a_station_row_is_live_and_lengthless() {
        let mut conn = db();

        put(&mut conn, &[station_named("https://host/jazz", "Jazz")]).unwrap();

        let id = store::id_for_path(&conn, SOURCE, "https://host/jazz")
            .unwrap()
            .unwrap();
        let locator = store::locators_for(&conn, &[id]).unwrap().pop().unwrap();

        assert_eq!(
            locator,
            Locator::Remote(Remote {
                url: "https://host/jazz".into(),
                headers: Vec::new(),
                hint: String::new(),
                live: true,
            })
        );

        let duration: i64 = conn
            .query_row("SELECT duration_ms FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(duration, 0);
    }

    /// Where a station lands in the library's rollups, checked rather than
    /// assumed. The folder count and the per-folder rollup are scoped to
    /// local rows and don't see stations, which is what a station being
    /// lengthless and sizeless needs. The whole-library counts are scoped
    /// to nothing, so a station reads as one more track carrying no gain
    /// and no tempo. Whether those three want a scope is store.rs's call,
    /// not this module's; it's pinned here so the next reader doesn't have
    /// to go find out.
    #[test]
    fn a_station_in_the_library_rollups() {
        let mut conn = db();

        store::insert_batch(
            &mut conn,
            &[TrackRow {
                path: "/music/one.flac".into(),
                album: "An Album".into(),
                size: 1_000,
                ..row_for(&station_named("https://host/jazz", "Jazz"))
            }],
        )
        .unwrap();
        put(&mut conn, &[station_named("https://host/jazz", "Jazz")]).unwrap();

        let stats = store::stats(&conn).unwrap();
        assert_eq!(stats.dirs, 1, "a station has no folder to watch");
        assert_eq!(stats.albums, 1, "and no album to count");
        assert_eq!(stats.bytes, 1_000, "and no bytes on disk");
        assert_eq!(stats.tracks, 2, "but it does count as a track");

        // The coverage splits count every row too, so a station sits in
        // the missing bucket of both.
        assert_eq!(store::replaygain_breakdown(&conn).unwrap().missing, 2);
        assert_eq!(store::bpm_breakdown(&conn).unwrap().missing, 2);
    }

    /// An unnamed station takes the URL's last segment rather than landing
    /// in the library as a blank row.
    #[test]
    fn an_unnamed_station_falls_back_to_the_url() {
        let mut conn = db();

        put(&mut conn, &[station_named("https://host/radio/jazz", "")]).unwrap();

        assert_eq!(all(&conn).unwrap()[0].name, "jazz");
    }

    /// The PLS shape a station directory hands out: File and Title pairs,
    /// matched by entry number.
    #[test]
    fn import_reads_a_pls() {
        let text = "[playlist]\n\
                    NumberOfEntries=2\n\
                    File1=https://host/jazz\n\
                    Title1=Jazz Forever\n\
                    Length1=-1\n\
                    File2=https://host/soul\n\
                    Title2=Soul Kitchen\n\
                    Length2=-1\n\
                    Version=2\n";

        assert_eq!(
            import(text),
            vec![
                station_named("https://host/jazz", "Jazz Forever"),
                station_named("https://host/soul", "Soul Kitchen"),
            ]
        );
    }

    /// The m3u shape, where the name rides the `#EXTINF` line above the URL
    /// and the duration is the -1 a stream always writes.
    #[test]
    fn import_reads_an_m3u() {
        let text = "#EXTM3U\n\
                    #EXTINF:-1,Jazz Forever\n\
                    https://host/jazz\n\
                    #EXTINF:-1,Soul Kitchen\n\
                    https://host/soul\n";

        assert_eq!(
            import(text),
            vec![
                station_named("https://host/jazz", "Jazz Forever"),
                station_named("https://host/soul", "Soul Kitchen"),
            ]
        );
    }

    /// A playlist of local files imports as nothing, and a mixed one
    /// imports only its streams. The dropped entry doesn't hand its name
    /// down to the URL that follows it either.
    #[test]
    fn import_rejects_everything_that_is_not_a_stream() {
        let mixed = "#EXTM3U\n\
                     #EXTINF:210,A Local Song\n\
                     /music/artist/song.flac\n\
                     #EXTINF:-1,Jazz Forever\n\
                     https://host/jazz\n\
                     #EXTINF:-1,Old Windows Stream\n\
                     mms://host/legacy\n";

        assert_eq!(
            import(mixed),
            vec![station_named("https://host/jazz", "Jazz Forever")]
        );

        let all_local = "/music/one.flac\n/music/two.flac\n";
        assert!(import(all_local).is_empty());

        let pls_local = "[playlist]\nFile1=C:\\Music\\one.mp3\nTitle1=One\n";
        assert!(import(pls_local).is_empty());
    }

    /// An import goes straight into the library, names and all.
    #[test]
    fn imported_stations_land_as_rows() {
        let mut conn = db();

        let stations = import("#EXTM3U\n#EXTINF:-1,Jazz Forever\nhttps://host/jazz\n");
        put(&mut conn, &stations).unwrap();

        assert_eq!(
            all(&conn).unwrap(),
            vec![station_named("https://host/jazz", "Jazz Forever")]
        );
    }

    /// The row starts with three empty columns and the stream fills them,
    /// which is the whole reason anything reads the response headers.
    #[test]
    fn the_stream_fills_the_columns_the_row_left_empty() {
        let mut conn = db();
        put(
            &mut conn,
            &[station_named("https://host/jazz", "Jazz Forever")],
        )
        .unwrap();

        let heard = Heard {
            genre: "Jazz".into(),
            codec: "mp3".into(),
            bitrate_kbps: 128,
        };
        assert!(fill_empty(&conn, "https://host/jazz", &heard).unwrap());

        let (station, filled) = detailed(&conn).unwrap().pop().expect("the one station");
        assert_eq!(station.genre, "Jazz");
        assert_eq!(filled, heard);

        // Nothing left to fill, so a second connect writes nothing and the
        // projection doesn't get rebuilt for a station that said the same
        // thing it said an hour ago.
        assert!(!fill_empty(&conn, "https://host/jazz", &heard).unwrap());
    }

    /// What the user typed wins. A station that announces "Various" on
    /// every connect would otherwise walk over a genre somebody chose.
    #[test]
    fn what_the_row_already_says_survives_the_stream() {
        let mut conn = db();
        put(
            &mut conn,
            &[Station {
                url: "https://host/jazz".into(),
                name: "Jazz Forever".into(),
                genre: "Bebop".into(),
            }],
        )
        .unwrap();

        let heard = Heard {
            genre: "Various".into(),
            codec: "mp3".into(),
            bitrate_kbps: 128,
        };
        assert!(fill_empty(&conn, "https://host/jazz", &heard).unwrap());

        let (station, filled) = detailed(&conn).unwrap().pop().expect("the one station");
        assert_eq!(station.genre, "Bebop", "the typed genre stands");
        assert_eq!(filled.codec, "mp3", "and the empty columns still fill");
        assert_eq!(filled.bitrate_kbps, 128);
    }

    /// A station that says nothing writes nothing. Without the guard the
    /// update would report a change on every connect and rebuild the
    /// projection for it.
    #[test]
    fn a_station_that_describes_nothing_writes_nothing() {
        let mut conn = db();
        put(
            &mut conn,
            &[station_named("https://host/jazz", "Jazz Forever")],
        )
        .unwrap();

        assert!(!fill_empty(&conn, "https://host/jazz", &Heard::default()).unwrap());
        assert!(
            !fill_empty(
                &conn,
                "https://host/other",
                &Heard {
                    genre: "Jazz".into(),
                    codec: "mp3".into(),
                    bitrate_kbps: 128,
                }
            )
            .unwrap(),
            "and a URL no row holds is nobody's station"
        );
    }
}
