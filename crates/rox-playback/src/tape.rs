//! The last few minutes of a live stream, kept in memory so a broadcast can
//! be paused, resumed, and stepped back through.
//!
//! Radio has no cursor. The server sends what it is sending now, and a client
//! that stops reading gets dropped or throttled, which is why pausing a
//! station used to mean hanging up on it and rejoining at the live edge. A
//! timeshift is the other answer: keep reading the socket whether or not
//! anything is decoding, write the raw container bytes here, and let the
//! decoder read from this at its own cursor. Pause stops the cursor and not
//! the connection, so Play carries on where the listener stopped, and the
//! distance between the cursor and the live edge is how far back they are.
//! YouTube and Twitch behave the same way, and this is the same trick.
//!
//! Two threads share one of these. The network thread appends and never
//! reads; the decode thread reads and never appends. Everything is under one
//! mutex with a condvar on it, because the interesting moment is the decode
//! thread catching up to the live edge and having to wait for bytes that only
//! exist once the socket delivers them. No lock-free structure would help:
//! the waiting is the point.
//!
//! What's stored is the station's own container bytes, untouched, ICY
//! metadata already stripped out by the reader above. So seeking in here is
//! seeking in an MP3 or an ADTS stream with no index, which works because
//! those formats resync on a frame header wherever you drop in. The two
//! things that don't splice are marked rather than hidden: a reconnect
//! records a gap, since the bytes either side of a dropped connection are not
//! one decodable stream, and a seek refuses to cross one.
//!
//! The cap is a length of time rather than a size, because that's what a
//! listener is choosing when they set it. Bytes per second is measured
//! instead of assumed: the decoder's own progress against the bytes it read
//! is the exact answer behind the cursor, `icy-br` is the station's claim,
//! and the wall clock is the fallback for a station that says nothing.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::icy::IcyTitle;
use crate::icy::TitleSink;
use crate::shared::LiveMark;
use crate::shared::Shift;

/// What a window is sized against before anything is known about the rate,
/// in bytes per second, and the floor the sizing uses either way. 128 kbps
/// is what most stations run at, so a station that says nothing about itself
/// gets about the window it asked for until the measurement lands.
const RATE_FLOOR: f64 = 16.0 * 1024.0;

/// The ceiling on the sizing, from the other end: a wrong estimate here is
/// memory, and a megabyte a second is already past any stream a listener is
/// going to leave running in the background.
const RATE_CEILING: f64 = 1024.0 * 1024.0;

/// How much audio has to have been decoded before the exact measurement is
/// trusted over the station's claim. The reader runs ahead of the decoder by
/// whatever symphonia has buffered, so the ratio starts high and settles.
const RATE_MIN_SECS: f64 = 10.0;

/// How long the wall-clock estimate has to run before it's worth anything.
const RATE_MIN_WIRE: Duration = Duration::from_secs(5);

/// How far a later measurement has to sit from the settled rate to count as
/// a disagreement at all.
const RATE_DRIFT: f64 = 0.2;

/// And how long it has to keep disagreeing before the rate is settled
/// again. Long enough that nothing short of a station really changing its
/// encoder gets there.
const RATE_DRIFT_SECS: Duration = Duration::from_secs(30);

/// How far over the cap the tape is allowed to run before the oldest bytes
/// come off. Trimming to the exact cap on every append would memmove the
/// whole window per read; trimming a sixteenth at a time makes it a move
/// every half minute or so, and keeps the published window steady rather
/// than sawtoothing between full and half full the way a drop-half rule
/// would.
const TRIM_SLACK: usize = 16;

/// How far behind the edge still reads as standing on it, before the chunk
/// size is taken into account. Inside this the published distance is exactly
/// zero, because it can't honestly be anything else: the edge arrives in
/// socket-sized chunks, and a cursor keeping up with a station is always
/// somewhere inside the last one.
pub const LIVE_EDGE_SNAP_SECS: f64 = 2.0;

/// The narrowest the deadband ever gets. A distance that hasn't moved by at
/// least this much hasn't moved.
const SHIFT_DEADBAND_SECS: f64 = 0.75;

/// How many chunks wide the deadband really is. The edge jumps a whole chunk
/// every time one lands while the cursor walks smoothly, so the raw distance
/// swings by exactly a chunk and a band narrower than that lets the published
/// number flip across it. At radio bitrates a socket chunk is a second or
/// more of audio, which is how a playhead that should be sitting still ends
/// up twitching.
const DEADBAND_CHUNKS: f64 = 1.5;

/// The longest a chunk is taken to be, in seconds of audio. The socket is
/// read into a buffer of a few kilobytes, so a real one is a second or two
/// at radio bitrates and four at the slowest anyone broadcasts music on.
/// The cap matters because every band here is measured in chunks: without
/// it, one oversized append would widen them past the numbers they're
/// meant to steady and freeze the readout entirely.
const CHUNK_MAX_SECS: f64 = 4.0;

/// How far the cursor may slip from the closest it has come to the edge
/// before a live session counts as having fallen behind. A pause is the
/// thing this catches: nobody seeked, but the broadcast ran on without the
/// listener and the distance is real. Measured in bands, since inside one
/// nothing about the distance is knowable anyway.
const LIVE_SLIP_BANDS: f64 = 1.0;

/// How long a read at the live edge parks before it looks at the world
/// again. The condvar wakes it the moment bytes land, so this only bounds
/// how stale its view of a dropped connection can get.
const EDGE_STEP: Duration = Duration::from_millis(100);

/// Where the station stands, as the network thread last left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// Connected and appending.
    Live,
    /// The connection went away and the thread is retrying.
    Reconnecting,
    /// The thread is gone. Nothing more will ever be appended.
    Done,
}

/// What a seek has to land on for the container to make sense of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Snap {
    /// Drop in anywhere. MP3 and ADTS carry a sync word on every frame, so
    /// the decoder finds the next boundary itself.
    Anywhere,
    /// Ogg only reads at a page header, so a seek scans forward to the next
    /// `OggS` capture pattern.
    OggPage,
}

/// The Ogg page capture pattern, which is the only thing in a live Ogg
/// stream that says "a container structure starts here".
const OGGS: &[u8; 4] = b"OggS";

/// How far a page scan will read before giving up and leaving the seek at
/// the live edge. A page is a few kilobytes at most; past this the bytes
/// aren't Ogg whatever the content type claimed.
const OGG_SCAN: usize = 1 << 20;

pub struct Tape {
    inner: Mutex<Inner>,
    /// Woken on every append, on every state change, and when the network
    /// thread gives up. The decode thread waits on it at the live edge.
    wake: Condvar,
    /// How many seconds of stream to hold, the setting's value. What the
    /// tape actually holds is whatever has arrived so far, which is shorter
    /// than this for the first few minutes of every station.
    ///
    /// An atomic rather than a plain field because the setting can move
    /// under a station that's already playing. Both threads read it, the
    /// engine writes it, and the buffer it sizes is behind the mutex
    /// anyway, so nothing here needs the two to move together.
    cap_secs: AtomicU32,
    /// What a seek has to land on, off the station's content type.
    snap: Snap,
    /// Where the titles the stream carries go, fired by the reader as the
    /// cursor reaches the byte they were found at rather than as they come
    /// off the wire. Held here so a reopen over this tape inherits it
    /// without the caller having to rebuild it.
    on_title: TitleSink,
    /// The session's "a command is waiting" flag. Read at the live edge,
    /// where a station that has gone quiet would otherwise leave the decode
    /// thread parked with a pause unanswered.
    interrupt: Arc<AtomicBool>,
}

