# Plugin host

Can a plugin that runs as a subprocess and speaks JSON-RPC over its stdin and
stdout carry browsing, syncing and streaming at what playback needs? [ADR
30](../02-architecture/decisions/30-adr-plugins.md) decided the mechanism and
left the numbers to a prototype: how long a cold open takes when the plugin
runs a downloader, whether it stalls pause and seek, what read size and
read-ahead keep a stream fed through the pipe, and what a plugin costs to
write. This entry records what the prototype host measured, and what those
numbers settle.

Unlike the entries before it, the prototype isn't thrown away. The host is
`crates/rox-plugins`, and the services side is `rox-services/src/plugins.rs`.

## What was built

The host crate loads a plugin folder the way the contract describes: a strict
manifest, a SHA-256 folder hash that refuses symlinks, the entry resolved per
platform or through an interpreter alias table, and a subprocess started in its
own folder with `PYTHONDONTWRITEBYTECODE=1` set. On Unix it gets its own
process group, and on Windows a kill-on-close job object. A host has one writer
thread, one reader thread and a table of pending requests, so any number of
calls can be in flight and answered in any order. It speaks `hello` before
anything else, restarts a crashed plugin on its next call with a 0, 1, 2 and 4
second backoff, and stops it for good after five crashes in ten minutes.

The services bridge installs the engine's stream opener, pre-opens the next two
plugin entries once the audible track has held for two seconds, syncs every enabled
collection once at start, and fetches covers through `source.cover` for rows
that have none stored.

Around it, the stage's decided calls: a stream whose read fails reopens with
radio's backoff and resumes at the same byte (or refuses if the reopened length
differs), a live plugin stream plays from radio's feed thread and tape, listens
relink by source and key, and a plugin record removed by hand departs on the
next start.

## How it was measured

On an Apple Silicon Mac, macOS 26.6, Python 3.15 release candidate 2,
September 2026. Two plugins:

- the echo fixture in `crates/rox-plugins/tests/fixtures/echo/`, which
  generates its bytes and touches no network, so it shows what the pipe and the
  host cost on their own
- an external plugin written against the contract alone, outside this
  repository: it lists and streams a streaming service's catalog through a
  downloader the user installs, and serves the audio as the service's own
  AAC-in-MP4 files

Headless numbers came from a harness outside the repository that drives
`rox-plugins` directly, built in release. In-app numbers came from a debug
build of rox driven over `roxctl`, with the engine's open timer and the
services opener logging each open.

## Starting a plugin

| | Spawn | Spawn and `hello` |
|---|---|---|
| Echo fixture, 10 starts | under 4 ms | median 22 ms, worst 59 ms |
| External plugin, 5 starts | under 4 ms | median 398 ms, worst 779 ms |
| External plugin, in the app | 0.4 to 3.6 ms | 390 to 509 ms |

The external plugin's `hello` runs its downloader once to read its version,
which is nearly all of those 400 ms. The spawn itself is free.

## Opening a track

Twenty cold `source.open` calls through the external plugin, each one a fresh
extraction by the downloader: nineteen answered in a median of 2651 ms (2260
best, 3776 worst). The twentieth hit the 20 second timeout, and the same key
then opened three times in a row in 1881, 2041 and 2203 ms, so the hang was
transient. Its cause isn't known: the harness kept no plugin log.

Cold opens in the app ran 1.76 to 3.56 s. The fixture opens in 2 to 23 ms.

## Reading

Sustained throughput, reading one track front to back as fast as the host
would go:

| Chunk | Echo, 1 in flight | Echo, 2 | External, 1 in flight | External, 2 |
|---|---|---|---|---|
| 64 KiB | 15.5 MiB/s | 15.4 | 1.73 MiB/s | 1.85 |
| 256 KiB | 16.9 | 17.3 | 7.11 | 7.26 |
| 512 KiB | 17.2 | 17.7 | 8.49 | 7.11 |

The pipe itself isn't the limit anywhere near audio rates: the slowest cell,
1.73 MiB/s, is about 14 Mbit/s, fifty times a 256 kbps stream and ten times
CD-quality lossless. Through the external plugin every read is one ranged
request upstream, so a small chunk pays a round trip per 64 KiB. A second read
in flight bought nothing, because that plugin serializes reads on one stream.

What a seek or a resume waits for is one cold read, not throughput: a median of
87 ms at 64 KiB (32 to 155), 68 ms at 256 KiB (58 to 85), and 71 ms at 512 KiB
(43 to 109). Chunk size barely moves it; the upstream round trip is the cost.

Base64 is cheap on both sides. The host parses and decodes a full 512 KiB read
answer in 0.30 ms, 0.6 ms per MiB. All host work while streaming came to 1.5 to
2.3 ms of CPU per MiB against the fixture and 3.4 to 5.8 ms against the
external plugin, and the external plugin's own work to 13 to 40 ms per MiB. At
256 kbps that's about 0.2 ms of host CPU and at most 1.2 ms of plugin CPU per
second of audio.

