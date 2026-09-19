//! What a station says it's playing. Web radio is the one source where the
//! track changes without the queue moving: one entry plays for hours and
//! the songs inside it turn over in band, announced by Shoutcast metadata
//! the transport strips out of the byte stream. Everything else in rox
//! learns about a new song from the engine opening a new entry, so this is
//! the piece that had to be built rather than inherited.
//!
//! The engine already does the hard half. It publishes each entry's latest
//! title into the shared snapshot, drops repeats, and bumps a revision when
//! one genuinely changed. So this reads that revision on the player's pump
//! clock, takes the title for whatever's audible when it moves, and says so
//! once. Headless like the rest of the services here: it holds state and
//! emits, and refers to no panel.
//!
//! The turnover is also the only end-of-song signal a stream has. A station
//! has no duration and no track boundary, so without this the scrobbler
//! would watch one track for as long as the stream ran and file a single
//! listen for the whole evening.
//!
//! The other half of this is the station itself rather than its songs. A
//! stream answers the connect with a set of `icy-` headers naming the
//! station, its genre, its bitrate and its homepage, and for a URL typed
//! into the add box that is the only description that will ever exist.
//! Playing one is therefore also the moment the library learns what it is,
//! so this writes the empty columns back onto the row and goes looking for
//! a logo at the homepage it named. Once per station per run, and never
//! over a value that was already there.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use gpui::{Context, Entity, EventEmitter, Subscription};

use rox_library::cue::TrackKey;
use rox_library::rusqlite::Connection;
use rox_library::stations::{self, Heard};
use rox_library::store::{self, TrackMeta};
use rox_playback::{IcyTitle, StationInfo};

use crate::catalog::Library;
use crate::player::Player;
use crate::station_art;
use crate::thumbs::Thumbs;

/// A station moved to the next song. The key is the station's, unchanged,
/// which is the whole oddity of the event: the thing playing is the same
/// row it was a moment ago and what it's playing is not.
pub struct TitleChanged {
    pub key: TrackKey,
    pub title: IcyTitle,
}

/// The playing station and the last title it sent. Split out of the entity
/// so the turnover rule can be read and tested as what it is: a comparison
/// against the last title, with no clock and no app around it.
#[derive(Default)]
struct Live {
    key: Option<TrackKey>,
    title: Option<IcyTitle>,
}

impl Live {
    /// Take what the player just said, and answer with the title if this is
    /// a turnover. `None` covers the three quiet cases: a different track
    /// under the cursor, no title yet, and the same title again.
    fn advance(&mut self, key: &TrackKey, title: Option<IcyTitle>) -> Option<IcyTitle> {
        // A different entry took over. Record whatever it already says
        // without calling it a turnover: a stream that opens carrying a
        // title hasn't changed song, it has only just started, and the
        // first real announcement is a second or two behind the open.
        if self.key.as_ref() != Some(key) {
            self.key = Some(key.clone());
            self.title = title;
            return None;
        }

        // Stations resend the current title in every metadata block,
        // several times a minute. Only a different one is the next song,
        // and a keepalive that scrobbled would double every play.
        let title = title?;
        if self.title.as_ref() == Some(&title) {
            return None;
        }

        self.title = Some(title.clone());
        Some(title)
    }

    /// Forget the station, for a player with nothing playing. True when
    /// there was something to forget, so the caller knows to notify.
    fn clear(&mut self) -> bool {
        let held = self.key.is_some() || self.title.is_some();
        self.key = None;
        self.title = None;
        held
    }
}

/// The live-title service: one per player, following what the stations it
/// plays announce.
pub struct Radio {
    live: Live,
    /// The title revision this last acted on. Polling an atomic on the
    /// pump's clock costs nothing; taking the title lock sixty times a
    /// second for an answer that changes once a song would not.
    seen_rev: u64,
    /// The library, for writing back what a station said about itself.
    library: Entity<Library>,
    /// The thumbnail service, for filing a station's logo under its URL
    /// and then telling the texture cache to look again.
    thumbs: Entity<Thumbs>,
    /// Stations already answered for this run, by stream URL. The headers
    /// don't change between plays, so the write-back and the logo fetch
    /// are each worth exactly one attempt: a station whose homepage has no
    /// icon would otherwise go back for it on every play all evening.
    described: HashSet<String>,
    _player_changed: Subscription,
}

impl EventEmitter<TitleChanged> for Radio {}

