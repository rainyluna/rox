//! A plugin's stream as the decoder sees it (ADR 29's first amendment, ADR
//! 30). The plugin serves container bytes by offset, and [`PluginSource`]
//! reads them like a file, with a window of recent bytes so the probe's small
//! backward seeks never go back to the plugin. A seek outside the window is a
//! read at the new offset, and on a stream that can't seek it's an error.
//!
//! The engine never knows what's behind an [`Opener`]. It's passed in with
//! the queue, so this crate never depends on the plugin host.
//!
//! [`ReadAt::read_at`] blocks the decode thread for as long as the plugin
//! takes, and it's never called from the output callback. Read-ahead and
//! timeouts belong to the host side of the opener, as do reconnects: a plugin
//! that loses its upstream recovers inside its own read.

use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::sync::Arc;

use rox_library::locator::PluginStream;
use symphonia::core::io::MediaSource;

pub trait ReadAt: Send + Sync {
    /// At most `len` bytes from `offset`; empty means end of stream.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String>;
}

pub struct Opened {
    /// Dropping it closes the stream.
    pub reader: Box<dyn ReadAt>,
    /// Container extension for the probe; empty lets it sniff.
    pub hint: String,
    pub length: Option<u64>,
    pub seekable: bool,
}

pub type Opener = Arc<dyn Fn(&PluginStream) -> Result<Opened, String> + Send + Sync>;

/// Asked of the plugin per read.
const CHUNK: usize = 256 * 1024;

/// Sized like `HttpSource`'s, for the same reason: a big ID3v2 tag pushes the
/// container header this far, and the probe walks back over it.
const WINDOW: usize = 1 << 20;

pub struct PluginSource {
    reader: Box<dyn ReadAt>,
    seekable: bool,
    len: Option<u64>,
    /// The only place a backward seek on an unseekable stream can be answered.
    buf: Vec<u8>,
    /// Stream offset of `buf[0]`; the cursor never leaves the window.
    buf_start: u64,
    pos: u64,
}

impl PluginSource {
    pub fn new(reader: Box<dyn ReadAt>, length: Option<u64>, seekable: bool) -> PluginSource {
        PluginSource {
            reader,
            seekable,
            len: length,
            buf: Vec::new(),
            buf_start: 0,
            pos: 0,
        }
    }

    /// Where the next read from the plugin starts.
    fn head(&self) -> u64 {
        self.buf_start + self.buf.len() as u64
    }

    /// One read at the head. False at the end of the stream.
    fn fill(&mut self) -> io::Result<bool> {
        let bytes = self
            .reader
            .read_at(self.head(), CHUNK)
            .map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Ok(false);
        }

        self.buf.extend_from_slice(&bytes);
        self.trim();

        Ok(true)
    }

    /// Drops the oldest half on overflow, never past the cursor, so a plugin
    /// that answers more than it was asked for still gets read in full.
    fn trim(&mut self) {
        if self.buf.len() <= WINDOW {
            return;
        }

        let behind = (self.pos - self.buf_start) as usize;
        let drop = (self.buf.len() - WINDOW / 2).min(behind);
        self.buf.drain(..drop);
        self.buf_start += drop as u64;
    }
}

impl Read for PluginSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        if self.pos == self.head() && !self.fill()? {
            return Ok(0);
        }

        let off = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf.len() - off);
        out[..n].copy_from_slice(&self.buf[off..off + n]);
        self.pos += n as u64;

        Ok(n)
    }
}

impl Seek for PluginSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(at) => Some(at),

            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),

            // Symphonia asks this of sources it hasn't checked `is_seekable` on.
            SeekFrom::End(delta) => match self.len {
                Some(len) => len.checked_add_signed(delta),
                None => return Err(io::Error::other("no length to seek from the end of")),
            },
        }
        .ok_or_else(|| io::Error::other("seek out of range"))?;

        // The head counts as inside: it's where the next read goes anyway.
        if target >= self.buf_start && target <= self.head() {
            self.pos = target;
            return Ok(target);
        }

        if !self.seekable {
            return Err(io::Error::other("stream is not seekable"));
        }

        self.buf.clear();
        self.buf_start = target;
        self.pos = target;

        Ok(target)
    }
}

