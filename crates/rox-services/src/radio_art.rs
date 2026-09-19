//! A picture for the song a station is playing. Web radio is the one
//! source with nothing on disk to read a cover out of: the row is a URL,
//! and the song inside it turns over every few minutes with only its
//! artist and title announced. So the art comes from the same enrichment
//! providers the cover picker searches, looked up per turnover and held
//! for as long as that song is on air.
//!
//! Nothing here is written anywhere. The bytes live in memory for the
//! session and are gone when the title moves or the station stops, which
//! is the whole difference from the picker: that one writes a tag after a
//! human confirmed the match, and this one guesses from two strings a
//! station sent and shows the guess behind a blur. Guessing is fine at
//! that stakes and would not be at the other.
//!
//! The lookup is fuzzy, so a candidate has to score against the title
//! before it's shown at all. A station whose art can't be found, or whose
//! metadata is one unsplittable field, falls back to the station's own
//! picture out of the thumbnail pool. [`StationArt`] is that precedence
//! and the staleness rule, held apart from the entity so both can be read
//! and tested without a clock or an app around them.

use rox_net::providers::{self, TrackQuery};
use rox_playback::IcyTitle;

/// The score below which no candidate is close enough to show, the same
/// bar the Discord presence picks its cover art on. A station sends a
/// song's own artist and title, so a near miss here is a different song
/// entirely rather than a reissue of the right one.
const ART_MATCH_BAR: f32 = 0.5;

/// The biggest picture a station's song art will take. The bytes are
/// downscaled to a blurred thumbnail the moment they land and never
/// stored, so anything past this is bandwidth spent on detail that gets
/// thrown away in the same breath.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The search a live title makes: the song's own artist and title, with
/// no album, because a station says what's playing and never what it's
/// from. None when either half is missing, which is the station that
/// sends one unsplittable field and the one that has announced nothing
/// yet; there's no way to tell a right cover from a wrong one on half a
/// name, so those don't search at all.
pub fn query(title: &IcyTitle) -> Option<TrackQuery> {
    let artist = title.artist.trim();
    let name = title.title.trim();
    if artist.is_empty() || name.is_empty() {
        return None;
    }

    Some(TrackQuery {
        artist: artist.to_string(),
        title: name.to_string(),
        album: String::new(),
        duration_secs: None,
    })
}

/// One title's cover, or None for every way it can come to nothing: the
/// art services all switched off, nothing found, nothing found that
/// scores, or a download that failed or came back too big. Blocking, and
/// the shared provider agent's ten second timeout is what bounds it;
/// background executor only.
pub fn lookup(query: &TrackQuery) -> Option<Vec<u8>> {
    // The same gate the picker runs behind. With every art service off,
    // this is a feature the user has turned down and no request goes out.
    if !providers::art_online() {
        return None;
    }

    let candidates = match providers::search_art(query) {
        Ok(candidates) => candidates,
        Err(e) => {
            log::debug!("radio art: search failed: {e}");
            return None;
        }
    };

    // Provider order is by pixel size, which says nothing about whether
    // the cover belongs to this song. Score them and take the closest;
    // below the bar the station's own picture is the better answer. Ties
    // fall to the first, which is the largest, since that's the order the
    // search left them in.
    let chosen = candidates
        .iter()
        .map(|candidate| (candidate, providers::art_confidence(query, candidate)))
        .filter(|(_, score)| *score >= ART_MATCH_BAR)
        .min_by(|(_, a), (_, b)| b.total_cmp(a))
        .map(|(candidate, _)| candidate)?;

    let bytes = match providers::fetch_image(&chosen.full_url) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::debug!("radio art: fetch failed: {e}");
            return None;
        }
    };

    if bytes.len() > MAX_BYTES {
        log::debug!("radio art: {} bytes is past the cap", bytes.len());
        return None;
    }

    Some(bytes)
}