## Whole-track buffering

The first build read one chunk ahead. After the in-app test, Andrew's call was
to download the whole track instead, the way a streaming site buffers a video:
the seekbar can show what's downloaded, a seek inside it costs nothing, and the
waveform can be decoded from those same bytes, so the track is fetched once.

The engine does it (`rox-playback/src/download.rs`), so it covers a server's
files as well as plugin streams: a Subsonic file answered by range, with a
length and no interleaved metadata, downloads the same way. A seekable stream
of known length up to 64 MB downloads front to back on its own thread, one
read at a time, from the engine's open. A read the download hasn't reached
moves it there, and the part it jumped over fills in after. Anything larger,
unseekable or live keeps reading as it plays. A pre-opened plugin stream
doesn't download until the engine opens it, so skipping past one costs an
open, not a download. A plugin can ask for read-ahead only (`buffer: "ahead"`
on `source.open`), for a service that meters or throttles fast downloads; it
can lower the buffering that way, never raise the cap.

In the app, plugin tracks of 3.49 to 5.68 MB downloaded whole in 0.58 to
0.93 s. The waveform decoded from the downloaded bytes in 767 and 774 ms in the
debug build and went into the peak cache. Four seeks across a downloaded track
landed and played on at once; the plugin logged nothing, and its CPU for the
whole session was 0.27 s. The Subsonic path is covered by tests against a fake
transport only: no server was configured for the in-app runs.

64 MB holds any lossy track, and by typical bitrates roughly 9 to 12 minutes of
CD-quality lossless or 2.5 to 3.5 minutes of 24-bit/96 kHz. Those are estimates
from bitrates, not measured.

## In the engine

**Pause during a cold open waits for it.** Commands drain at the top of the
decode loop, so a pause pressed while the opener is running lands when the open
returns. Of four jumps to a cold track with a pause 0.1 s after, three logged
the pause waiting behind opens of 2.06, 2.19 and 2.29 s. ADR 29's first
amendment inferred this; it's now measured.

**A seek right after a jump landed on the track just left.** The engine
seeked the audible track (`seek_to`), and after a jump the new track isn't
audible until the position clock reaches it. With a skip crossfade on, as it was
in these runs (4 s), the clock flips at the fade's midpoint, 2 s after the
jump, and a cold open adds its own seconds before that. A seek in that window
reopened the track just left and seeked it. All three trials did it, each a
seek 0.1 s after the jump, two of them on tracks that opened from a pre-open in
under a millisecond. Local files had the same window whenever a skip
crossfades. Fixed: the engine now seeks the track its last skip or jump opened
until another track is adopted, and a seek 0.15 s after a skip in the app
stayed on the new track.

**Pre-opens hit.** Over nine sequential advances, every open found its stream
already opened (the opener answered in 0.1 to 1.4 ms). A jump to anything
beyond the next two entries opens cold, as expected. Skipping fast started two
pre-opens per skip, each a full extraction, and most were never used, so a
pre-open now waits until the audible track has held for 2 s. The next open in
the app joined that pre-open in 418 ms.

**A cold open was silent in the UI.** The transport's waiting spinner followed
the audible track's stream state, and during a jump's open the audible track is
still the one being left, so nothing showed for two to three seconds. The
engine now publishes the entry a skip, jump or queue start is opening, and not
a gapless pre-decode's. The play button spins, the waveform shows its
generating stand-in, and the track's row in the library spins beside its
title while that open runs.

## Recovery

Killing the external plugin mid-track: playback carried on from bytes already
fetched, the next read found the plugin gone, the host restarted it (`hello` in
391 ms), the stream reopened cold in 2.1 s and resumed at byte 524288 on the
same track. The clock's pace suggests a short stall while the reopen ran; nobody
listened for it.

Killing it five times inside ten minutes stopped the host with "Stopped after
repeated crashes". The stream's own recovery ran out, the track dropped with
that refusal, the queue skipped every remaining entry of that plugin with the
same reason, and a local file then played normally.

The echo fixture's live stream played from the tape. Paused for a minute, the
feed kept taping (13 seconds of tape grew to 72) and resume picked up at the
pause, 58.9 s behind live. Killing the fixture mid-stream restarted it, reopened
at the live edge and recovered within a second. The idle hang-up after thirty
paused minutes wasn't run in the app.

## What the numbers settle

- **Timeouts.** `hello` 5 s stays: the slowest seen was 779 ms. `source.open`
  20 s stays: normal opens top out under 4 s, and the one that hung would have
  hung past any shorter limit too. `source.read` 10 s stays: cold reads are
  under 160 ms, and the margin is for a plugin recovering its own upstream
  inside the read. `source.cover` 10 s and `shutdown` 2 s stay. Listing pages
  stay at 15 s, and a sync's first page gets 60 s.
