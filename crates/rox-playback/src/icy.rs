//! Shoutcast/Icecast in-band metadata, stripped back out of the byte stream
//! before the decoder ever sees it.
//!
//! A station that agrees to send metadata answers with `icy-metaint: N` and
//! then interleaves the stream: N bytes of audio, one length byte, that many
//! sixteen-byte units of text, N bytes of audio again, forever. Symphonia has
//! no idea any of that is there. Hand it the raw body and every metadata block
//! lands in the decoder as garbage frames, which is a click at best and a
//! desync at worst.
//!
//! So this is a wrapper rather than a branch inside the transport. The reader
//! underneath stays a plain `Read` that knows nothing about stations, and the
//! stripping is one layer that either exists or doesn't, depending on whether
//! the response carried the header. Nothing downstream has to ask which kind
//! of stream it's on.
//!
//! For a live station this runs on the feed thread, between the socket and
//! the tape, so the titles and the tee see the stream in the order the
//! station sent it whatever the decoder is doing. The title callback the
//! transport installs there only records where each one was found; what
//! publishes it is the decode side reaching that point in the tape, which is
//! how a listener a few minutes behind gets told the song they're hearing.
//!
//! The text blocks are also the only now-playing a station has. There's no
//! catalog to look a track up in and no duration to show, so a `StreamTitle`
//! change is the whole track-change event for web radio.
//!
//! That makes this the one place in rox that can save a song off the air.
//! The bytes going past here are the station's own container, already
//! encoded, and the title blocks say where one song stops and the next
//! starts. So there's a second tee below the title sink, handing those
//! bytes and those boundaries to whoever asked for them. It only ever
//! copies and hands over: the buffering, the start-to-finish rule and the
//! write all belong to the service on the other end of the channel, which
//! is not on the decode thread.

use std::io::Read;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// One `StreamTitle=` update, split on the " - " convention every station
/// follows. `artist` is empty when the station sends one unsplittable field,
/// which is common enough that it can't be treated as a parse failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IcyTitle {
    pub artist: String,
    pub title: String,
}

/// Where a station's title updates go. Handed down from whoever opened the
/// track, because the reader firing these sits three layers below anything
/// that knows which queue entry it belongs to.
///
/// `Arc` rather than a plain box: the sink outlives the open that installed
/// it, since titles keep arriving for as long as the stream plays, and the
/// same one is cheap to hand to a reconnect's fresh reader. `Sync` because
/// the reader ends up inside a `MediaSource`, which is `Send + Sync`.
pub type TitleSink = Arc<dyn Fn(IcyTitle) + Send + Sync>;

/// A sink that drops everything, for an open with nobody to show a title to:
/// the analysis passes, and every local file, which has no metadata band in
/// it to begin with.
pub fn no_titles() -> TitleSink {
    Arc::new(|_| {})
}

/// One step of the raw stream, for whoever is saving songs off it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    /// A block named a song other than the one before it. Every byte fed
    /// before this belongs to the song that just ended; every byte after
    /// it belongs to the one starting.
    Boundary(IcyTitle),
    /// A run of container bytes, exactly as the station sent them. Batched,
    /// because a read is a few kilobytes and a channel send per read would
    /// put the decode thread's time into bookkeeping.
    Bytes(Vec<u8>),
    /// The body is gone: a reconnect, a hang-up, or the stream ending.
    /// Whatever was mid-capture was cut short, so it can't be saved.
    End,
}

/// Where the raw bytes go. Same shape and same reason as [`TitleSink`]:
/// this fires from inside the read on the decode thread, so whatever is
/// behind it has to be a channel send and nothing more.
pub type CaptureSink = Arc<dyn Fn(CaptureEvent) + Send + Sync>;

/// A sink that drops everything, for a build with nobody saving anything.
pub fn no_capture() -> CaptureSink {
    Arc::new(|_| {})
}

/// How many bytes pile up before a batch goes out, a handful of reads'
/// worth. Small enough that a capture never holds much in the reader, big
/// enough that the sink fires a few times a second rather than hundreds.
const BATCH: usize = 16 * 1024;

