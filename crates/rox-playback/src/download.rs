//! A remote track downloaded whole while it plays: a plugin's stream or a
//! file off a server. Seeks inside what's here cost nothing, the seekbar
//! shows what's here ([`Buffered`]), and the waveform decodes these same
//! bytes once they're all in, so the track is fetched once.
//!
//! A thread reads the stream front to back, one read at a time. A read the
//! download hasn't reached moves it there, and what it jumped over fills in
//! after. Only a seekable stream of known length up to [`CAP`] is downloaded
//! whole; the opener decides, and anything else reads as it did.
//!
//! Dropping the [`Download`] stops the thread and closes the stream beneath
//! it. The bytes live until the last [`Buffered`] handle to them goes, and
//! the engine only ever publishes weak ones.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::plugin::ReadAt;

/// The most one track holds in memory.
pub const CAP: u64 = 64 * 1024 * 1024;

/// Asked of the stream per read.
const CHUNK: usize = 256 * 1024;

/// How long a read waits for the download to bring its bytes.
const READ_WAIT: Duration = Duration::from_secs(10);

/// What the seekbar and the waveform read of a download.
pub trait Buffered: Send + Sync {
    fn length(&self) -> u64;

    /// What's here, as sorted half-open byte ranges.
    fn ranges(&self) -> Vec<(u64, u64)>;

    /// Every byte, once all of them are here.
    fn bytes(&self) -> Option<Arc<[u8]>>;

    /// The container's extension, empty when unknown.
    fn hint(&self) -> &str;
}

/// The [`ReadAt`] the decoder reads through. Dropping it stops the download.
pub struct Download {
    inner: Arc<Inner>,
}

struct Inner {
    length: u64,
    hint: String,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    /// Filled as the download goes; handed over to `whole` once complete.
    data: Vec<u8>,
    whole: Option<Arc<[u8]>>,
    filled: Vec<(u64, u64)>,
    /// Where a waiting read needs the download to go next.
    want: Option<u64>,
    /// The read on its way.
    flight: Option<(u64, u64)>,
    failed: Option<String>,
    stopped: bool,
}

impl State {
    fn covering(&self, at: u64) -> Option<(u64, u64)> {
        self.filled
            .iter()
            .copied()
            .find(|(start, end)| *start <= at && at < *end)
    }

    fn insert(&mut self, start: u64, end: u64) {
        self.filled.push((start, end));
        self.filled.sort_unstable();

        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.filled.len());
        for (start, end) in self.filled.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }

        self.filled = merged;
    }

    /// The first missing byte at or after `from`, wrapping to the top once,
    /// and where that gap ends.
    fn gap(&self, from: u64, length: u64) -> Option<(u64, u64)> {
        let after = |from: u64| {
            let mut at = from;
            for (start, end) in &self.filled {
                if *end <= at {
                    continue;
                }
                if *start > at {
                    return Some((at, *start));
                }
                at = *end;
            }

            (at < length).then_some((at, length))
        };

        after(from).or_else(|| after(0))
    }

    fn slice(&self, from: u64, to: u64) -> Vec<u8> {
        let (from, to) = (from as usize, to as usize);

        match &self.whole {
            Some(whole) => whole[from..to].to_vec(),
            None => self.data[from..to].to_vec(),
        }
    }
}

impl Download {
    /// Starts downloading `reader` on its own thread. `length` must be the
    /// stream's real length, at most [`CAP`].
    pub fn start(
        reader: Box<dyn ReadAt>,
        length: u64,
        hint: &str,
    ) -> Result<(Download, Arc<dyn Buffered>), String> {
        let inner = Arc::new(Inner {
            length,
            hint: hint.to_string(),
            state: Mutex::new(State {
                data: vec![0; length as usize],
                whole: None,
                filled: Vec::new(),
                want: None,
                flight: None,
                failed: None,
                stopped: false,
            }),
            changed: Condvar::new(),
        });

        let running = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("download".into())
            .spawn(move || running.run(reader))
            .map_err(|e| format!("could not start the download: {e}"))?;

        let buffered: Arc<dyn Buffered> = inner.clone();

        Ok((Download { inner }, buffered))
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.stopped = true;
        }
        self.inner.changed.notify_all();
    }
}