/// What a remote row has to show, and which lookups still count. The two
/// pictures rank: the song on air wins when one was found for it, the
/// row's own falls in behind, and neither leaves the row looking like an
/// untagged file.
#[derive(Default)]
pub struct StationArt {
    /// The cover found for the title on air.
    title: Option<Vec<u8>>,
    /// The row's own picture out of the thumbnail pool: a station's
    /// favicon, a Subsonic song's stored cover.
    station: Option<Vec<u8>>,
    /// Stamps the lookups. A station plays four songs in the time a slow
    /// provider answers for the first, and the cover for a song that went
    /// past is worse than none: it names the wrong thing with total
    /// confidence.
    generation: u64,
}

impl StationArt {
    /// Start a lookup and take the stamp it has to come back under.
    pub fn arm(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// File a finished lookup. False when the stamp is old, which means
    /// the song it was for is no longer on air and nothing should move.
    pub fn land(&mut self, generation: u64, bytes: Option<Vec<u8>>) -> bool {
        if generation != self.generation {
            return false;
        }

        self.title = bytes;
        true
    }

    /// The row's own picture, read once when the row starts playing.
    pub fn set_station(&mut self, bytes: Option<Vec<u8>>) {
        self.station = bytes;
    }

    /// What to show right now.
    pub fn current(&self) -> Option<&[u8]> {
        self.title.as_deref().or(self.station.as_deref())
    }

    /// Forget the row. Bumping the stamp goes with it, so a lookup still
    /// out for the last station's song can't land on the next one.
    pub fn clear(&mut self) {
        self.generation += 1;
        self.title = None;
        self.station = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(artist: &str, name: &str) -> IcyTitle {
        IcyTitle {
            artist: artist.to_string(),
            title: name.to_string(),
        }
    }

    /// A title with both halves searches on exactly those, and nothing
    /// else: a station knows the song, never the record it came off.
    #[test]
    fn a_full_title_searches_on_artist_and_song() {
        let query = query(&title("Miles Davis", "So What")).expect("a search");

        assert_eq!(query.artist, "Miles Davis");
        assert_eq!(query.title, "So What");
        assert!(query.album.is_empty(), "a station names no album");
        assert_eq!(query.duration_secs, None, "and a stream has no length");
    }

    /// Half a name is not enough to tell a right cover from a wrong one,
    /// so the station that sends one unsplittable field doesn't search.
    #[test]
    fn half_a_title_does_not_search() {
        assert!(query(&title("", "So What")).is_none());
        assert!(query(&title("Miles Davis", "")).is_none());
        assert!(query(&title("  ", "  ")).is_none());
    }

    /// The precedence, in order: the song's own cover, then the station's
    /// picture, then nothing.
    #[test]
    fn the_songs_cover_outranks_the_stations() {
        let mut art = StationArt::default();
        assert_eq!(art.current(), None, "a station with neither shows neither");

        art.set_station(Some(b"favicon".to_vec()));
        assert_eq!(art.current(), Some(b"favicon".as_slice()));

        let generation = art.arm();
        assert!(art.land(generation, Some(b"cover".to_vec())));
        assert_eq!(art.current(), Some(b"cover".as_slice()));

        // The next song has no cover anywhere, so the station's picture
        // comes back up rather than the last song's sticking.
        let generation = art.arm();
        assert!(art.land(generation, None));
        assert_eq!(art.current(), Some(b"favicon".as_slice()));
    }

    /// A slow reply for the song before last lands on nothing. Without
    /// the stamp it would overwrite the cover of the song actually on
    /// air, which reads as the app naming the wrong track.
    #[test]
    fn a_reply_for_a_song_that_went_past_is_dropped() {
        let mut art = StationArt::default();
        art.set_station(Some(b"favicon".to_vec()));

        let slow = art.arm();
        let current = art.arm();

        assert!(art.land(current, Some(b"cover".to_vec())));
        assert!(!art.land(slow, Some(b"stale".to_vec())), "the old song");
        assert_eq!(art.current(), Some(b"cover".as_slice()));
    }

    /// Leaving the station drops both pictures and orphans whatever was
    /// still in flight for it.
    #[test]
    fn leaving_the_station_drops_everything() {
        let mut art = StationArt::default();
        art.set_station(Some(b"favicon".to_vec()));
        let pending = art.arm();

        art.clear();
        assert_eq!(art.current(), None);
        assert!(!art.land(pending, Some(b"cover".to_vec())));
        assert_eq!(art.current(), None);
    }
}