impl Radio {
    pub fn new(
        player: &Entity<Player>,
        library: &Entity<Library>,
        thumbs: &Entity<Thumbs>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The player's pump notifies every tick while a session runs, the
        // same clock the scrobbler rides.
        let _player_changed = cx.observe(player, |this: &mut Self, player, cx| {
            this.tick(&player, cx);
        });

        Radio {
            live: Live::default(),
            seen_rev: 0,
            library: library.clone(),
            thumbs: thumbs.clone(),
            described: HashSet::new(),
            _player_changed,
        }
    }

    /// What the playing stream says is on, or None for anything that isn't
    /// a station or hasn't announced yet. The transport and the info panels
    /// show this in place of the row's own title, which names the station
    /// rather than the song.
    pub fn live_title(&self) -> Option<IcyTitle> {
        self.live.title.clone()
    }

    /// The station the live title belongs to, so a reader can tell a stale
    /// title from one about the track it's drawing.
    pub fn station(&self) -> Option<&TrackKey> {
        self.live.key.as_ref()
    }

    fn tick(&mut self, player: &Entity<Player>, cx: &mut Context<Self>) {
        let player = player.read(cx);

        let Some(now) = player.now_playing() else {
            // Nothing playing, so the next station starts clean instead of
            // inheriting the last one's song.
            if self.live.clear() {
                cx.notify();
            }
            return;
        };

        // Two things are worth a look: a title that moved, and a different
        // entry under the cursor. Every other tick is the overwhelming
        // majority and costs one atomic load.
        let rev = player.title_rev().unwrap_or(0);
        let switched = self.live.key.as_ref() != Some(&now.key);
        if rev == self.seen_rev && !switched {
            return;
        }
        self.seen_rev = rev;

        // Both reads happen here, before anything is acted on: the player
        // borrow ends with them, and what follows takes the app mutably to
        // write the library and spawn.
        let info = player.station_info();
        let title = player.live_title();

        // The description arrives on the same revision the titles ride, so
        // this is where it's noticed. It's published once at the open, and
        // what follows writes to the library and goes to the network, so
        // the station is marked before either runs.
        if let Some(info) = info {
            self.describe(&now.key, &info, cx);
        }

        if let Some(title) = self.live.advance(&now.key, title) {
            cx.emit(TitleChanged {
                key: now.key,
                title,
            });
            cx.notify();
        }
    }

    /// Take what the stream said about itself: fill in the row's empty
    /// columns and go find the station a picture. Once per station per run.
    ///
    /// Stations only. A Subsonic server answers no `icy-` headers, so
    /// nothing should ever get here for one, but the row write and the art
    /// key are both scoped by the source anyway rather than trusting that.
    fn describe(&mut self, key: &TrackKey, info: &StationInfo, cx: &mut Context<Self>) {
        if &*key.source != stations::SOURCE {
            return;
        }

        let url = key.path.to_string_lossy().to_string();
        if !self.described.insert(url.clone()) {
            return;
        }

        self.fill_row(&url, info, cx);
        self.find_logo(&url, info, cx);
    }

    /// Write what the stream said into whichever of the row's genre, codec
    /// and bitrate columns are still empty, and rebuild the projection when
    /// something actually landed. A station typed in as a bare URL picks up
    /// its whole second line this way, the first time it plays.
    fn fill_row(&mut self, url: &str, info: &StationInfo, cx: &mut Context<Self>) {
        let heard = heard_from(info);
        if heard == Heard::default() {
            return;
        }

        let path = self.library.read(cx).db_path();
        let Ok(conn) = store::open(&path) else {
            return;
        };

        match stations::fill_empty(&conn, url, &heard) {
            Ok(true) => self
                .library
                .update(cx, |library, cx| library.reload_projection(cx)),

            Ok(false) => {}

            Err(e) => log::warn!("radio: recording what {url} said failed: {e}"),
        }
    }

    /// Go looking for the station's logo at the homepage it named, for a
    /// station the thumbnail store holds no picture for. A station added
    /// through the directory already has one filed under this same key, so
    /// the common case never leaves the machine.
    ///
    /// Silent either way. The headers gave us a homepage, not a logo, and
    /// `/favicon.ico` is a guess: plenty of sites don't have one and
    /// nothing about that is worth telling anybody.
    fn find_logo(&mut self, url: &str, info: &StationInfo, cx: &mut Context<Self>) {
        let Some(favicon) = station_art::favicon_url(&info.homepage) else {
            return;
        };
        let Some(conn) = self.thumbs.read(cx).store_conn() else {
            return;
        };

        let key = url.to_string();
        let thumbs = self.thumbs.clone();

        // Back to the main thread once the logo is filed, because the row
        // for this station is very likely on screen: it painted before the
        // fetch finished, was told there was no art, and that answer is
        // cached as definitive.
        cx.spawn(async move |_, cx| {
            let stored = cx
                .background_executor()
                .spawn(async move {
                    if has_art(&conn, &key) {
                        return None;
                    }

                    station_art::fetch_and_store(&favicon, &key, &conn).then_some(key)
                })
                .await;

            let Some(key) = stored else {
                return;
            };

            thumbs
                .update(cx, |thumbs, cx| {
                    thumbs.forget(Path::new(&key), cx);
                })
                .ok();
        })
        .detach();
    }
}