- **Chunk size: 256 KiB.** It gets nearly all of 512 KiB's throughput through
  a real plugin, four times 64 KiB's, and every size seeks at the same cost.
- **Read-ahead: the whole track** for a seekable stream up to 64 MB, one chunk
  otherwise or when the plugin asks. A second read in flight showed no gain.
- **Pre-open: the next two entries**, opened once the audible track has held
  for 2 s, closed if nothing takes them within 60 s, downloaded only once
  playing.

## Answers to the external plugin's questions

The external plugin was written against the contract alone, and it found nine
places the contract was silent. The host answers each:

- **Environment.** The plugin inherits rox's environment, PATH and HOME
  included, plus `PYTHONDONTWRITEBYTECODE=1`. A plugin can read its own test
  switches from an environment variable, as the external one does.
- **A `null` result.** It's an answer, not a missing one: the host tells a
  present `"result": null` from an absent field, so `source.close`, `shutdown`
  and a cover the plugin doesn't have all read as success.
- **The `jsonrpc` field.** Accepted on every inbound frame, and checked to be
  `"2.0"` when present.
- **Before `hello` answers.** Nothing else is sent. Every other call waits for
  the handshake.
- **Shutdown with requests in flight.** The host sends `shutdown` and waits up
  to 2 s for its answer. Requests still in flight can answer inside that grace;
  after it they fail, stdin closes, and the process group is killed. When rox
  quits it skips the grace and hangs up at once.
- **A first sync page over 15 s.** A 13-track collection's whole first page took
  1.1 s. A plugin that lists the whole collection on the first page to compute
  its token will take longer on thousands of tracks, which nothing here
  measured. A sync's first page gets 60 s and the rest keep 15 s.
- **The token on `unchanged: true`.** The host keeps the token it stored and
  ignores any token in that answer.
- **Overlapping reads on one stream.** The host never sends overlapping ranges.
  A seekable stream sees its next chunk asked for while the last is being
  used, at the next offset: one read ahead, which the download keeps busy. A
  seek moves where the next read starts. An unseekable stream sees one read at
  a time, each starting where the last answer ended. A plugin still has to
  accept a read on one stream while another stream's read is running.
- **Programs.** `programs` is information for the user, so it should list every
  program the plugin runs, including a JavaScript runtime its downloader needs.
  The external plugin lists the downloader and not the runtime.

## What writing the plugin cost

The external plugin is one Python file of 1055 lines on the standard library,
with 789 lines of tests that run offline against a stubbed downloader and a
local HTTP server. Its tests run on macOS and Linux; the stub needs a shebang,
so not on Windows. A per-step account of the time it took was asked for and
isn't recorded anywhere this writeup could find.

## Found along the way

- **Live audio needs a container symphonia reads.** The pinned symphonia 0.6.0
  has no MPEG-TS reader, and none exists in its full feature set: its formats
  are CAF, ISO MP4, Matroska, Ogg, AIFF and WAV, plus ADTS AAC and MP3 through
  their codecs. A live plugin whose upstream sends segmented TS has to remux to
  one of those. It also has to answer each read with what it has ready rather
  than waiting to fill `len`: a 256 KiB read at 128 kbps would take 16 s to
  fill, past the read timeout.
- **A live plugin tape measures its own rate.** `Tape::new` takes a stated
  bitrate and a seek snap. A plugin stream passes no bitrate (the locator
  doesn't carry one) and the tape measures from the decoder, as it does for a
  station that sends none. The snap comes from the stream's container hint:
  Ogg and Opus scan to a page, everything else lands anywhere.
- **A strict manifest made additions breaking.** Refusing unknown keys at
  every level meant a field a newer host adds refused the plugin on an older
  one. Now only the top level and `entry` are strict (a new top-level key bumps
  `api`); `meta` and `capabilities` ignore keys they don't know.
- **Scrobbling checks both halves.** A plugin row scrobbles only when its
  loaded manifest declares it and its record leaves it on; either alone
  isn't enough.
- **Auto-generated mixes don't sync through the external plugin.** It turns
  any list id into a plain playlist URL, and the service refuses that for a
  mix. That's the plugin's to fix.
- **Remote waveforms come from the download.** An earlier build filled a
  waveform from the visualizer tap as a track played; it failed its one
  full-play test and was dropped. A plugin track or a server's file now decodes
  its waveform from the downloaded bytes, keyed in the peak cache on its source
  and key, so a plugin key that reads like a path never meets a local file's
  entry.
- **Every plugin can reach the log.** A plugin's stderr goes to the log line by
  line under its id, so a chatty plugin fills it. Lines are capped at 4 KiB.

## Still open

- A plugin built on a client library that decodes inside the plugin, to test
  ADR 30's inference that such a plugin can serve container bytes and skip a
  PCM contract.
- A live-capable external plugin, to test live streams against a real
  broadcast rather than a looped file.
- The idle hang-up for a paused live plugin stream, run in the app.
