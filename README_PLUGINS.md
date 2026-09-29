# Plugins

A plugin brings a source from outside rox into the library: rox browses, searches,
syncs and plays it through a program the plugin supplies. The plugin is a folder
holding a manifest and that program. rox starts the program as a subprocess and
speaks newline-delimited JSON-RPC 2.0 with it over stdin and stdout.

rox doesn't ship plugins for any service. [`examples/plugins/tones`](examples/plugins/tones/)
is a working source to copy: it generates sine tones and chords, so it needs no
network and nothing beyond Python 3's standard library.

This guide covers plugin API version 1, the only one the host supports.

## Installing one

1. Turn on Experimental Panels in Settings > Development, then Enable Plugins in
   Settings > Application. That shows the Plugins page.
2. Press Reveal Folder on the Plugins page. It opens the plugins folder, creating it
   the first time.
3. Drop the plugin's folder in. The folder's name has to be the plugin's id. The page
   picks it up with its switch off.
4. Switch it on. The first time, a card shows what the plugin declares and which
   programs it runs, and asks you to confirm.

The plugin's source then shows up in the External Sources panel (Add Panel >
Experimental). Keep in the library on a collection syncs it into the library.
Playing, queueing or adding a single track to a playlist adds just that track.

Remove on the Plugins page drops the plugin's tracks, synced collections, settings and
approval. Its folder stays where it is.

## Trust

A plugin runs as a program on your computer with your permissions, its own network
access and its own filesystem access. rox doesn't sandbox it, and the enable card says
so.

Switching a plugin on approves exactly the files in its folder, by a SHA-256 hash over
every file in it. The approval is stored per machine, so a copied settings file
doesn't bring someone else's trust decision along. When any file in the folder
changes, the plugin switches off and its row reads Changed on disk. Switching it on
again shows what changed in the manifest since the last approval: capabilities added
or dropped, new programs, the scrobble declaration, a different entry.

Some things about the hash matter when you write a plugin:

- A write into the plugin's own folder changes its hash and switches it off. Write
  only under the `data_dir` that `hello` hands you.
- A symlink anywhere in the folder refuses the plugin, and so does a plugin folder
  that is itself a symlink. So does a file name that isn't UTF-8.
- `.DS_Store`, `Thumbs.db` and `desktop.ini` are left out of the hash, since the OS
  writes them. `__pycache__` is hashed, because Python would run a planted `.pyc`.
  rox sets `PYTHONDONTWRITEBYTECODE=1` for every plugin so a Python plugin's first
  import doesn't write one.

The gate doesn't defend against other software on the machine: anything that can
write the plugins folder can write rox's settings too. It makes sure nothing runs
that the user didn't switch on, and shows what a plugin declares before any of it
runs.

## The folder

```
<data>/plugins/<id>/        the plugin, as the user dropped it
    plugin.json
    <entry and anything else it ships>
<data>/plugin-data/<id>/    where the plugin writes, created on its first start
```

`<data>` is rox's data directory, or `rox-data` beside the executable in portable
mode. rox reads every subfolder of `plugins`, parses its manifest, hashes it and
checks the programs it lists, all without running anything. A folder that can't load
still shows on the Plugins page with the reason. A folder whose manifest `id` doesn't
match the folder's name is refused.

## The manifest

`plugin.json`, at most 256 KiB. The tones example's:

```json
{
  "id": "tones",
  "name": "Tones",
  "version": "0.1.0",
  "api": 1,
  "entry": {
    "script": { "path": "tones.py", "interpreter": "python3" }
  },
  "meta": {
    "author": "rox",
    "description": "An example source plugin. ...",
    "website": "https://github.com/zealsprince/rox",
    "license": "AGPL-3.0-only"
  },
  "capabilities": {
    "source": { "label": "Tones", "scrobble": false }
  },
  "programs": [],
  "config_schema": {
    "type": "object",
    "properties": {
      "volume": { "type": "integer", "title": "Volume", "minimum": 1, "maximum": 100 }
    }
  }
}
```