impl Inner {
    fn run(&self, reader: Box<dyn ReadAt>) {
        let began = Instant::now();
        let mut cursor = 0;

        loop {
            let (at, len) = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                if state.stopped {
                    return;
                }
                if let Some(want) = state.want.take() {
                    cursor = want;
                }

                let Some((start, end)) = state.gap(cursor, self.length) else {
                    let data = std::mem::take(&mut state.data);
                    state.whole = Some(Arc::from(data));
                    drop(state);
                    self.changed.notify_all();

                    log::info!(
                        "download: {} bytes in {:.2} s",
                        self.length,
                        began.elapsed().as_secs_f64()
                    );

                    return;
                };

                let len = (end - start).min(CHUNK as u64);
                state.flight = Some((start, start + len));

                (start, len as usize)
            };

            let answer = reader.read_at(at, len);

            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.flight = None;

            let bytes = match answer {
                Ok(bytes) if bytes.is_empty() => {
                    state.failed = Some(format!(
                        "the stream ended at byte {at}, short of {}",
                        self.length
                    ));
                    drop(state);
                    self.changed.notify_all();

                    return;
                }

                Ok(bytes) => bytes,

                Err(e) => {
                    state.failed = Some(e);
                    drop(state);
                    self.changed.notify_all();

                    return;
                }
            };

            // A stream that answers more than it was asked for is cut to fit.
            let n = bytes.len().min(len);
            let end = at + n as u64;
            state.data[at as usize..end as usize].copy_from_slice(&bytes[..n]);
            state.insert(at, end);
            cursor = end;
            drop(state);

            self.changed.notify_all();
        }
    }
}

impl ReadAt for Download {
    /// Waits for the download to bring `offset`, moving it there if its next
    /// read won't. A failed download fails the read, which is what starts
    /// the caller's recovery.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let inner = &self.inner;
        if offset >= inner.length || len == 0 {
            return Ok(Vec::new());
        }

        let deadline = Instant::now() + READ_WAIT;
        let mut state = inner.state.lock().map_err(|_| "the download is poisoned")?;

        loop {
            if let Some((_, end)) = state.covering(offset) {
                return Ok(state.slice(offset, end.min(offset + len as u64)));
            }

            if let Some(failed) = &state.failed {
                return Err(failed.clone());
            }

            let coming = state
                .flight
                .is_some_and(|(start, end)| start <= offset && offset < end);
            if !coming {
                state.want = Some(offset);
            }

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no bytes at {offset} within {}s",
                    READ_WAIT.as_secs()
                ));
            }

            state = inner
                .changed
                .wait_timeout(state, left)
                .map_err(|_| "the download is poisoned")?
                .0;
        }
    }
}

impl Buffered for Inner {
    fn length(&self) -> u64 {
        self.length
    }

    fn ranges(&self) -> Vec<(u64, u64)> {
        self.state
            .lock()
            .map(|state| state.filled.clone())
            .unwrap_or_default()
    }

    fn bytes(&self) -> Option<Arc<[u8]>> {
        self.state.lock().ok()?.whole.clone()
    }

    fn hint(&self) -> &str {
        &self.hint
    }
}