/// The installed tee. Written once, by whoever holds the other end of the
/// channel; read once per connection, when a reader is built.
static TEE: RwLock<Option<CaptureSink>> = RwLock::new(None);

/// Whether the tee is actually fed. Separate from [`TEE`] so the switch
/// can move without the channel being torn down and rebuilt, and read per
/// read so flipping it mid-stream takes effect at the next song rather
/// than the next station.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Install the tee every station reader built from here on will feed.
/// Nothing reaches it until [`set_capturing`] turns the switch on.
pub fn tee_to(sink: CaptureSink) {
    if let Ok(mut tee) = TEE.write() {
        *tee = Some(sink);
    }
}

/// Turn the tee on or off.
pub fn set_capturing(on: bool) {
    ARMED.store(on, Ordering::Relaxed);
}

/// Whether bytes are being copied right now, the per-read gate.
fn capturing() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// The installed tee, or nothing on a process where none was installed.
fn installed_tee() -> Option<CaptureSink> {
    TEE.read().ok()?.clone()
}

/// Strips in-band metadata out of `inner` so a decoder reading through this
/// sees nothing but audio. Titles go out through the callback as they change.
///
/// The callback is `Sync` as well as `Send` because a reader wrapped in this
/// ends up inside a [`symphonia_core::io::MediaSource`], and that trait is
/// `Send + Sync`.
pub struct IcyReader<R: Read> {
    inner: R,
    /// Audio bytes between two metadata blocks, straight off `icy-metaint`.
    metaint: usize,
    /// Audio bytes still owed before the next block. Zero means the next byte
    /// off the inner reader is a block length.
    until_meta: usize,
    /// The last title handed to the callback. Stations repeat the current
    /// title in every block, so without this the callback fires on the
    /// keepalive rather than on the song change.
    last: String,
    on_title: Box<dyn Fn(IcyTitle) + Send + Sync>,
    /// The byte tee, or None on a reader built before anything installed
    /// one. Grabbed at construction rather than per read: a connection
    /// either has somewhere to copy to or it doesn't.
    capture: Option<CaptureSink>,
    /// Bytes owed to the tee, held back until [`BATCH`] or the next
    /// boundary, whichever comes first.
    batch: Vec<u8>,
}

impl<R: Read> IcyReader<R> {
    pub fn new(
        inner: R,
        metaint: usize,
        on_title: impl Fn(IcyTitle) + Send + Sync + 'static,
    ) -> Self {
        Self::with_capture(inner, metaint, on_title, installed_tee())
    }

    /// [`IcyReader::new`] with the byte tee named outright. The public
    /// constructor takes whatever the app installed, which is a process
    /// global and therefore no good to a test.
    pub fn with_capture(
        inner: R,
        metaint: usize,
        on_title: impl Fn(IcyTitle) + Send + Sync + 'static,
        capture: Option<CaptureSink>,
    ) -> Self {
        Self {
            inner,
            metaint,
            until_meta: metaint,
            last: String::new(),
            on_title: Box::new(on_title),
            capture,
            batch: Vec::new(),
        }
    }

    /// Hand the held bytes over. Called on every boundary as well as on
    /// the size threshold, so a batch never straddles two songs.
    fn flush_batch(&mut self) {
        if self.batch.is_empty() {
            return;
        }

        let Some(capture) = self.capture.clone() else {
            self.batch.clear();
            return;
        };

        capture(CaptureEvent::Bytes(std::mem::take(&mut self.batch)));
    }

    /// Read exactly `buf.len()` bytes, or report that the stream ended before
    /// they arrived. A metadata block can land across as many inner reads as
    /// the socket feels like splitting it into, and the caller can't be handed
    /// half of one, so this is where the boundary gets waited out.
    fn fill(&mut self, buf: &mut [u8]) -> IoResult<bool> {
        let mut got = 0;
        while got < buf.len() {
            let n = self.inner.read(&mut buf[got..])?;
            if n == 0 {
                return Ok(false);
            }
            got += n;
        }
        Ok(true)
    }