| Key                   | Meaning                                                                                                          |
| --------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `id`                  | `^[a-z0-9][a-z0-9-]{1,63}$`. The plugin's rows are filed under it for good, so it never changes once you have users. |
| `name`, `version`     | Shown on the Plugins page and the enable card.                                                                   |
| `api`                 | The plugin API version it targets: `1`.                                                                          |
| `entry`               | Exactly one of `script` or `native`.                                                                             |
| `meta`                | `author`, `description`, `website`, `license`, `version`, all optional. The card shows the author and description. |
| `capabilities.source` | `label` names the source in rox. `scrobble` defaults to false.                                                   |
| `programs`            | Programs the plugin runs, by name. The page reports each as found on PATH or missing. rox doesn't enforce the list. |
| `config_schema`       | JSON Schema for the plugin's settings.                                                                           |

A script entry runs `<interpreter> <path>` from inside the plugin folder. `python3`
tries `python3`, then `python`, then `py -3` on Windows. `node` tries `node`. Any other
name is looked up on PATH as written, with Windows' executable extensions tried after
the bare name. The path has to be a plain relative path to a file in the folder.

A native entry names a binary per platform, keyed `<os>-<arch>` with Rust's names
(`linux`, `windows`, `macos`; `x86_64`, `aarch64`):

```json
"entry": {
  "native": {
    "linux-x86_64": "bin/tones",
    "windows-x86_64": "bin/tones.exe",
    "macos-aarch64": "bin/tones-macos"
  }
}
```

The Plugins page draws `config_schema.properties` as settings rows. It understands
`string`, `string` with `"format": "password"` (a masked field), `number`, `integer`,
`boolean`, and any property with an `enum`. A property's `title` labels the row and its
`description` shows under it. Anything else shows as raw JSON. Values are stored
in plaintext with the rest of rox's account settings. A change restarts the plugin
with the new config once the edit ends.

A manifest is refused, with the reason on the Plugins page, when:

- it isn't valid JSON, is over 256 KiB, or isn't a plain file
- it has an unknown top-level key, or an unknown key inside `entry` or `entry.script`
- the `id` doesn't match the pattern, or `api` isn't a version the host supports
- `entry` names both kinds or neither
- there's no native build for this platform, the interpreter isn't on PATH, or the
  entry path leaves the folder or doesn't exist

Unknown keys inside `meta` and `capabilities` are ignored, so fields added there later
don't break older hosts. A new top-level key means a new `api` version. A plugin with
no `capabilities.source` loads but never starts, since source is the only capability
the host runs.

## Wire format

rox writes requests to the plugin's stdin and reads answers from its stdout. One JSON
object per line, UTF-8, each line at most 1 MiB. Every request has an `id`, and the
answer echoes it with either `result` or `error: {"code": n, "message": ".."}`.
`jsonrpc` is optional on answers and has to be `"2.0"` when present. `"result": null`
is a valid answer.

rox can have many requests in flight, and a plugin may answer them in any order. A
plugin that answers one at a time works, but its own playback then waits behind its
own searches. The tones example runs everything but `shutdown` on a thread pool.

Answers are parsed strictly. A result with a field rox doesn't know is refused, and a
line that doesn't parse or answers an id rox never sent is logged and dropped. An
error's message is shown to the user when the call was theirs (a browse, a sync) and
logged otherwise.

stderr is free text. Each line goes to rox's log as `plugin <id>: <line>`, cut at 4 KiB.
The plugin never sends requests to rox.

## Methods

| Method           | Params                            | Answers                                          |
| ---------------- | --------------------------------- | ------------------------------------------------ |
| `hello`          | `api`, `config`, `data_dir`, `platform` | `{name, version, api}`                     |
| `source.browse`  | `node`, `cursor`                  | a page of entries                                |
| `source.search`  | `query`, `cursor`                 | a page of entries                                |
| `source.sync`    | `collection`, `token`, `cursor`   | a page of tracks with the collection's token     |
| `source.open`    | `key`                             | a stream id and what rox needs to read it        |
| `source.read`    | `stream`, `offset`, `len`         | `{data}`, base64                                 |
| `source.close`   | `stream`                          | null                                             |
| `source.cover`   | `key`                             | `{mime, data}` with the image base64, or null    |
| `shutdown`       |                                   | null, then the plugin exits                      |