/// Whether the thumbnail store already holds a picture under `key`. A
/// station's URL never stats as a file, so this is the pooled answer and
/// nothing else, which is exactly the question: has anything ever filed a
/// logo for this station.
///
/// Takes the store lock. Background executor only.
fn has_art(conn: &Mutex<Connection>, key: &str) -> bool {
    rox_library::thumbs::thumbnail(conn, Path::new(key)).is_some()
}

/// What a stream's headers say in the terms the library's columns are in.
/// The genre is the station's own word for itself, and the codec comes off
/// the `Content-Type` through the same mapping the probe hint uses, so a
/// row records the container the transport decided to decode as.
fn heard_from(info: &StationInfo) -> Heard {
    Heard {
        genre: info.genre.trim().to_string(),
        codec: rox_playback::http::extension_for(&info.content_type)
            .unwrap_or_default()
            .to_string(),
        bitrate_kbps: info.bitrate_kbps,
    }
}

/// The tags a station play records under once the stream has said what's
/// on. The song's artist and title come off the metadata band; the album
/// stays the station's own name, because that's what a listen against a
/// station reads as and there is no album to name. Everything else is the
/// row's, untouched, and so is the row id beside these: the thing in the
/// library that played is the station, whatever song went past inside it.
///
/// A station that sends one unsplittable field leaves the artist empty.
/// Last.fm won't take a track without one, so those watch silently, the
/// same way an untagged file already does.
pub fn live_tags(station: Option<TrackMeta>, title: &IcyTitle) -> TrackMeta {
    let mut meta = station.unwrap_or(TrackMeta {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        track_no: 0,
        album_artist: String::new(),
        year: 0,
        genre: String::new(),
        duration_ms: 0,
        codec: String::new(),
        bitrate_kbps: 0,
        sample_rate_hz: 0,
        bit_depth: 0,
        rating: 0,
    });

    // The row's title names the station, which is the closest thing a
    // stream has to an album.
    meta.album = std::mem::take(&mut meta.title);
    meta.title = title.title.clone();
    meta.artist = title.artist.clone();
    // A stream has no length, and a length carried over off the row would
    // be a lie the listen rule then divides by.
    meta.duration_ms = 0;

    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn station_key() -> TrackKey {
        TrackKey {
            source: rox_library::cue::source_id(rox_library::stations::SOURCE),
            path: PathBuf::from("https://host/jazz"),
            sub: 0,
        }
    }

    fn title(artist: &str, name: &str) -> IcyTitle {
        IcyTitle {
            artist: artist.to_string(),
            title: name.to_string(),
        }
    }

    fn station_meta(name: &str) -> TrackMeta {
        TrackMeta {
            title: name.to_string(),
            artist: String::new(),
            album: String::new(),
            track_no: 0,
            album_artist: String::new(),
            year: 0,
            genre: "Jazz".into(),
            duration_ms: 0,
            codec: String::new(),
            bitrate_kbps: 0,
            sample_rate_hz: 0,
            bit_depth: 0,
            rating: 0,
        }
    }

    /// The station coming up is not a turnover, and neither is the title it
    /// arrives carrying. The next one is.
    #[test]
    fn a_turnover_announces_once() {
        let mut live = Live::default();
        let key = station_key();

        assert_eq!(live.advance(&key, None), None, "the stream just opened");
        assert_eq!(
            live.advance(&key, Some(title("Miles Davis", "So What"))),
            Some(title("Miles Davis", "So What")),
            "the first announcement is the song starting"
        );
        assert_eq!(
            live.advance(&key, Some(title("Bill Evans", "Peace Piece"))),
            Some(title("Bill Evans", "Peace Piece"))
        );
    }

    /// A station resends the current title on a keepalive, several times a
    /// minute. Without this the evening scrobbles a hundred times.
    #[test]
    fn the_same_title_twice_announces_once() {
        let mut live = Live::default();
        let key = station_key();
        let on_air = title("Miles Davis", "So What");

        live.advance(&key, None);
        assert_eq!(
            live.advance(&key, Some(on_air.clone())),
            Some(on_air.clone())
        );
        assert_eq!(live.advance(&key, Some(on_air.clone())), None);
        assert_eq!(live.advance(&key, Some(on_air)), None);
    }

    /// A different entry takes whatever it already says without announcing:
    /// a track change is the queue's event, and doubling it here would
    /// re-arm a watch that just began.
    #[test]
    fn a_track_change_is_not_a_turnover() {
        let mut live = Live::default();
        let key = station_key();
        live.advance(&key, Some(title("Miles Davis", "So What")));

        let other = TrackKey::from(PathBuf::from("/music/track.flac"));
        assert_eq!(live.advance(&other, Some(title("A", "B"))), None);
        assert_eq!(live.live_title_for_test(), Some(title("A", "B")));

        // And back to the station: still not a turnover, still adopted.
        assert_eq!(live.advance(&key, None), None);
        assert_eq!(live.live_title_for_test(), None);
    }

    /// Nothing playing clears the station, so the next stream doesn't
    /// inherit the last one's song.
    #[test]
    fn a_stopped_player_forgets_the_station() {
        let mut live = Live::default();
        let key = station_key();
        live.advance(&key, Some(title("Miles Davis", "So What")));

        assert!(live.clear(), "there was a station to forget");
        assert!(!live.clear(), "and nothing to forget twice");

        // The same title after the clear announces again: this is a fresh
        // play of it, not the keepalive of the old one.
        assert_eq!(live.advance(&key, None), None);
        assert_eq!(
            live.advance(&key, Some(title("Miles Davis", "So What"))),
            Some(title("Miles Davis", "So What"))
        );
    }

    /// What a turnover records: the new song's tags over the station's row,
    /// with the station's name kept as the album. The row id isn't here
    /// because nothing about a turnover moves it, which is the point.
    #[test]
    fn a_turnover_records_the_new_song_under_the_station() {
        let station = station_meta("Jazz Forever");
        let tags = live_tags(Some(station), &title("Miles Davis", "So What"));

        assert_eq!(tags.title, "So What");
        assert_eq!(tags.artist, "Miles Davis");
        assert_eq!(tags.album, "Jazz Forever", "the station stands in");
        assert_eq!(tags.genre, "Jazz", "the row's other tags carry over");
        assert_eq!(tags.duration_ms, 0, "a stream still has no length");
    }

    /// A station the library holds no row for still records what the stream
    /// said, so a URL played from outside the library scrobbles.
    #[test]
    fn a_turnover_without_a_row_still_carries_the_song() {
        let tags = live_tags(None, &title("Miles Davis", "So What"));

        assert_eq!(tags.title, "So What");
        assert_eq!(tags.artist, "Miles Davis");
        assert!(tags.album.is_empty());
    }

    fn described(content_type: &str) -> Heard {
        heard_from(&StationInfo {
            genre: " Jazz ".into(),
            bitrate_kbps: 128,
            content_type: content_type.into(),
            ..StationInfo::default()
        })
    }

    /// The codec a row records is the one the transport decided to decode
    /// as, which is why this goes through the probe's own mapping rather
    /// than a second table that could disagree with it. The parameters
    /// stations hang off the header ("audio/mpeg; charset=UTF-8") are part
    /// of the shape, not an edge case.
    #[test]
    fn the_content_type_names_the_codec() {
        assert_eq!(described("audio/mpeg").codec, "mp3");
        assert_eq!(described("audio/mpeg; charset=UTF-8").codec, "mp3");
        assert_eq!(described("audio/aac").codec, "aac");
        assert_eq!(described("audio/aacp").codec, "aac");
        assert_eq!(described("application/ogg").codec, "ogg");
        assert_eq!(described("audio/ogg").codec, "ogg");

        // A server that answers with a generic type, or with nothing, says
        // nothing about the codec, and an empty column is the honest
        // record of that.
        assert_eq!(described("application/octet-stream").codec, "");
        assert_eq!(described("").codec, "");
    }

    /// The genre comes across trimmed, since stations pad the header, and
    /// the bitrate comes across as stated. A station that sends none of it
    /// reduces to nothing to write, which is what stops the row being
    /// rewritten on every play.
    #[test]
    fn a_station_that_says_nothing_has_nothing_to_record() {
        let heard = described("audio/mpeg");
        assert_eq!(heard.genre, "Jazz");
        assert_eq!(heard.bitrate_kbps, 128);

        assert_eq!(heard_from(&StationInfo::default()), Heard::default());
    }

    impl Live {
        /// The held title, for the tests above; the entity reads it through
        /// [`Radio::live_title`].
        fn live_title_for_test(&self) -> Option<IcyTitle> {
            self.title.clone()
        }
    }
}