    /// Consume the metadata block sitting at the cursor and arm the next audio
    /// run. False means the stream ended inside the block.
    fn consume_meta(&mut self) -> IoResult<bool> {
        let mut len = [0u8; 1];
        if !self.fill(&mut len)? {
            return Ok(false);
        }

        // Zero length is the keepalive every station sends between title
        // changes, and it's the overwhelmingly common case.
        let bytes = len[0] as usize * 16;
        if bytes == 0 {
            self.until_meta = self.metaint;
            return Ok(true);
        }

        let mut block = vec![0u8; bytes];
        if !self.fill(&mut block)? {
            return Ok(false);
        }

        // A block that doesn't parse is nothing to stop playback over. The
        // audio is fine either way, and a station with a broken tagger would
        // otherwise be unlistenable.
        if let Some(title) = parse_title(&block)
            && title != self.last
        {
            self.last = title.clone();
            let split = split_title(&title);
            (self.on_title)(split.clone());

            // The bytes read up to here are the last song's, so they go
            // out before the boundary that ends them.
            if self.capture.is_some() && capturing() {
                self.flush_batch();
                if let Some(capture) = self.capture.clone() {
                    capture(CaptureEvent::Boundary(split));
                }
            }
        }

        self.until_meta = self.metaint;
        Ok(true)
    }
}

impl<R: Read> Read for IcyReader<R> {
    fn read(&mut self, out: &mut [u8]) -> IoResult<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        // Sitting on a block boundary, so it has to come out of the stream
        // before any audio can go back. Returning zero bytes here instead
        // would read as end of stream to everything upstream.
        if self.until_meta == 0 && !self.consume_meta()? {
            return Ok(0);
        }

        // Never read past the next boundary in one go. Short reads are legal
        // and the next call picks the metadata up.
        let want = out.len().min(self.until_meta);
        let n = self.inner.read(&mut out[..want])?;
        self.until_meta -= n;

        // The copy is the whole tee. One relaxed load while capture is off,
        // and a memcpy into a growing buffer while it's on.
        if n > 0 && self.capture.is_some() && capturing() {
            self.batch.extend_from_slice(&out[..n]);
            if self.batch.len() >= BATCH {
                self.flush_batch();
            }
        }

        Ok(n)
    }
}

/// Dropping the reader is how a station's connection ends, whether that's
/// a reconnect building a fresh one, the queue moving on, or a pause left
/// running long enough to give the socket up. Either way the song being
/// read was cut in the middle, so the held bytes go nowhere and the tee
/// just hears that it happened.
impl<R: Read> Drop for IcyReader<R> {
    fn drop(&mut self) {
        let Some(capture) = self.capture.clone() else {
            return;
        };

        capture(CaptureEvent::End);
    }
}