struct Inner {
    /// The window itself: raw container bytes, oldest first.
    buf: Vec<u8>,
    /// Stream offset `buf[0]` sits at. Everything else here is an absolute
    /// offset on that same clock, so a cursor stays meaningful after the
    /// bytes under it have been dropped.
    start: u64,
    /// Where the reader has got to. Published here rather than kept to
    /// itself because the engine needs it for the timeshift readout and to
    /// decide where a seek is going from, and it has no other way down to
    /// the reader.
    cursor: u64,
    /// Offsets where one connection ended and the next began. The bytes
    /// either side don't splice into one decodable stream, so a seek lands
    /// on the live side of the newest one it would have crossed.
    gaps: VecDeque<u64>,
    /// Where each title the station announced was found. Kept for the whole
    /// window, so a seek backwards resolves the title that was on then
    /// rather than leaving the newest one standing.
    marks: VecDeque<Mark>,
    /// The offset of the mark the reader last published, so an unchanged
    /// answer costs no sink call.
    published: Option<u64>,
    /// Whether the next title names music already in progress rather than a
    /// song starting. True at the open and again after every reconnect.
    joined: bool,
    /// Whether the listener has stepped back into the buffer. Live is a
    /// state rather than a distance: a station bursts several seconds at
    /// connect and playing from the start of that burst is what a radio is
    /// for, so a cursor that has never been moved reads as live however far
    /// behind the edge it technically sits.
    timeshifted: bool,
    /// The closest the cursor has come to the edge while live, in seconds.
    /// A pause holds the cursor while the broadcast runs on, and that
    /// slipping is how a session stops being live without anybody seeking.
    live_lead: f64,
    /// Bumped whenever the set of marks changes: a song announced, or one
    /// falling off the back. Not for the distances moving, which every
    /// chunk does. What the engine polls to know a republish is worth a
    /// revision of its own.
    marks_rev: u64,
    state: Feed,
    rate: Rate,
    /// How big the last chunk off the socket was. A station sends in real
    /// time but arrives in bursts, so this is the resolution of any distance
    /// measured against the live edge, and the width of the band that keeps
    /// such a distance still.
    last_chunk: usize,
    /// The distance and the span as last published. Held rather than
    /// recomputed so a number that hasn't really moved doesn't move on
    /// screen. See [`SHIFT_DEADBAND_SECS`].
    held_behind: Option<f64>,
    held_window: Option<f64>,
    /// The reader asked for bytes that had already been dropped, so its
    /// cursor snapped forward to the oldest byte held. Taken by the engine,
    /// which answers it with a re-sync: the decoder is mid-frame on bytes
    /// that no longer follow what it was reading.
    underran: bool,
}

/// One title the station announced, and where in the stream it said so.
struct Mark {
    /// The stream offset the title arrived at.
    at: u64,
    title: IcyTitle,
    /// The title that was already playing when this connection opened,
    /// rather than a song starting. A station announces what's on the
    /// moment you tune in and again after every reconnect, and that
    /// announcement is a name for music already in progress: it names the
    /// song and it isn't a boundary, so nothing draws it.
    joined: bool,
    /// The distance from the live edge as last published, held on the same
    /// band as the playhead so a mark and the window it sits in move
    /// together rather than one crossing the other between two paints.
    held: Option<f64>,
}

/// How bytes become seconds, settled once and then held.
///
/// Every number published about a buffer is a count of bytes divided by
/// this, so a rate that keeps moving rescales the whole strip under the
/// listener: the filled bar grows and shrinks, the playhead slides, a mark
/// crosses the edge of the window and comes back. None of that is the
/// stream doing anything. Radio is constant bitrate, so the right shape is
/// one number decided early and left alone.
struct Rate {
    /// The rate everything is divided by, once there's one worth keeping.
    /// None while the first ten seconds are being measured, where the
    /// provisional answer below stands in.
    held: Option<f64>,
    /// The held rate came from `icy-br`, the station's own word, and is
    /// never replaced. A measurement is a check on it, not a candidate.
    stated: bool,
    /// Bytes appended and the wall clock they arrived over, the provisional
    /// answer before anything has been decoded.
    wire_bytes: u64,
    wire_since: Instant,
    /// Bytes the decoder read and the seconds of audio they turned into,
    /// since the last time this window was opened. The exact answer for the
    /// part of the tape that has actually been played.
    played_bytes: u64,
    played_secs: f64,
    /// Since when the measurement has disagreed with the held rate by more
    /// than [`RATE_DRIFT`]. A station really changing bitrate mid-stream is
    /// rare enough to insist on it lasting.
    drifting_since: Option<Instant>,
}

impl Rate {
    fn new(stated_kbps: u32) -> Rate {
        let stated = (stated_kbps > 0).then(|| stated_kbps as f64 * 1000.0 / 8.0);

        Rate {
            held: stated,
            stated: stated.is_some(),
            wire_bytes: 0,
            wire_since: Instant::now(),
            played_bytes: 0,
            played_secs: 0.0,
            drifting_since: None,
        }
    }

    /// Take the decoder's own account of `secs` of audio, and settle the
    /// rate off it once there's enough to settle on.
    ///
    /// The window is reopened at the first call rather than at the open,
    /// because the reader runs ahead of the decoder by whatever symphonia
    /// has buffered: counted from zero that read-ahead is a constant
    /// sitting on top of ten seconds' worth, which on a 128 kbps stream is
    /// half again the real rate. Measured from the first decoded chunk it
    /// cancels, since both ends of the window carry it.
    fn played(&mut self, secs: f64) {
        if self.played_secs == 0.0 {
            self.played_bytes = 0;
        }

        self.played_secs += secs;
        if self.played_secs < RATE_MIN_SECS {
            return;
        }

        let measured = self.played_bytes as f64 / self.played_secs;
        self.played_bytes = 0;
        self.played_secs = 0.0;

        // The first ten seconds decide it, unless the station already said.
        let Some(held) = self.held else {
            self.held = Some(measured);

            return;
        };
        if self.stated {
            return;
        }

        // A disagreement has to hold for half a minute before it counts.
        // One window out of band is a station's own jitter or a decode that
        // stalled; three in a row is a stream that really did change.
        if (measured - held).abs() <= held * RATE_DRIFT {
            self.drifting_since = None;

            return;
        }

        match self.drifting_since {
            Some(since) if since.elapsed() >= RATE_DRIFT_SECS => {
                log::info!("station rate re-settled at {measured:.0} bytes/s from {held:.0}");
                self.held = Some(measured);
                self.drifting_since = None;
            }

            Some(_) => {}

            None => self.drifting_since = Some(Instant::now()),
        }
    }

    /// Bytes per second: the held rate, or the socket's own average while
    /// there's nothing better, or the default before even that means
    /// anything.
    ///
    /// Unclamped on purpose. This is what turns a distance in bytes into a
    /// distance in seconds, and holding a 128 kbps station to some floor
    /// would have the transport report half the timeshift a listener
    /// actually has. The clamp belongs to [`Inner::cap`], where a wrong
    /// answer costs memory rather than a wrong readout.
    fn bytes_per_sec(&self) -> f64 {
        if let Some(held) = self.held {
            return held;
        }

        let elapsed = self.wire_since.elapsed();
        match elapsed >= RATE_MIN_WIRE {
            true => self.wire_bytes as f64 / elapsed.as_secs_f64(),
            false => RATE_FLOOR,
        }
    }
}

