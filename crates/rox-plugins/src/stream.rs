//! A plugin's open stream, read by offset. Each read asks the plugin for a
//! whole chunk and serves the caller from it, and after a read the next
//! chunk is already on its way, so the decode thread mostly reads memory.
//!
//! The host never sends overlapping ranges on one stream. A seekable stream
//! can have up to `depth` reads in flight at consecutive offsets; a stream
//! that can't seek (live, or a server that ignores ranges) has one at a
//! time, each starting where the last answer ended, which is the only offset
//! such a plugin can serve.
//!
//! Downloading a whole track is the engine's, which reads through this one
//! read at a time; `buffer_whole` carries the plugin's say in it.
//!
//! A stream belongs to the process that opened it. Once that process exits
//! every read fails at once, without starting a new one: the caller's
//! recovery reopens through the opener, which does.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::host::{Conn, Host, Pending};
use crate::wire::{self, MAX_READ};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Asked of the plugin per read, whatever the caller asked for.
    pub chunk: u32,
    /// Chunks kept fetched or on their way beyond the one being read.
    pub depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            chunk: 256 * 1024,
            depth: 1,
        }
    }
}

/// How each read was answered, for the prototype's measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// From a chunk already in memory.
    pub memory: u64,
    /// From a read-ahead still in flight when the read came.
    pub waited: u64,
    /// Nothing fetched or asked for covered it: a cold read.
    pub cold: u64,
}

pub struct Stream {
    host: Host,
    conn: Arc<Conn>,
    id: String,
    pub hint: String,
    pub length: Option<u64>,
    pub seekable: bool,
    pub live: bool,
    /// False when the plugin asked for read-ahead only.
    pub buffer_whole: bool,
    options: Options,
    ahead: Mutex<Ahead>,
    memory: AtomicU64,
    waited: AtomicU64,
    cold: AtomicU64,
}

#[derive(Default)]
struct Ahead {
    ready: VecDeque<Chunk>,
    flight: VecDeque<Flight>,
    /// Where an empty answer said the stream ends.
    end: Option<u64>,
}

struct Chunk {
    at: u64,
    bytes: Vec<u8>,
}

impl Chunk {
    fn end(&self) -> u64 {
        self.at + self.bytes.len() as u64
    }
}

struct Flight {
    at: u64,
    len: u32,
    pending: Pending,
}

impl Flight {
    fn covers(&self, offset: u64) -> bool {
        self.at <= offset && offset < self.at + self.len as u64
    }
}

impl Ahead {
    fn serve(&self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let chunk = self
            .ready
            .iter()
            .find(|c| c.at <= offset && offset < c.end())?;

        let from = (offset - chunk.at) as usize;
        let to = chunk.bytes.len().min(from + len);

        Some(chunk.bytes[from..to].to_vec())
    }

    /// Where the next read-ahead starts: past everything asked for.
    fn next(&self, served_to: u64) -> u64 {
        let asked = self.flight.back().map(|f| f.at + f.len as u64);
        let fetched = self.ready.back().map(Chunk::end);

        asked.or(fetched).unwrap_or(served_to).max(served_to)
    }

    fn beyond(&self, served_to: u64) -> usize {
        let fetched = self.ready.iter().filter(|c| c.at >= served_to).count();

        fetched + self.flight.len()
    }
}

impl Stream {
    /// `source.open`, starting the plugin if it isn't running.
    pub fn open(host: &Host, key: &str, options: Options) -> Result<Stream, String> {
        let conn = host.conn()?;
        let value = host
            .send_on(
                &conn,
                "source.open",
                json!({ "key": key }),
                host.timeouts().open,
            )?
            .wait()?;
        let open: wire::Open = wire::decode(value)?;

        let chunk = options.chunk.clamp(1, MAX_READ);
        let seekable = open.seekable && !open.live;

        Ok(Stream {
            host: host.clone(),
            conn,
            id: open.stream,
            hint: open.hint,
            length: open.length,
            seekable,
            live: open.live,
            buffer_whole: open.buffer != Some(wire::Buffer::Ahead),
            options: Options { chunk, ..options },
            ahead: Mutex::new(Ahead::default()),
            memory: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            cold: AtomicU64::new(0),
        })
    }