impl MediaSource for PluginSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serves `bytes`, at most `most` per read, and records every offset asked for.
    struct Memory {
        bytes: Vec<u8>,
        most: usize,
        asked: Mutex<Vec<u64>>,
    }

    impl Memory {
        fn new(bytes: Vec<u8>, most: usize) -> Arc<Memory> {
            Arc::new(Memory {
                bytes,
                most,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<u64> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl ReadAt for Arc<Memory> {
        fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
            self.asked.lock().unwrap().push(offset);

            let start = (offset as usize).min(self.bytes.len());
            let end = (start + len.min(self.most)).min(self.bytes.len());

            Ok(self.bytes[start..end].to_vec())
        }
    }

    struct Failing;

    impl ReadAt for Failing {
        fn read_at(&self, _: u64, _: usize) -> Result<Vec<u8>, String> {
            Err("upstream went away".into())
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn source(memory: &Arc<Memory>, seekable: bool) -> PluginSource {
        let len = memory.bytes.len() as u64;

        PluginSource::new(Box::new(Arc::clone(memory)), Some(len), seekable)
    }

    #[test]
    fn a_sequential_read_returns_every_byte_then_ends() {
        let bytes = pattern(CHUNK * 3 + 17);
        let memory = Memory::new(bytes.clone(), usize::MAX);
        let mut src = source(&memory, true);

        let mut out = Vec::new();
        src.read_to_end(&mut out).expect("the stream reads");

        assert_eq!(out, bytes);
        assert_eq!(src.read(&mut [0; 16]).unwrap(), 0, "and stays ended");
        assert_eq!(src.byte_len(), Some(bytes.len() as u64));
    }

    #[test]
    fn short_reads_still_add_up_to_the_whole_stream() {
        let bytes = pattern(10_000);
        let memory = Memory::new(bytes.clone(), 333);
        let mut src = source(&memory, false);

        let mut out = Vec::new();
        src.read_to_end(&mut out).expect("the stream reads");

        assert_eq!(out, bytes);

        // Each read picks up exactly where the last one stopped, which is all an
        // unseekable plugin can answer.
        let asked = memory.asked();
        for pair in asked.windows(2) {
            assert!(pair[1] - pair[0] <= 333);
        }
    }

    #[test]
    fn a_seek_inside_the_window_asks_the_plugin_nothing() {
        let bytes = pattern(CHUNK * 2);
        let memory = Memory::new(bytes.clone(), usize::MAX);
        let mut src = source(&memory, false);

        let mut head = [0; 4096];
        src.read_exact(&mut head).unwrap();
        let asked = memory.asked().len();

        assert_eq!(src.seek(SeekFrom::Start(10)).unwrap(), 10);
        let mut again = [0; 32];
        src.read_exact(&mut again).unwrap();

        assert_eq!(&again[..], &bytes[10..42]);
        assert_eq!(memory.asked().len(), asked, "served from memory");
    }

    #[test]
    fn a_seek_outside_the_window_reads_at_the_new_offset() {
        let bytes = pattern(WINDOW * 3);
        let memory = Memory::new(bytes.clone(), usize::MAX);
        let mut src = source(&memory, true);

        let target = (WINDOW * 2 + 5) as u64;
        assert_eq!(src.seek(SeekFrom::Start(target)).unwrap(), target);

        let mut out = [0; 64];
        src.read_exact(&mut out).unwrap();

        assert_eq!(&out[..], &bytes[target as usize..target as usize + 64]);
        assert_eq!(memory.asked().last(), Some(&target));

        // Back to the top is outside the window again.
        src.seek(SeekFrom::Start(0)).unwrap();
        src.read_exact(&mut out).unwrap();
        assert_eq!(&out[..], &bytes[..64]);
    }

    #[test]
    fn an_unseekable_stream_refuses_a_seek_it_cant_serve() {
        let bytes = pattern(WINDOW * 3);
        let memory = Memory::new(bytes, usize::MAX);
        let mut src = source(&memory, false);

        // Read far enough that the start has left the window.
        let mut out = vec![0; WINDOW * 2];
        src.read_exact(&mut out).unwrap();

        let back = src.seek(SeekFrom::Start(0));
        assert!(
            back.is_err(),
            "the start is gone and the plugin can't go back"
        );

        let ahead = src.seek(SeekFrom::Start((WINDOW * 3 - 1) as u64));
        assert!(ahead.is_err(), "nor skip ahead");

        assert!(!src.is_seekable());
    }

    #[test]
    fn a_plugin_error_surfaces_as_an_io_error() {
        let mut src = PluginSource::new(Box::new(Failing), None, false);

        let err = src.read(&mut [0; 16]).expect_err("the read fails");
        assert_eq!(err.to_string(), "upstream went away");

        assert!(
            src.seek(SeekFrom::End(0)).is_err(),
            "no length to count back from"
        );
    }
}