impl Inner {
    /// The offset just past the newest byte held, which is the live edge.
    fn head(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    /// How many bytes `window_secs` of this station comes to. The rate is
    /// held to a band here and nowhere else: too low an estimate would make
    /// the window shorter than the listener asked for, too high is memory
    /// spent on a station that doesn't need it, and neither is worth
    /// inheriting from a header a station made up.
    fn cap(&self, cap_secs: u32) -> usize {
        let bps = self.rate.bytes_per_sec().clamp(RATE_FLOOR, RATE_CEILING);

        (bps * cap_secs as f64) as usize
    }

    /// Drop the oldest bytes once the window has overrun its cap, and the
    /// gaps and title marks that went with them.
    fn trim(&mut self, cap_secs: u32) {
        let cap = self.cap(cap_secs);
        if self.buf.len() <= cap + cap / TRIM_SLACK {
            return;
        }

        let inside = self.inside_marks();
        let drop = self.buf.len() - cap;
        self.buf.drain(..drop);
        self.start += drop as u64;

        // A gap that fell off the back stops mattering: there's nothing
        // behind it left to seek into.
        while self.gaps.front().is_some_and(|g| *g <= self.start) {
            self.gaps.pop_front();
        }

        // Titles go the same way with one kept back. The mark before the
        // oldest byte is the song that was playing when the window opens,
        // so dropping it would leave a seek to the back of the tape with
        // nothing to name.
        while self.marks.len() > 1 && self.marks[1].at <= self.start {
            self.marks.pop_front();
        }

        // A song whose start has gone off the back is a song with no point
        // on the strip to draw it at, so the set being drawn changed even
        // though the mark may still be held for the title it names.
        if self.inside_marks() != inside {
            self.marks_rev += 1;
        }
    }

    /// How many marks still have their start inside the window, which is
    /// the set anything drawing the buffer can show.
    fn inside_marks(&self) -> usize {
        self.marks.iter().filter(|mark| self.drawn(mark)).count()
    }

    /// The band every distance measured against the live edge is held
    /// inside: as wide as the bursts it's measured against, and never
    /// narrower than [`SHIFT_DEADBAND_SECS`]. One rule for the playhead and
    /// for each mark, so they move together or not at all.
    fn band(&self, bps: f64) -> f64 {
        SHIFT_DEADBAND_SECS.max(DEADBAND_CHUNKS * self.chunk_secs(bps))
    }

    /// How long the last chunk off the socket was, in seconds of audio, and
    /// zero before one has landed. The resolution of everything measured
    /// against the live edge: inside a chunk there's nothing to know.
    fn chunk_secs(&self, bps: f64) -> f64 {
        (self.last_chunk as f64 / bps).min(CHUNK_MAX_SECS)
    }

    /// Which mark the cursor sits under: the newest one at or behind it.
    fn mark_index(&self, cursor: u64) -> Option<usize> {
        self.marks.iter().rposition(|mark| mark.at <= cursor)
    }

    /// Whether a mark is one a strip over the buffer can draw: inside what
    /// the buffer holds, and a song really starting rather than the name of
    /// what was already on when we tuned in.
    fn drawn(&self, mark: &Mark) -> bool {
        !mark.joined && mark.at >= self.start && mark.at <= self.head()
    }

    /// The newest title mark at or behind `cursor`, which is the song the
    /// listener is actually hearing.
    fn title_at(&self, cursor: u64) -> Option<&Mark> {
        self.marks.get(self.mark_index(cursor)?)
    }

    /// Where the song under the cursor started and where the next one does,
    /// as absolute offsets. A song clock is made of the pair: how far past
    /// the first the cursor has got, and how far apart the two are.
    fn song_bounds(&self, cursor: u64) -> (Option<u64>, Option<u64>) {
        let Some(i) = self.mark_index(cursor) else {
            return (None, None);
        };

        (
            Some(self.marks[i].at),
            self.marks.get(i + 1).map(|next| next.at),
        )
    }
}

impl Tape {
    /// A tape holding `window_secs` of a station whose headers claimed
    /// `stated_kbps` (zero for one that claimed nothing).
    pub fn new(
        cap_secs: u32,
        stated_kbps: u32,
        snap: Snap,
        on_title: TitleSink,
        interrupt: Arc<AtomicBool>,
    ) -> Tape {
        Tape {
            inner: Mutex::new(Inner {
                buf: Vec::new(),
                start: 0,
                cursor: 0,
                gaps: VecDeque::new(),
                marks: VecDeque::new(),
                joined: true,
                timeshifted: false,
                live_lead: f64::INFINITY,
                published: None,
                marks_rev: 0,
                state: Feed::Live,
                rate: Rate::new(stated_kbps),
                last_chunk: 0,
                held_behind: None,
                held_window: None,
                underran: false,
            }),
            wake: Condvar::new(),
            cap_secs: AtomicU32::new(cap_secs),
            snap,
            on_title,
            interrupt,
        }
    }

    /// How long a window this is set to hold, in seconds.
    pub fn cap_secs(&self) -> u32 {
        self.cap_secs.load(Ordering::Relaxed)
    }

    /// Re-cap the tape with a station already on air, for the setting being
    /// moved by someone listening to the thing it changes.
    ///
    /// Growing costs nothing and takes effect gradually: the ceiling rises,
    /// nothing is dropped, and the window reaches the new length once the
    /// station has been on that long. Shrinking takes effect now, because
    /// getting the memory back is the whole reason anyone turns it down,
    /// and a trim that waited for the next append would leave a station
    /// that has gone quiet holding the old window indefinitely.
    pub fn set_cap_secs(&self, secs: u32) {
        self.cap_secs.store(secs, Ordering::Relaxed);

        let mut inner = self.inner.lock().unwrap();
        inner.trim(secs);
    }

    /// Add what came off the socket. Network thread only.
    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut inner = self.inner.lock().unwrap();
        inner.buf.extend_from_slice(bytes);
        inner.rate.wire_bytes += bytes.len() as u64;
        inner.last_chunk = bytes.len();
        inner.trim(self.cap_secs());
        drop(inner);

        self.wake.notify_all();
    }

    /// Note a title the station announced, at the point in the stream it was
    /// announced. Network thread only, from inside the ICY reader.
    pub fn mark_title(&self, title: IcyTitle) {
        let mut inner = self.inner.lock().unwrap();
        let at = inner.head();
        if inner.marks.back().is_some_and(|last| last.title == title) {
            return;
        }

        // The first title on a connection is the station saying what it's
        // already playing, which the capture service knows as the joined
        // case and answers the same way. It's kept, because it's the name of
        // the song the listener is hearing and the clock counts from
        // somewhere; it just isn't a boundary anybody can point at.
        let joined = std::mem::take(&mut inner.joined);
        inner.marks.push_back(Mark {
            at,
            title,
            joined,
            held: None,
        });
        if !joined {
            inner.marks_rev += 1;
        }
    }

    /// Record that the stream picked up again on a fresh connection. The
    /// bytes from here on belong to a different run of the encoder, so
    /// nothing may decode across this point.
    pub fn splice(&self) {
        let mut inner = self.inner.lock().unwrap();
        let at = inner.head();
        inner.gaps.push_back(at);
        // A reconnect rejoins mid-song exactly the way the first connect
        // did, so whatever the station announces next names music already
        // in progress.
        inner.joined = true;
    }