/// The `StreamTitle` value out of one metadata block, None when the block
/// holds no title or isn't text at all. The block is NUL padded to a multiple
/// of sixteen and holds `key='value';` pairs, `StreamUrl` being the other one
/// stations send.
fn parse_title(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let rest = text.split_once("StreamTitle='")?.1;

    // Terminated by the quote-semicolon pair rather than the first quote,
    // because a title with an apostrophe in it is ordinary ("Rock 'n' Roll").
    let value = match rest.find("';") {
        Some(end) => &rest[..end],
        None => rest.trim_end_matches('\0').trim_end_matches('\''),
    };

    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Split a station's title field on the first " - ". Everything sends artist
/// first, and the ones that don't send a single field with no separator at
/// all, which comes back as a title with no artist.
fn split_title(value: &str) -> IcyTitle {
    match value.split_once(" - ") {
        Some((artist, title)) => IcyTitle {
            artist: artist.trim().to_string(),
            title: title.trim().to_string(),
        },

        None => IcyTitle {
            artist: String::new(),
            title: value.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// One metadata block: the length byte plus the text padded out to the
    /// sixteen-byte units the length counts.
    fn meta(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        while !bytes.len().is_multiple_of(16) {
            bytes.push(0);
        }
        let mut out = vec![(bytes.len() / 16) as u8];
        out.extend_from_slice(&bytes);
        out
    }

    /// A reader that hands back at most `chunk` bytes per call, for the tests
    /// about boundaries landing mid-block.
    struct Choppy {
        data: Vec<u8>,
        at: usize,
        chunk: usize,
    }

    impl Read for Choppy {
        fn read(&mut self, out: &mut [u8]) -> IoResult<usize> {
            let n = out.len().min(self.chunk).min(self.data.len() - self.at);
            out[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    /// The arming switch is a process global, so the tee's tests take
    /// turns at it rather than racing each other's reads.
    static ARM: Mutex<()> = Mutex::new(());

    /// Every capture event an `IcyReader` over `data` fires, with the tee
    /// armed for the length of the call and the reader dropped at the end
    /// of it, so `End` is always the last one.
    fn tee(data: Vec<u8>, metaint: usize) -> Vec<CaptureEvent> {
        let _held = ARM.lock().unwrap_or_else(|e| e.into_inner());
        set_capturing(true);

        let seen: Arc<Mutex<Vec<CaptureEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let capture: CaptureSink = Arc::new(move |e| sink.lock().unwrap().push(e));

        {
            let src = Choppy {
                data,
                at: 0,
                chunk: 1024,
            };
            let mut reader = IcyReader::with_capture(src, metaint, |_| {}, Some(capture));
            let mut buf = [0u8; 7];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        }

        set_capturing(false);

        let events = seen.lock().unwrap();
        events.clone()
    }

    /// Everything an `IcyReader` over `data` hands back, plus the titles it
    /// fired on the way through.
    fn drain(data: Vec<u8>, metaint: usize, chunk: usize) -> (Vec<u8>, Vec<IcyTitle>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let src = Choppy { data, at: 0, chunk };
        let mut reader = IcyReader::new(src, metaint, move |t| sink.lock().unwrap().push(t));

        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
        let titles = seen.lock().unwrap().clone();

        (out, titles)
    }

    #[test]
    fn metadata_never_reaches_the_caller() {
        let audio: Vec<u8> = (0..64u8).collect();
        let mut stream = Vec::new();
        stream.extend_from_slice(&audio[..32]);
        stream.extend_from_slice(&meta("StreamTitle='Boards of Canada - Roygbiv';"));
        stream.extend_from_slice(&audio[32..]);
        stream.extend_from_slice(&meta(""));

        let (out, _) = drain(stream, 32, 1024);
        assert_eq!(out, audio);
    }

    #[test]
    fn a_title_change_fires_once() {
        let mut stream = vec![0u8; 16];
        stream.extend_from_slice(&meta("StreamTitle='Aphex Twin - Xtal';"));
        stream.extend_from_slice(&[0u8; 16]);
        // The same title again is the station repeating itself, not a new
        // song, so it must not fire a second time.
        stream.extend_from_slice(&meta("StreamTitle='Aphex Twin - Xtal';"));
        stream.extend_from_slice(&[0u8; 16]);
        stream.extend_from_slice(&meta("StreamTitle='Autechre - Rae';"));
        stream.extend_from_slice(&[0u8; 16]);

        let (out, titles) = drain(stream, 16, 1024);
        assert_eq!(out.len(), 64);
        assert_eq!(
            titles,
            vec![
                IcyTitle {
                    artist: "Aphex Twin".into(),
                    title: "Xtal".into(),
                },
                IcyTitle {
                    artist: "Autechre".into(),
                    title: "Rae".into(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_block_is_skipped() {
        let mut stream = vec![1u8; 8];
        stream.push(0);
        stream.extend_from_slice(&[2u8; 8]);
        stream.push(0);

        let (out, titles) = drain(stream, 8, 1024);
        assert_eq!(out, [vec![1u8; 8], vec![2u8; 8]].concat());
        assert!(titles.is_empty());
    }

    #[test]
    fn a_title_with_no_separator_is_all_title() {
        let mut stream = vec![0u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='NTS Radio 1';StreamUrl='';"));
        stream.extend_from_slice(&[0u8; 8]);

        let (_, titles) = drain(stream, 8, 1024);
        assert_eq!(
            titles,
            vec![IcyTitle {
                artist: String::new(),
                title: "NTS Radio 1".into(),
            }]
        );
    }

    #[test]
    fn a_block_split_across_reads_still_parses() {
        let mut stream = vec![9u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='Burial - Archangel';"));
        stream.extend_from_slice(&[9u8; 8]);

        // Three bytes at a time puts the length byte, the title, and the audio
        // either side of it across a dozen inner reads.
        let (out, titles) = drain(stream, 8, 3);
        assert_eq!(out, vec![9u8; 16]);
        assert_eq!(
            titles,
            vec![IcyTitle {
                artist: "Burial".into(),
                title: "Archangel".into(),
            }]
        );
    }

    #[test]
    fn an_apostrophe_in_a_title_survives() {
        let mut stream = vec![0u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='Guns N' Roses - Sweet Child O' Mine';"));
        stream.extend_from_slice(&[0u8; 8]);

        let (_, titles) = drain(stream, 8, 1024);
        assert_eq!(
            titles,
            vec![IcyTitle {
                artist: "Guns N' Roses".into(),
                title: "Sweet Child O' Mine".into(),
            }]
        );
    }

    #[test]
    fn the_tee_splits_the_bytes_at_the_title_changes() {
        let mut stream = vec![1u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='Aphex Twin - Xtal';"));
        stream.extend_from_slice(&[2u8; 8]);
        stream.extend_from_slice(&meta("StreamTitle='Autechre - Rae';"));
        stream.extend_from_slice(&[3u8; 8]);
        stream.push(0);

        assert_eq!(
            tee(stream, 8),
            vec![
                // The eight bytes before the first block never reach the
                // tee: they are the tail of whatever was playing when we
                // connected, and no boundary opened them.
                CaptureEvent::Bytes(vec![1u8; 8]),
                CaptureEvent::Boundary(IcyTitle {
                    artist: "Aphex Twin".into(),
                    title: "Xtal".into(),
                }),
                CaptureEvent::Bytes(vec![2u8; 8]),
                CaptureEvent::Boundary(IcyTitle {
                    artist: "Autechre".into(),
                    title: "Rae".into(),
                }),
                // The last song's bytes die with the reader. Nothing said
                // where it ends, so there is no song there to save.
                CaptureEvent::End,
            ]
        );
    }

    #[test]
    fn a_repeated_title_is_no_boundary() {
        let mut stream = vec![1u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='Burial - Archangel';"));
        stream.extend_from_slice(&[2u8; 8]);
        stream.extend_from_slice(&meta("StreamTitle='Burial - Archangel';"));
        stream.extend_from_slice(&[3u8; 8]);
        stream.push(0);

        let boundaries = tee(stream, 8)
            .into_iter()
            .filter(|e| matches!(e, CaptureEvent::Boundary(_)))
            .count();

        assert_eq!(boundaries, 1, "the keepalive is not a song change");
    }

    #[test]
    fn the_tee_stays_quiet_while_capture_is_off() {
        let _held = ARM.lock().unwrap_or_else(|e| e.into_inner());
        set_capturing(false);

        let seen: Arc<Mutex<Vec<CaptureEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let capture: CaptureSink = Arc::new(move |e| sink.lock().unwrap().push(e));

        let mut stream = vec![1u8; 8];
        stream.extend_from_slice(&meta("StreamTitle='Aphex Twin - Xtal';"));
        stream.extend_from_slice(&[2u8; 8]);
        stream.push(0);

        {
            let src = Choppy {
                data: stream,
                at: 0,
                chunk: 1024,
            };
            let mut reader = IcyReader::with_capture(src, 8, |_| {}, Some(capture));
            let mut buf = [0u8; 7];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        }

        // Only the drop is heard: nothing was copied, so there is nothing
        // for the service on the other end to throw away either.
        assert_eq!(*seen.lock().unwrap(), vec![CaptureEvent::End]);
    }
}