### hello

The first request, and nothing else is sent until it's answered:

```
→ {"jsonrpc":"2.0","id":1,"method":"hello","params":{"api":1,"config":{"volume":30},"data_dir":"/home/me/.local/share/rox/plugin-data/tones","platform":"linux-x86_64"}}
← {"jsonrpc":"2.0","id":1,"result":{"name":"Tones","version":"0.1.0","api":1}}
```

`config` is the plugin's settings as the Plugins page stored them, `{}` or null when
there are none. The answer's `api` has to be one the host supports, or rox hangs up.

### source.browse and source.search

Browse with `node: null` asks for the roots:

```
→ {"jsonrpc":"2.0","id":2,"method":"source.browse","params":{"node":null,"cursor":null}}
← {"jsonrpc":"2.0","id":2,"result":{"entries":[{"node":{"id":"tones","title":"Tones","subtitle":"6 sine tones","collection":true}},{"node":{"id":"chords","title":"Chords","subtitle":"4 triads","collection":true}}],"cursor":null}}
```

An entry is `{"node": {id, title, subtitle, collection}}` or `{"track": Track}`. Node
ids are non-empty. `collection: true` marks a node the user can keep in the library. A
non-null `cursor` means there's another page, fetched by sending that cursor back:

```
→ {"jsonrpc":"2.0","id":3,"method":"source.browse","params":{"node":"tones","cursor":null}}
← {"jsonrpc":"2.0","id":3,"result":{"entries":[{"track":{"key":"tone:A3","title":"A3, 220 Hz","artist":"rox","album_artist":"rox","album":"Tones","genre":"Test Tone","year":0,"disc_no":1,"track_no":1,"duration_ms":10000,"codec":"PCM","bitrate_kbps":705,"live":false}}, ...],"cursor":"4"}}
```

A Track has `key`, `title`, `artist`, `album_artist`, `album`, `genre`, `year`,
`disc_no`, `track_no`, `duration_ms`, `codec`, `bitrate_kbps` and `live`. None of them
is ever null: unknown text is `""` and an unknown number is `0`. A missing field reads
as empty.

`key` is opaque to rox and non-empty. It becomes the track's path in the library, so it
has to stay the same across sessions and plugin versions. A plugin that changes how it
builds keys orphans every row it made.

Search answers in the same page shape, so it can return nodes as well as tracks:

```
→ {"jsonrpc":"2.0","id":4,"method":"source.search","params":{"query":"minor","cursor":null}}
← {"jsonrpc":"2.0","id":4,"result":{"entries":[{"track":{"key":"chord:A minor","title":"A minor", ...}}],"cursor":null}}
```

### source.sync

Pages through one collection the user keeps in the library:

```
→ {"jsonrpc":"2.0","id":5,"method":"source.sync","params":{"collection":"tones","token":null,"cursor":null}}
← {"jsonrpc":"2.0","id":5,"result":{"unchanged":false,"tracks":[ ...4 tracks... ],"cursor":"4","token":null}}
→ {"jsonrpc":"2.0","id":6,"method":"source.sync","params":{"collection":"tones","token":null,"cursor":"4"}}
← {"jsonrpc":"2.0","id":6,"result":{"unchanged":false,"tracks":[ ...2 tracks... ],"cursor":null,"token":"bbeea25a853c2c83"}}
```

The first page's request includes the token from the last complete sync, and only the
first page's does. Answering `"unchanged": true` there ends the sync with nothing
written, and rox keeps the token it has. The last page, the one with a null `cursor`,
holds the new token. rox replaces the collection's tracks only after the last page, so
a sync that fails partway changes nothing. A sync still paging after 2,000 pages is
treated as a plugin looping on its own cursor and fails.

Kept collections sync when the plugin starts and on Sync Now on the Plugins page.
Between syncs they browse from the library with no plugin call, so they still work
with the plugin stopped or the network down.

### source.open