    /// Say where the station stands. Network thread only.
    pub fn set_feed(&self, state: Feed) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = state;
        drop(inner);

        self.wake.notify_all();
    }

    pub fn feed(&self) -> Feed {
        self.inner.lock().unwrap().state
    }

    /// How much audio the decoder just produced, for the byte-to-second
    /// measurement. Decode thread only, once per chunk.
    pub fn note_audio(&self, secs: f64) {
        if secs <= 0.0 {
            return;
        }

        self.inner.lock().unwrap().rate.played(secs);
    }

    /// Where the cursor stands, in the seconds the transport draws: how far
    /// behind the live edge it is, how much tape there is either side of it,
    /// and how far into the song it's under.
    ///
    /// The song half comes off the in-band marks and nothing else. A
    /// broadcast has no other idea when a song began, and a listener who
    /// steps back into the middle of one should see the clock they'd have
    /// seen the first time round rather than a fresh zero. It's None until a
    /// title has been announced behind the cursor, which is the first
    /// seconds of every connect.
    ///
    /// The length is None wherever it would be a guess: at the newest song,
    /// which hasn't ended, and at a song whose start has been trimmed off
    /// the back, where what's left isn't the whole of it.
    pub fn shift(&self) -> Shift {
        let mut inner = self.inner.lock().unwrap();
        let bps = inner.rate.bytes_per_sec();
        let edge = inner.head();
        let cursor = inner.cursor;
        let (song_at, next_at) = inner.song_bounds(cursor);

        // The two numbers measured against the edge are the ones that
        // twitch, so both are held inside a band as wide as the bursts they
        // are measured against. The song clock runs from a mark to the
        // cursor, with the edge nowhere in it, and is left alone.
        let band = inner.band(bps);
        let edge_of = LIVE_EDGE_SNAP_SECS.max(inner.chunk_secs(bps));
        let behind = edge.saturating_sub(cursor) as f64 / bps;

        // Live is a state rather than a distance. A station bursts several
        // seconds of audio at the connect so the decoder has something to
        // work with, and playing from the start of that burst is what every
        // radio does: the listener is at the front of the broadcast, not six
        // seconds behind it. So a cursor nobody has moved reads as live
        // however far back it technically sits, and the distance only starts
        // meaning something once the listener has stepped back or a pause
        // has let the broadcast run on without them.
        inner.live_lead = inner.live_lead.min(behind);
        if behind > inner.live_lead + LIVE_SLIP_BANDS * band {
            inner.timeshifted = true;
        }

        let behind_secs = match !inner.timeshifted || behind < edge_of {
            true => 0.0,
            false => steady(inner.held_behind, behind, band),
        };
        let window_secs = steady(inner.held_window, (edge - inner.start) as f64 / bps, band);

        inner.held_behind = Some(behind_secs);
        inner.held_window = Some(window_secs);

        Shift {
            behind_secs,
            window_secs,
            cap_secs: self.cap_secs() as f64,
            bytes_per_sec: bps,
            song_secs: song_at.map(|at| cursor.saturating_sub(at) as f64 / bps),
            song_len_secs: song_at
                .filter(|at| *at >= inner.start)
                .zip(next_at)
                .map(|(at, next)| next.saturating_sub(at) as f64 / bps),
        }
    }

    /// The song boundaries still inside the window, oldest first, each as a
    /// distance back from the live edge.
    ///
    /// Raw distances, where [`Shift::behind_secs`] is held still. The
    /// playhead is one number an eye follows and a band keeps it from
    /// twitching; these are the buffer's own contents, and when a chunk
    /// lands the whole of it really did slide away from the edge together.
    pub fn live_marks(&self) -> Vec<LiveMark> {
        let mut inner = self.inner.lock().unwrap();
        let bps = inner.rate.bytes_per_sec();
        let band = inner.band(bps);
        let head = inner.head();
        let start = inner.start;

        inner
            .marks
            .iter_mut()
            .filter(|mark| !mark.joined && mark.at >= start && mark.at <= head)
            .map(|mark| {
                let behind_secs =
                    steady(mark.held, head.saturating_sub(mark.at) as f64 / bps, band);
                mark.held = Some(behind_secs);

                LiveMark {
                    behind_secs,
                    artist: mark.title.artist.clone(),
                    title: mark.title.title.clone(),
                }
            })
            .collect()
    }

    /// The revision of that set, moved by a song being announced or one
    /// falling off the back and by nothing else. Cheap to poll; the list
    /// above allocates.
    pub fn marks_rev(&self) -> u64 {
        self.inner.lock().unwrap().marks_rev
    }

    /// Where the reader is, in absolute stream bytes.
    pub fn cursor(&self) -> u64 {
        self.inner.lock().unwrap().cursor
    }

    /// Whether a read has fallen off the back of the window since this was
    /// last asked, and clear it. The engine answers a true by re-syncing,
    /// since the decoder's next packet would otherwise start mid-frame on
    /// bytes that don't follow the ones before them.
    pub fn took_underrun(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        std::mem::replace(&mut inner.underran, false)
    }

    /// The offset to start reading at for a cursor `behind` seconds back
    /// from the live edge, held to what the tape holds and to the live side
    /// of any gap it would have crossed.
    ///
    /// "Live" is never the edge itself. A cursor put right on the head has
    /// nothing to read until the next chunk lands, and chunks land in
    /// bursts, so the decoder starves a few times over the first second
    /// until it has drifted a chunk back on its own. Landing that far back
    /// to begin with is the same place it would settle, minus the stutter:
    /// the snap distance the readout already calls "live", or one and a
    /// half chunks on a stream whose chunks run longer than that.
    pub fn seek_target(&self, behind_secs: f64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let bps = inner.rate.bytes_per_sec();
        let lead = (LIVE_EDGE_SNAP_SECS * bps).max(inner.last_chunk as f64 * 1.5) as u64;
        let back = ((behind_secs.max(0.0) * bps) as u64).max(lead);
        let head = inner.head();

        // Asking for no more than the lead is asking to be live, which is
        // what the LIVE button sends; anything further back is a listener
        // stepping into the buffer. The state is what the readout follows,
        // so it's set here rather than guessed from a distance later.
        inner.timeshifted = back > lead;
        inner.live_lead = f64::INFINITY;
        let mut at = head.saturating_sub(back).max(inner.start);

        // A gap is a splice between two connections, and no decoder reads
        // across one. Landing on its live side gives up the older audio
        // rather than handing over bytes that can't be played as one
        // stream.
        if let Some(gap) = inner.gaps.iter().rev().find(|gap| **gap > at) {
            at = *gap;
        }

        match self.snap {
            Snap::Anywhere => at,
            Snap::OggPage => next_page(&inner, at).unwrap_or(head),
        }
    }

    /// Put the published cursor back where it was, for a reopen that got as
    /// far as building a reader and then failed. The reader it built is
    /// dropped and the old one carries on, so the number the transport reads
    /// has to carry on with it.
    pub fn restore_cursor(&self, at: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.cursor = at;

        // The seek that asked for this is the one that set the state, and it
        // didn't happen. Work it back out from where the cursor really is,
        // which is the same question the first sample after a connect
        // answers.
        let bps = inner.rate.bytes_per_sec();
        let lead = (LIVE_EDGE_SNAP_SECS * bps).max(inner.last_chunk as f64 * 1.5) as u64;
        inner.timeshifted = inner.head().saturating_sub(at) > lead;
        inner.live_lead = f64::INFINITY;
    }

    /// A reader over this tape starting at `at`. The engine builds one of
    /// these to re-sync a decoder after a timeshift seek, over the same
    /// tape the old one was reading.
    pub fn reader(self: &Arc<Self>, at: u64) -> TapeReader {
        let mut inner = self.inner.lock().unwrap();
        let at = at.clamp(inner.start, inner.head());
        inner.cursor = at;
        // Whatever the last reader had published belonged to its own place
        // in the stream. The new one republishes from where it lands, which
        // for a seek backwards is an older song than the one standing.
        inner.published = None;
        drop(inner);

        TapeReader {
            tape: Arc::clone(self),
            cursor: at,
        }
    }
}