    /// False once the process it was opened on has exited.
    pub fn alive(&self) -> bool {
        self.conn.alive()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            memory: self.memory.load(Ordering::Relaxed),
            waited: self.waited.load(Ordering::Relaxed),
            cold: self.cold.load(Ordering::Relaxed),
        }
    }

    /// At most `len` bytes from `offset`, empty at the end. Blocks for as
    /// long as the plugin takes, up to the read timeout.
    pub fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        if !self.conn.alive() {
            return Err("the plugin exited".into());
        }

        let mut ahead = self
            .ahead
            .lock()
            .map_err(|_| "the stream's buffer is poisoned")?;
        let past_end = ahead.end.or(self.length).is_some_and(|end| offset >= end);
        if past_end || len == 0 {
            return Ok(Vec::new());
        }

        // Nothing reads behind itself here: the caller keeps its own window.
        ahead.ready.retain(|c| c.end() > offset);

        let mut first = true;
        let bytes = loop {
            if let Some(bytes) = ahead.serve(offset, len) {
                if first {
                    self.memory.fetch_add(1, Ordering::Relaxed);
                }
                break bytes;
            }

            if ahead.end.is_some_and(|end| offset >= end) {
                break Vec::new();
            }

            match ahead.flight.iter().position(|f| f.covers(offset)) {
                // Land everything up to it in order, so an unseekable stream's
                // answers stay in sequence.
                Some(at) => {
                    if first {
                        self.waited.fetch_add(1, Ordering::Relaxed);
                    }

                    for _ in 0..=at {
                        let flight = ahead.flight.pop_front().expect("counted above");
                        self.land(&mut ahead, flight)?;
                    }
                }

                // Whatever is queued was for somewhere else.
                None => {
                    if first {
                        self.cold.fetch_add(1, Ordering::Relaxed);
                    }

                    ahead.flight.clear();
                    ahead.ready.clear();

                    let flight = self.ask(offset)?;
                    ahead.flight.push_back(flight);
                }
            }

            first = false;
        };

        let served_to = offset + bytes.len() as u64;
        self.top_up(&mut ahead, served_to);

        Ok(bytes)
    }

    fn ask(&self, at: u64) -> Result<Flight, String> {
        let len = self.options.chunk;
        let params = json!({ "stream": self.id, "offset": at, "len": len });
        let pending =
            self.host
                .send_on(&self.conn, "source.read", params, self.host.timeouts().read)?;

        Ok(Flight { at, len, pending })
    }

    fn land(&self, ahead: &mut Ahead, flight: Flight) -> Result<(), String> {
        let value = flight.pending.wait()?;
        let read: wire::Read = wire::decode(value)?;
        let bytes = read.bytes(flight.len)?;

        match bytes.is_empty() {
            true => ahead.end = Some(flight.at),
            false => ahead.ready.push_back(Chunk {
                at: flight.at,
                bytes,
            }),
        }

        Ok(())
    }

    /// Keeps `depth` chunks fetched or asked for beyond what was served. A
    /// failed ask is left for the next read to find.
    fn top_up(&self, ahead: &mut Ahead, served_to: u64) {
        if ahead.end.is_some() || !self.conn.alive() {
            return;
        }

        while ahead.beyond(served_to) < self.options.depth {
            // One at a time where the next offset isn't known until the last
            // answer is in.
            if !self.seekable && !ahead.flight.is_empty() {
                return;
            }

            let at = ahead.next(served_to);
            if self.length.is_some_and(|len| at >= len) {
                return;
            }

            match self.ask(at) {
                Ok(flight) => ahead.flight.push_back(flight),
                Err(_) => return,
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.conn.alive() {
            return;
        }

        // Nobody waits on the answer; the reader throws it away.
        let _ = self.host.send_on(
            &self.conn,
            "source.close",
            json!({ "stream": self.id }),
            self.host.timeouts().read,
        );
    }
}