```
→ {"jsonrpc":"2.0","id":7,"method":"source.open","params":{"key":"tone:A4"}}
← {"jsonrpc":"2.0","id":7,"result":{"stream":"s1","hint":"wav","length":882044,"seekable":true,"live":false}}
```

| Field      | Meaning                                                                                |
| ---------- | -------------------------------------------------------------------------------------- |
| `stream`   | An id the plugin chooses for this stream.                                               |
| `hint`     | A container extension for the probe, or `""` to let it sniff.                          |
| `length`   | The total byte count, or null when unknown.                                             |
| `seekable` | A read at any offset works.                                                             |
| `live`     | A stream with no end and no duration.                                                   |
| `buffer`   | Optional. `"ahead"` asks rox to read one chunk ahead instead of downloading the whole track. |

`buffer: "ahead"` is for a service that meters or throttles fast downloads. A plugin can
lower rox's buffering this way but never raise it.

### source.read

```
→ {"jsonrpc":"2.0","id":8,"method":"source.read","params":{"stream":"s1","offset":0,"len":262144}}
← {"jsonrpc":"2.0","id":8,"result":{"data":"UklGRnR1DQBXQVZFZm10IBAAAAABAAEARKwAAIhYAQACABAAZGF0YVB1DQAAAGcCzQQtB4cJ1wsaDlAQ..."}}
```

Up to `len` bytes from `offset`, base64. Fewer is fine, and an empty `data` means the
end of the stream. More than `len` is refused. rox asks for 256 KiB at a time and never
more than 512 KiB, so an answer stays under the 1 MiB line cap once encoded.

rox never sends overlapping ranges on one stream. A seekable stream gets its next chunk
requested while the last one is still in use. A stream that can't seek gets one read at
a time, each starting where the last answer ended. A plugin still has to accept a read
on one stream while a read on another is running.