/// What to publish for a number that has been published before: the new
/// value once it has moved by more than `band`, and the old one until then.
///
/// The raw distance to the live edge saws by a chunk, since the edge steps a
/// chunk at a time and the cursor walks. Nothing downstream can do anything
/// with that saw except draw it, so it's stopped here rather than smoothed:
/// a band wider than the saw leaves one value standing, which is what a
/// playhead sitting behind a broadcast should do. The cost is up to a band's
/// worth of lag on a number that only ever changes slowly.
fn steady(held: Option<f64>, value: f64, band: f64) -> f64 {
    match held {
        Some(held) if (value - held).abs() <= band => held,
        _ => value,
    }
}

/// The offset of the first Ogg page header at or after `at`, None when the
/// scan ran out of tape without finding one.
fn next_page(inner: &Inner, at: u64) -> Option<u64> {
    let from = at.saturating_sub(inner.start) as usize;
    let end = inner.buf.len().min(from + OGG_SCAN);

    inner.buf[from..end]
        .windows(OGGS.len())
        .position(|w| w == OGGS)
        .map(|off| at + off as u64)
}

/// The decode side of a tape: a cursor, and a read that waits at the live
/// edge for the network thread to catch it up.
///
/// One of these at a time per tape in practice. A timeshift seek builds the
/// replacement before dropping the source holding the old one, so the two
/// overlap for the length of a probe, during which the new one is the only
/// one reading.
pub struct TapeReader {
    tape: Arc<Tape>,
    /// Where this reader is. Mirrored into the tape on every move, since
    /// the engine reads it from there.
    cursor: u64,
}

impl io::Read for TapeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        loop {
            let mut inner = self.tape.inner.lock().unwrap();

            // The pause outlasted the window: the bytes this was reading
            // have been dropped to make room for the broadcast that kept
            // arriving. Snapping to the oldest byte held is the only place
            // there is to go, and the flag is what gets the decoder rebuilt
            // around the jump.
            if self.cursor < inner.start {
                self.cursor = inner.start;
                inner.underran = true;
            }

            let head = inner.head();
            if self.cursor < head {
                let off = (self.cursor - inner.start) as usize;
                let n = out.len().min(inner.buf.len() - off);
                out[..n].copy_from_slice(&inner.buf[off..off + n]);
                self.cursor += n as u64;
                inner.cursor = self.cursor;
                inner.rate.played_bytes += n as u64;

                // The song the cursor just moved into, if it moved into
                // one. Resolved here and fired below, because the sink
                // ends up in the engine and holding the tape's lock
                // across it would put the network thread behind whatever
                // it does.
                let mark = inner
                    .title_at(self.cursor)
                    .filter(|mark| inner.published != Some(mark.at))
                    .map(|mark| (mark.at, mark.title.clone()));
                let title = mark.map(|(at, title)| {
                    inner.published = Some(at);
                    title
                });
                let on_title = Arc::clone(&self.tape.on_title);
                drop(inner);

                if let Some(title) = title {
                    on_title(title);
                }

                return Ok(n);
            }

            // At the live edge. Nothing to do but wait for the socket,
            // which is what the condvar is for.
            match inner.state {
                // The thread gave up. A station that ran out of reconnects
                // is over, and the engine treats that the way it treats any
                // track ending.
                Feed::Done => {
                    return Err(io::Error::other("the station is gone"));
                }

                // A command is waiting and the station is off the air. The
                // decode thread is the thread that answers the transport,
                // so waiting out a reconnect here is a pause going
                // unanswered for as long as the station stays down. The
                // error ends the entry and the queue moves on, which is
                // what pressing something during a drop asks for.
                //
                // Deliberately not checked while the feed is live: a pause
                // on a healthy station arrives with bytes microseconds
                // away, and erroring out on it would kill the very stream
                // the pause is meant to hold.
                Feed::Reconnecting if self.tape.interrupt.load(Ordering::Relaxed) => {
                    return Err(io::Error::other(
                        "the station is down and a command is waiting",
                    ));
                }

                _ => {
                    let _ = self.tape.wake.wait_timeout(inner, EDGE_STEP).unwrap();
                }
            }
        }
    }
}

impl io::Seek for TapeReader {
    /// Move the cursor inside the window. Symphonia is told this source
    /// isn't seekable, so nothing asks this to do the thing a file's seek
    /// does; what it answers is the probe's small rewinds and the engine's
    /// own arithmetic.
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        let mut inner = self.tape.inner.lock().unwrap();
        let target = match from {
            io::SeekFrom::Start(at) => Some(at),

            io::SeekFrom::Current(delta) => self.cursor.checked_add_signed(delta),

            // A broadcast has no end to count back from, and won't have one
            // later either.
            io::SeekFrom::End(_) => {
                return Err(io::Error::other("a live stream has no end to seek from"));
            }
        }
        .ok_or_else(|| io::Error::other("seek out of range"))?;

        if target < inner.start || target > inner.head() {
            return Err(io::Error::other("seek outside the buffered window"));
        }

        self.cursor = target;
        inner.cursor = target;

        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;