/// Whether a stream this shape is downloaded whole.
pub fn whole(length: Option<u64>, seekable: bool) -> Option<u64> {
    length.filter(|len| seekable && (1..=CAP).contains(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Serves `bytes`, counting reads, failing every read once `fail_after`
    /// reads have gone by.
    struct Memory {
        bytes: Vec<u8>,
        reads: Arc<AtomicUsize>,
        fail_after: usize,
        delay: Duration,
    }

    impl ReadAt for Memory {
        fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
            if self.reads.fetch_add(1, Ordering::Relaxed) >= self.fail_after {
                return Err("the plugin exited".into());
            }
            std::thread::sleep(self.delay);

            let start = (offset as usize).min(self.bytes.len());
            let end = (start + len).min(self.bytes.len());

            Ok(self.bytes[start..end].to_vec())
        }
    }

    fn download(len: usize, fail_after: usize) -> (Download, Arc<dyn Buffered>, Arc<AtomicUsize>) {
        download_paced(len, fail_after, Duration::ZERO)
    }

    fn download_paced(
        len: usize,
        fail_after: usize,
        delay: Duration,
    ) -> (Download, Arc<dyn Buffered>, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let memory = Memory {
            bytes: pattern(len),
            reads: Arc::clone(&reads),
            fail_after,
            delay,
        };
        let (download, buffered) = Download::start(Box::new(memory), len as u64, "m4a").unwrap();

        (download, buffered, reads)
    }

    fn wait_complete(buffered: &Arc<dyn Buffered>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while buffered.bytes().is_none() {
            assert!(Instant::now() < deadline, "the download never finished");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn ranges_merge_as_they_meet() {
        let mut state = State {
            data: Vec::new(),
            whole: None,
            filled: Vec::new(),
            want: None,
            flight: None,
            failed: None,
            stopped: false,
        };
        state.insert(0, 10);
        state.insert(20, 30);
        assert_eq!(state.filled, vec![(0, 10), (20, 30)]);

        state.insert(10, 20);
        assert_eq!(state.filled, vec![(0, 30)]);

        assert_eq!(state.gap(0, 50), Some((30, 50)));
        state.filled = vec![(0, 10), (50, 100)];
        assert_eq!(
            state.gap(60, 100),
            Some((10, 50)),
            "past a jump, the skipped part"
        );
    }

    #[test]
    fn the_whole_stream_comes_down_and_hands_over_every_byte() {
        let len = CHUNK * 3 + 17;
        let (download, buffered, _) = download(len, usize::MAX);

        assert_eq!(download.read_at(0, 100).unwrap(), pattern(100));
        wait_complete(&buffered);

        assert_eq!(&buffered.bytes().unwrap()[..], &pattern(len)[..]);
        assert_eq!(buffered.ranges(), vec![(0, len as u64)]);
        assert_eq!(buffered.hint(), "m4a");
        assert_eq!(
            download.read_at(len as u64 - 5, 100).unwrap(),
            pattern(len)[len - 5..]
        );
        assert!(download.read_at(len as u64, 10).unwrap().is_empty());
    }

    #[test]
    fn a_read_past_the_download_moves_it_and_the_gap_fills_after() {
        let len = CHUNK * 20;
        let (download, buffered, _) = download(len, usize::MAX);

        let far = (CHUNK * 15 + 3) as u64;
        assert_eq!(
            download.read_at(far, 10).unwrap(),
            pattern(len)[far as usize..][..10]
        );

        wait_complete(&buffered);
        assert_eq!(&buffered.bytes().unwrap()[..], &pattern(len)[..]);
    }

    #[test]
    fn bytes_already_here_outlive_a_failed_stream() {
        let len = CHUNK * 4;
        let (download, buffered, _) = download(len, 2);

        // Two reads land, then the stream dies.
        let deadline = Instant::now() + Duration::from_secs(5);
        while buffered.ranges() != vec![(0, (CHUNK * 2) as u64)] {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(download.read_at(10, 10).unwrap(), pattern(len)[10..20]);
        let err = download.read_at((CHUNK * 3) as u64, 10).unwrap_err();
        assert_eq!(
            err, "the plugin exited",
            "the failure reaches the caller's recovery"
        );
        assert!(buffered.bytes().is_none());
    }

    #[test]
    fn dropping_the_download_stops_it() {
        let len = CHUNK * 200;
        let (download, buffered, reads) = download_paced(len, usize::MAX, Duration::from_millis(2));
        download.read_at(0, 1).unwrap();

        drop(download);
        std::thread::sleep(Duration::from_millis(50));
        let after = reads.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(
            reads.load(Ordering::Relaxed),
            after,
            "no reads after the drop"
        );
        assert!(buffered.bytes().is_none());
    }

    #[test]
    fn only_a_seekable_stream_of_known_length_under_the_cap_is_whole() {
        assert_eq!(whole(Some(10), true), Some(10));
        assert_eq!(whole(Some(10), false), None);
        assert_eq!(whole(None, true), None);
        assert_eq!(whole(Some(0), true), None);
        assert_eq!(whole(Some(CAP + 1), true), None);
    }
}