A plugin that loses its upstream mid-stream should recover inside `source.read` and
answer an error only when that fails. See [Streams](#streams) for what rox does with
the error.

### source.close

```
→ {"jsonrpc":"2.0","id":9,"method":"source.close","params":{"stream":"s1"}}
← {"jsonrpc":"2.0","id":9,"result":null}
```

Sent when rox drops the stream. Nothing waits on the answer.

### source.cover

```
→ {"jsonrpc":"2.0","id":10,"method":"source.cover","params":{"key":"tone:A4"}}
← {"jsonrpc":"2.0","id":10,"result":null}
```

`{"mime": "..", "data": ".."}` with the image base64, or null for no cover. rox asks
only for tracks with no stored cover.

### shutdown

```
→ {"jsonrpc":"2.0","id":11,"method":"shutdown"}
← {"jsonrpc":"2.0","id":11,"result":null}
```

Then the plugin exits. It should also exit whenever stdin closes, since that's the one
signal that reaches a plugin on every OS when rox goes away without a shutdown.

## Streams

The plugin hands rox container bytes, and rox decodes them with the same decoder it
uses for files. It reads CAF, MP4, Matroska, Ogg, AIFF, WAV, and raw ADTS AAC and MP3.
There's no MPEG-TS reader, so a segmented upstream is the plugin's to join into one
continuous stream in one of those containers.

How rox reads depends on what `source.open` said:

- A seekable stream of known length up to 64 MB, without `buffer: "ahead"`, is
  downloaded whole while it plays, front to back. A seek past the download moves it
  there, and the skipped part fills in after. The seekbar shows what's downloaded.
- Anything else is read one 256 KiB chunk ahead of playback.

Once a track has played for 2 seconds, rox opens the next two entries in the queue
ahead of time when they're plugin tracks, so the next track doesn't wait on
`source.open`. A pre-opened stream nobody plays within 60 seconds is closed. Live
streams are never pre-opened.

When a read fails mid-track, rox closes the stream and opens it again, waiting 0, 1, 2
and 4 seconds before the attempts. The transport shows Reconnecting meanwhile. A
seekable stream of known length resumes at the byte it failed on, provided the reopened
stream has the same length. A different length is a different encode, and rox won't
splice two. A stream that can't seek or has no length can't resume and ends at once.
When the attempts run out, the track ends with an error naming the plugin, and the
queue moves on. A dead or slow plugin never stops local playback.

A live stream answers each read with what it has ready instead of waiting to fill `len`.
A 256 KiB read at 128 kbps takes 16 seconds to fill, past the read timeout. rox keeps
pulling a live stream while it's paused, so a pause resumes where it stopped and the
last minutes stay seekable. A pause longer than 30 minutes hangs up, and Play rejoins
at the live edge.

## Tracks in the library

A track enters the library when the user keeps a collection that holds it, or plays,
queues or adds it to a playlist on its own. Syncing a collection makes it hold exactly
the tracks the sync returned. A track no kept collection holds any more, and that the
user never picked on its own, leaves the library.

A plugin's tracks show only while it's switched on and its folder is present. A
switched-off plugin's tracks are hidden, and so are those of a plugin whose folder is
gone, since deleting the old folder is how many people update a plugin. They're
deleted only on Remove. Playlist entries and play history reattach when a track comes
back with the same key.

A plugin's tracks scrobble only when its manifest declares `"scrobble": true` and the
user leaves Scrobble Plays on for it. Approving a manifest that declares scrobbling
turns Scrobble Plays on, since the card just said so.

## Process lifecycle

A plugin gets one process, started by the first call that needs it. Switching a plugin
on syncs its kept collections, so it starts right away. The process runs in the
plugin's folder with rox's environment (PATH, HOME and the rest) plus
`PYTHONDONTWRITEBYTECODE=1`.

When the plugin exits unexpectedly, every call it had in flight fails, and the next
call starts it again after 0, 1, 2 or 4 seconds for the first through fourth crash
within ten minutes. The fifth stops it with "Stopped after repeated crashes" on its row
until the user switches it off and on. A plugin that can't start or fails `hello`
counts as a crash.

Switching a plugin off sends `shutdown`, waits up to 2 seconds for requests still in
flight, then closes stdin and kills what's left. Quitting rox skips the wait. A changed
config restarts the plugin.

On Linux and macOS the plugin leads its own process group, and stopping it kills the
whole group, so programs it started go with it. A child that moves itself to another
group escapes, as it would from a shell. On Windows the plugin starts with no console
window and goes into a job object, and stopping it ends everything in the job. A
grandchild started in the instant between the spawn and the job assignment can escape.

## Timeouts and caps

| What                                                 | Limit                          |
| ---------------------------------------------------- | ------------------------------ |
| `hello`                                              | 5 s                            |
| `source.browse`, `source.search`, later sync pages   | 15 s                           |
| A sync's first page                                  | 60 s                           |
| `source.open`                                        | 20 s                           |
| `source.read`, `source.cover`                        | 10 s                           |
| `shutdown`                                           | 2 s, then killed               |
| A line on stdout                                     | 1 MiB                          |
| A string in a result                                 | 4 KiB                          |
| Entries or tracks per page                           | 500                            |
| A read's `len`                                       | 256 KiB asked, 512 KiB at most |
| A stderr line                                        | 4 KiB                          |
| Sync pages per collection                            | 2,000                          |

A call that times out fails on its own, and the plugin keeps serving its other calls.
A longer stdout line is dropped with a warning in the log. A result over a cap fails
that call.

## What a plugin can't do

- Run code in rox's UI or draw anything.
- Process audio or hook the engine. Decoding stays in rox.
- Hand rox a URL to fetch. rox never fetches an address a plugin chose, and bytes make
  expiring, segmented or shifting URLs the plugin's problem.
- Call rox or connect to the [control socket](README_IPC.md). The socket authenticates
  by filesystem permission and can't tell a plugin from any other local program.
- Write into its own folder without switching itself off.
- Scrobble without declaring it.

## Trying a plugin without rox

A plugin is a program that reads lines on stdin, so a shell can drive it. From the
tones folder:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api":1,"config":{},"data_dir":"/tmp/tones","platform":"linux-x86_64"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"source.browse","params":{"node":null,"cursor":null}}' \
  | python3 -B tones.py
```

`-B` keeps Python from writing `__pycache__` into the folder, which would change its
hash.