    fn tape(cap_secs: u32, kbps: u32) -> Arc<Tape> {
        Arc::new(Tape::new(
            cap_secs,
            kbps,
            Snap::Anywhere,
            crate::icy::no_titles(),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    fn bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// Append `n` bytes the way a socket delivers them, in 16 kB reads, so
    /// the live lead is the snap distance rather than a chunk and a half
    /// of one oversized append.
    fn feed(tape: &Tape, n: usize) {
        for chunk in bytes(n).chunks(16_000) {
            tape.append(chunk);
        }
    }

    #[test]
    fn a_read_behind_the_edge_comes_straight_out_of_the_window() {
        let tape = tape(60, 128);
        tape.append(&bytes(4096));

        let mut reader = tape.reader(0);
        let mut out = vec![0u8; 1024];
        assert_eq!(reader.read(&mut out).unwrap(), 1024);
        assert_eq!(out, bytes(4096)[..1024]);
        assert_eq!(tape.cursor(), 1024);
    }

    /// The window is a length of time, so what it holds in bytes follows
    /// the rate: at the station's stated 32 kB/s, ten seconds is 320 kB and
    /// everything older comes off.
    #[test]
    fn the_window_drops_its_oldest_past_the_cap() {
        // 256 kbps is 32 kB/s, so a ten second window is 320 kB.
        let tape = tape(10, 256);
        tape.append(&bytes(512 * 1024));

        let window = tape.shift().window_secs;
        let inner = tape.inner.lock().unwrap();
        assert_eq!(inner.buf.len(), 320 * 1000, "trimmed back to the cap");
        assert_eq!(inner.start, 512 * 1024 - 320 * 1000);
        assert!(
            (window - 10.0).abs() < 0.01,
            "ten seconds of tape: {window}"
        );
    }

    #[test]
    fn a_cursor_that_fell_off_the_back_snaps_and_says_so() {
        let tape = tape(10, 256);
        let mut reader = tape.reader(0);
        tape.append(&bytes(512 * 1024));

        let mut out = vec![0u8; 16];
        assert_eq!(reader.read(&mut out).unwrap(), 16);
        assert!(tape.took_underrun(), "the read fell off the back");
        assert!(!tape.took_underrun(), "and the flag is taken once");
        assert_eq!(tape.cursor(), 512 * 1024 - 320 * 1000 + 16);
    }

    #[test]
    fn a_read_at_the_edge_waits_for_the_next_chunk() {
        let tape = tape(60, 128);
        let mut reader = tape.reader(0);

        let writer = Arc::clone(&tape);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            writer.append(&bytes(64));
        });

        let mut out = vec![0u8; 64];
        let began = Instant::now();
        assert_eq!(reader.read(&mut out).unwrap(), 64);
        assert!(began.elapsed() >= Duration::from_millis(20), "it waited");
    }

    /// The one shape of interrupt that ends a read: a command waiting while
    /// the station is off the air. A live feed never takes this path, or a
    /// pause would kill the stream it means to hold.
    #[test]
    fn a_waiting_command_ends_a_read_at_a_dead_edge() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let tape = Arc::new(Tape::new(
            60,
            128,
            Snap::Anywhere,
            crate::icy::no_titles(),
            Arc::clone(&interrupt),
        ));
        let mut reader = tape.reader(0);
        tape.set_feed(Feed::Reconnecting);
        interrupt.store(true, Ordering::Relaxed);

        let mut out = vec![0u8; 16];
        assert!(reader.read(&mut out).is_err());
    }

    #[test]
    fn a_dead_station_ends_the_read() {
        let tape = tape(60, 128);
        let mut reader = tape.reader(0);
        tape.set_feed(Feed::Done);

        let mut out = vec![0u8; 16];
        assert!(reader.read(&mut out).is_err());
    }

    /// A seek back over a reconnect lands on the live side of it: the bytes
    /// before the splice belong to a connection the decoder can't carry on
    /// from.
    #[test]
    fn a_seek_never_crosses_a_gap() {
        // 32 kB/s, so a second is 32000 bytes.
        let tape = tape(600, 256);
        feed(&tape, 64_000);
        tape.splice();
        feed(&tape, 64_000);

        assert_eq!(tape.seek_target(3.0), 64_000, "held at the splice");
        // A second back is inside the live lead, so it lands at the lead's
        // two seconds instead: clear of the splice all the same.
        assert_eq!(
            tape.seek_target(1.0),
            64_000,
            "a second back is the lead, and the lead sits on the splice"
        );
        feed(&tape, 64_000);
        assert_eq!(
            tape.seek_target(1.0),
            64_000 + 64_000,
            "with room behind the edge the lead clears the splice"
        );
    }

    #[test]
    fn a_seek_target_holds_to_what_the_tape_has() {
        let tape = tape(600, 256);
        feed(&tape, 64_000);

        assert_eq!(tape.seek_target(600.0), 0, "the oldest byte held");
        // "Live" is a lead behind the edge (two seconds at 32 kB/s), never
        // the edge itself, so the decoder has bytes in hand.
        assert_eq!(tape.seek_target(0.0), 0, "two seconds of a two-second tape");
        feed(&tape, 64_000);
        assert_eq!(tape.seek_target(0.0), 64_000, "the edge less the lead");
    }

    #[test]
    fn an_ogg_seek_lands_on_a_page_header() {
        let tape = Arc::new(Tape::new(
            600,
            256,
            Snap::OggPage,
            crate::icy::no_titles(),
            Arc::new(AtomicBool::new(false)),
        ));
        let mut stream = bytes(32_000);
        stream.extend_from_slice(OGGS);
        stream.extend_from_slice(&bytes(32_000));
        tape.append(&stream);

        assert_eq!(tape.seek_target(1.5), 32_000, "forward to the page");
    }

    /// The title the cursor is under, not the one the socket is under. A
    /// listener ten minutes behind hears the song that was on ten minutes
    /// ago, and that's what the transport has to name.
    #[test]
    fn titles_fire_as_the_cursor_reaches_them() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let tape = Arc::new(Tape::new(
            600,
            256,
            Snap::Anywhere,
            Arc::new(move |title: IcyTitle| sink.lock().unwrap().push(title.title)),
            Arc::new(AtomicBool::new(false)),
        ));

        let song = |title: &str| IcyTitle {
            artist: String::new(),
            title: title.to_string(),
        };

        let mut reader = tape.reader(0);
        tape.mark_title(song("first"));
        tape.append(&bytes(1000));
        tape.mark_title(song("second"));
        tape.append(&bytes(1000));

        let mut out = vec![0u8; 500];
        assert_eq!(reader.read(&mut out).unwrap(), 500);
        assert_eq!(*seen.lock().unwrap(), vec!["first".to_string()]);

        let mut out = vec![0u8; 1000];
        assert_eq!(reader.read(&mut out).unwrap(), 1000);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["first".to_string(), "second".to_string()],
            "the second song lands once the cursor is past its mark"
        );
    }

    /// Live is a state, not a distance. Icecast opens a connection by
    /// bursting several seconds of audio so the decoder has something to
    /// work with, and playing from the start of that burst is what every
    /// radio does: the listener is at the front of the broadcast. Reading
    /// six seconds behind and jittering between six and seven is the
    /// arithmetic being shown where the answer is simply "live".
    #[test]
    fn a_connect_burst_reads_as_live_until_the_listener_steps_back() {
        // 256 kbps is 32 kB/s, so six seconds of burst is 192 kB.
        let tape = tape(600, 256);
        feed(&tape, 192_000);

        let mut reader = tape.reader(0);
        assert_eq!(tape.shift().behind_secs, 0.0, "at the front of the burst");

        // Keeping pace with it: a second of audio decoded for every second
        // that arrives. Still live, however much of the burst is ahead.
        for _ in 0..10 {
            feed(&tape, 32_000);
            let mut out = vec![0u8; 32_000];
            assert_eq!(reader.read(&mut out).unwrap(), 32_000);
            assert_eq!(tape.shift().behind_secs, 0.0, "still live");
        }

        // Stepping back is what makes the distance mean something. Ten
        // seconds, which is well inside the sixteen this tape holds.
        let at = tape.seek_target(10.0);
        let _reader = tape.reader(at);
        let behind = tape.shift().behind_secs;
        assert!(
            (behind - 10.0).abs() < 1.0,
            "ten seconds into the buffer: {behind}"
        );

        // And the LIVE button puts it back, without the cursor having to
        // reach the last byte.
        let at = tape.seek_target(0.0);
        let _reader = tape.reader(at);
        assert_eq!(tape.shift().behind_secs, 0.0, "live again");
    }

    /// The other way a session stops being live: nobody seeked, the
    /// listener paused, and the broadcast ran on without them. That
    /// distance is real and has to show.
    #[test]
    fn a_pause_that_lets_the_broadcast_run_on_stops_reading_as_live() {
        let tape = tape(600, 256);
        feed(&tape, 64_000);

        let _reader = tape.reader(0);
        assert_eq!(tape.shift().behind_secs, 0.0, "live at the open");

        // Thirty seconds arrive with nothing reading them, which is what a
        // pause looks like from here.
        feed(&tape, 960_000);

        let behind = tape.shift().behind_secs;
        assert!(
            (behind - 32.0).abs() < 2.0,
            "the pause is a real distance: {behind}"
        );
    }

    /// The clock a listener sees while playing steadily behind the    /// The clock a listener sees while playing steadily behind the
    /// broadcast. The edge lands a chunk at a time while the cursor walks,
    /// so the raw distance saws by exactly a chunk, and the band is measured
    /// in chunks for that reason: it holds whether a chunk is half a second
    /// of radio or two.
    ///
    /// No clock in here. The distance is byte arithmetic against the rate,
    /// so what the test has to reproduce is the pattern of arrivals, not the
    /// pace of them.
    #[test]
    fn the_behind_clock_holds_still_while_the_edge_arrives_in_bursts() {
        // 256 kbps is 32 kB/s: the cursor walks 3.2 kB per tenth of a second
        // of audio, and a chunk is however many tenths it carries.
        for chunk_secs in [0.5, 1.0, 2.0] {
            let chunk = (chunk_secs * 32_000.0) as usize;
            let period = (chunk_secs * 10.0) as usize;

            // Primed the way the feed fills it, a chunk at a time, so the
            // band is measured against the chunk size the test is about.
            let tape = tape(600, 256);
            for _ in 0..(1_920_000 / chunk) {
                tape.append(&bytes(chunk));
            }

            // Thirty seconds back and keeping pace, parked the way the
            // engine parks it: a seek, which is what says the listener has
            // stepped off the live edge.
            let at = tape.seek_target(30.0);
            let mut reader = tape.reader(at);
            let mut seen = Vec::new();
            for tick in 0..(period * 3) {
                if tick % period == period - 1 {
                    tape.append(&bytes(chunk));
                }

                let mut out = vec![0u8; 3_200];
                assert_eq!(reader.read(&mut out).unwrap(), 3_200);
                seen.push(tape.shift().behind_secs);
            }

            let first = seen[0];
            assert!(
                (first - 30.0).abs() < 1.0 + chunk_secs,
                "thirty seconds behind on a {chunk_secs}s chunk: {first}"
            );
            assert!(
                seen.iter().all(|behind| *behind == first),
                "and it never moved on a {chunk_secs}s chunk: {seen:?}"
            );
        }
    }

    /// Turning the setting down while a station plays gives the memory
    /// back now rather than at the next connect: the tape trims to the new
    /// length on the spot, and the shift says so.
    #[test]
    fn a_smaller_cap_trims_the_window_it_already_holds() {
        // 256 kbps is 32 kB/s, so sixty seconds is 1.92 MB.
        let tape = tape(60, 256);
        tape.append(&bytes(1_920_000));
        assert_eq!(tape.inner.lock().unwrap().buf.len(), 1_920_000);

        tape.set_cap_secs(10);

        assert_eq!(tape.cap_secs(), 10);
        assert_eq!(tape.inner.lock().unwrap().buf.len(), 320_000);
        assert_eq!(tape.shift().cap_secs, 10.0);
    }

    /// Turning it up keeps everything and just raises the ceiling. There's
    /// no way to fetch the minutes before the listener asked for them, so
    /// growing is the window filling into the new length from here.
    #[test]
    fn a_larger_cap_keeps_every_byte_it_already_had() {
        let tape = tape(60, 256);
        tape.append(&bytes(1_920_000));

        tape.set_cap_secs(600);

        assert_eq!(tape.cap_secs(), 600);
        assert_eq!(tape.inner.lock().unwrap().buf.len(), 1_920_000);
        let window = tape.shift().window_secs;
        assert!(
            (window - 60.0).abs() < 0.5,
            "what's held, not the cap: {window}"
        );
        assert_eq!(tape.shift().cap_secs, 600.0);
    }

    /// The rate the window is sized against rides along too, because it's
    /// what turns a length of buffer into the megabytes it costs.
    #[test]
    fn the_shift_carries_the_rate_the_window_is_sized_at() {
        let tape = tape(600, 256);
        tape.append(&bytes(64_000));

        assert_eq!(tape.shift().bytes_per_sec, 32_000.0);
    }

    /// The setting's own length rides along, so a strip can draw the whole
    /// buffer with the part that hasn't arrived yet marked as such.
    #[test]
    fn the_shift_carries_the_buffer_length_it_was_set_to() {
        let tape = tape(600, 256);
        tape.append(&bytes(64_000));

        let shift = tape.shift();
        assert_eq!(shift.cap_secs, 600.0);
        assert_eq!(shift.window_secs, 2.0, "and what has actually arrived");
    }

    /// The song boundaries a strip over the buffer draws: one per title
    /// still inside it, oldest first, each at its distance from the edge.
    #[test]
    fn the_marks_come_back_as_distances_from_the_edge() {
        // 256 kbps is 32 kB/s, so a second is 32000 bytes.
        let tape = tape(600, 256);
        let song = |name: &str| IcyTitle {
            artist: "Boards of Canada".into(),
            title: name.into(),
        };

        // The first title names what was already on when we tuned in, so
        // the three after it are the three boundaries.
        tape.mark_title(song("Telephasic Workshop"));
        tape.append(&bytes(32_000));
        tape.mark_title(song("Roygbiv"));
        tape.append(&bytes(96_000));
        tape.mark_title(song("Olson"));
        tape.append(&bytes(64_000));
        tape.mark_title(song("Rue the Whirl"));
        tape.append(&bytes(32_000));

        let marks = tape.live_marks();
        let seen: Vec<_> = marks
            .iter()
            .map(|mark| (mark.title.as_str(), mark.behind_secs))
            .collect();

        assert_eq!(
            seen,
            vec![("Roygbiv", 6.0), ("Olson", 3.0), ("Rue the Whirl", 1.0)],
            "oldest first, each back from the live edge"
        );
        assert!(marks.iter().all(|mark| mark.artist == "Boards of Canada"));
    }

    /// A song whose start has been trimmed off the back has nowhere on the
    /// strip to be drawn, so it stops being published even though the tape
    /// keeps the mark to know what's playing at the back of the window.
    #[test]
    fn a_mark_trimmed_off_the_back_stops_being_published() {
        // Ten seconds of window at 32 kB/s is 320 kB.
        let tape = tape(10, 256);
        let song = |name: &str| IcyTitle {
            artist: String::new(),
            title: name.into(),
        };

        tape.mark_title(song("tuning in"));
        tape.mark_title(song("first"));
        tape.append(&bytes(160_000));
        tape.mark_title(song("second"));
        assert_eq!(tape.live_marks().len(), 2, "both still inside");
        let rev = tape.marks_rev();

        // Ten more seconds, which is a window's worth: the trim takes the
        // first song's start with it.
        tape.append(&bytes(320_000));

        let marks = tape.live_marks();
        assert_eq!(marks.len(), 1, "the trimmed one went");
        assert_eq!(marks[0].title, "second");
        assert!(tape.marks_rev() > rev, "and the set said it changed");

        // The song at the back of the window still has a clock, since the
        // tape keeps the mark that names it even with its start gone.
        let _reader = tape.reader(0);
        assert!(tape.shift().song_secs.is_some());
    }

    /// The song clock a station has and no other source does: where the
    /// cursor sits between two title marks. A seek backwards lands in the
    /// middle of a song and has to read as the middle of it.
    #[test]
    fn the_marks_say_how_far_into_a_song_the_cursor_is() {
        // 256 kbps is 32 kB/s, so a second is 32000 bytes.
        let tape = tape(600, 256);
        let song = |title: &str| IcyTitle {
            artist: String::new(),
            title: title.to_string(),
        };

        tape.mark_title(song("first"));
        tape.append(&bytes(64_000));
        tape.mark_title(song("second"));
        tape.append(&bytes(64_000));

        // Nothing read yet, so the cursor is at the top of the first song.
        let shift = tape.shift();
        assert_eq!(shift.song_secs, Some(0.0));
        assert_eq!(shift.song_len_secs, Some(2.0), "mark to mark");

        // A second into the first song, which is still two long.
        let mut reader = tape.reader(32_000);
        let shift = tape.shift();
        assert_eq!(shift.song_secs, Some(1.0));
        assert_eq!(shift.song_len_secs, Some(2.0));

        // And into the second song, which hasn't ended, so it has no length.
        let mut out = vec![0u8; 48_000];
        assert_eq!(reader.read(&mut out).unwrap(), 48_000);
        let shift = tape.shift();
        assert_eq!(shift.song_secs, Some(0.5));
        assert_eq!(shift.song_len_secs, None, "the newest song is unfinished");
    }

    /// Before the station has announced anything there's no song to count
    /// from, and saying zero would be a claim the tape can't make.
    #[test]
    fn a_stream_with_no_marks_has_no_song_clock() {
        let tape = tape(600, 256);
        tape.append(&bytes(64_000));

        let shift = tape.shift();
        assert_eq!(shift.song_secs, None);
        assert_eq!(shift.song_len_secs, None);
    }

    /// A song whose start has been trimmed off the back still says how far
    /// in the cursor is, since the mark's offset outlives its bytes, but it
    /// won't claim a length: what's left isn't the whole song.
    #[test]
    fn a_song_trimmed_off_the_back_keeps_its_clock_and_loses_its_length() {
        let tape = tape(10, 256);
        let song = |title: &str| IcyTitle {
            artist: String::new(),
            title: title.to_string(),
        };

        tape.mark_title(song("first"));
        tape.append(&bytes(320_000));
        tape.mark_title(song("second"));
        tape.append(&bytes(64_000));

        // The window holds ten seconds, so the first song's start went.
        let start = tape.inner.lock().unwrap().start;
        assert!(start > 0, "the oldest bytes were dropped");

        let _reader = tape.reader(start + 16_000);
        let shift = tape.shift();
        assert_eq!(
            shift.song_secs,
            Some((start + 16_000) as f64 / 32_000.0),
            "counted from the mark, gone bytes and all"
        );
        assert_eq!(shift.song_len_secs, None, "and no length to claim");
    }

    /// The station's own word is the rate, and a measurement never
    /// replaces it. Every number the strip draws is bytes over this, so a
    /// figure that keeps being revised rescales the whole picture: the
    /// filled bar grows and shrinks, the marks slide, the playhead moves
    /// while the music doesn't. Radio is constant bitrate, so one number
    /// decided early and left alone is both the steadier answer and the
    /// truer one.
    #[test]
    fn a_stated_rate_is_never_replaced_by_a_measurement() {
        // Claims 256 kbps, which is 32 kB/s.
        let tape = tape(600, 256);
        let song = |name: &str| IcyTitle {
            artist: String::new(),
            title: name.into(),
        };

        tape.mark_title(song("tuning in"));
        feed(&tape, 320_000);
        tape.mark_title(song("first"));
        feed(&tape, 320_000);

        let mut reader = tape.reader(0);
        let window = tape.shift().window_secs;
        let mark = tape.live_marks()[0].behind_secs;
        assert_eq!(window, 20.0, "twenty seconds at the stated rate");

        // Twenty seconds of audio out of 480 kB, which measures 24 kB/s: a
        // quarter off what the station said, and ignored.
        for _ in 0..20 {
            let mut out = vec![0u8; 24_000];
            assert_eq!(reader.read(&mut out).unwrap(), 24_000);
            tape.note_audio(1.0);
        }

        assert_eq!(tape.shift().window_secs, window, "the bar didn't move");
        assert_eq!(tape.live_marks()[0].behind_secs, mark, "nor the mark");
        assert_eq!(tape.shift().bytes_per_sec, 32_000.0);
    }

    /// A station that says nothing about itself settles once, on the first
    /// ten seconds the decoder accounts for, and holds that. The numbers
    /// move the once, on the settle, and never again.
    #[test]
    fn an_unstated_rate_settles_once_and_then_holds() {
        let tape = tape(600, 0);
        feed(&tape, 640_000);

        let mut reader = tape.reader(0);
        let mut seen = vec![tape.shift().window_secs];

        // A second of audio per 32 kB read, which is the 32 kB/s the header
        // never mentioned. The first tick opens the measurement window, the
        // tenth after it closes it.
        for _ in 0..16 {
            let mut out = vec![0u8; 32_000];
            assert_eq!(reader.read(&mut out).unwrap(), 32_000);
            tape.note_audio(1.0);
            seen.push(tape.shift().window_secs);
        }

        let mut distinct = seen.clone();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            2,
            "one settle and nothing after it: {seen:?}"
        );

        // Twenty seconds of tape at the real 32 kB/s, give or take the
        // first tick: opening the measurement window throws away what was
        // read before it, which is the read-ahead in a real session and a
        // whole second of it here.
        let settled = *seen.last().expect("samples");
        assert!(
            (settled - 20.0).abs() < 3.0,
            "about twenty seconds of tape at the measured rate: {settled}"
        );
    }

    #[test]
    fn a_seek_outside_the_window_is_refused() {
        let tape = tape(600, 256);
        tape.append(&bytes(1000));

        let mut reader = tape.reader(500);
        assert_eq!(reader.seek(SeekFrom::Start(750)).unwrap(), 750);
        assert!(reader.seek(SeekFrom::Start(2000)).is_err());
        assert!(reader.seek(SeekFrom::End(-10)).is_err());
    }

    /// A seek to the edge lands a snap distance behind it, so the decoder
    /// has bytes in hand instead of blocking on the next chunk; a seek
    /// further back than that is honoured as asked.
    #[test]
    fn a_seek_to_live_lands_a_chunk_behind_the_edge() {
        let tape = tape(600, 128);
        tape.set_feed(Feed::Live);
        let bps = tape.inner.lock().unwrap().rate.bytes_per_sec();
        // A minute of stream in 16 KiB chunks, so the edge is well past
        // the snap distance and the lead is the snap, not the chunk.
        let chunk = bytes(16 * 1024);
        while (tape.inner.lock().unwrap().head() as f64) < bps * 60.0 {
            tape.append(&chunk);
        }
        let head = tape.inner.lock().unwrap().head();

        let at_live = tape.seek_target(0.0);
        let lead = head - at_live;
        let want = (LIVE_EDGE_SNAP_SECS * bps) as u64;
        assert!(
            lead >= want && lead <= want + chunk.len() as u64,
            "lead {lead} vs {want}"
        );

        let back = tape.seek_target(30.0);
        assert!(
            head - back > lead,
            "an asked-for distance is further than the lead"
        );
    }
}
