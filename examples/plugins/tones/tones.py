"""Tones: an example rox source plugin, written to be copied.

rox starts this file as a subprocess and speaks newline-delimited JSON-RPC 2.0
with it: one request per line on stdin, one answer per line on stdout. stderr
is free text that lands in rox's log. The protocol is written down in
README_PLUGINS.md in the rox repository.

The source makes its own audio, so it needs no network and no service. Every
track is a sine tone or a chord, generated as a 16-bit mono WAV the first time
it's opened, with a length known up front. Two habits here are worth keeping
in any plugin:

- Every request but shutdown runs on a thread pool, so a slow search never
  holds up the reads that keep a track playing.
- The plugin exits when stdin closes. That's the one signal that reaches it
  on every OS when rox goes away without saying goodbye.

Only the standard library is used, so there's nothing to install.
"""

import base64
import concurrent.futures
import functools
import hashlib
import json
import math
import struct
import sys
import threading

API = 1
RATE = 44100

# Pages are kept short on purpose, so browsing and syncing show their cursors.
PAGE = 4

# Whole hertz and whole seconds: every track ends on a whole cycle, so two
# tracks played back to back join without a click. A gap between them is
# silence you can hear.
TONES = [
    ("A3", [220]),
    ("C4", [262]),
    ("E4", [330]),
    ("A4", [440]),
    ("C5", [523]),
    ("E5", [659]),
]
TONE_SECS = 10

CHORDS = [
    ("C major", [262, 330, 392]),
    ("A minor", [220, 262, 330]),
    ("F major", [175, 220, 262]),
    ("G major", [196, 247, 294]),
]
CHORD_SECS = 15


def track(key, title, album, number, secs):
    # Every field is always sent. Unknown text is "", an unknown number is 0.
    return {
        "key": key,
        "title": title,
        "artist": "rox",
        "album_artist": "rox",
        "album": album,
        "genre": "Test Tone",
        "year": 0,
        "disc_no": 1,
        "track_no": number,
        "duration_ms": secs * 1000,
        "codec": "PCM",
        "bitrate_kbps": RATE * 16 // 1000,
        "live": False,
    }


# Keys become the rows' paths in rox's library, so they never change.
CATALOG = {
    "tones": {
        "title": "Tones",
        "subtitle": f"{len(TONES)} sine tones",
        "tracks": [
            (track(f"tone:{name}", f"{name}, {freqs[0]} Hz", "Tones", i + 1, TONE_SECS), freqs, TONE_SECS)
            for i, (name, freqs) in enumerate(TONES)
        ],
    },
    "chords": {
        "title": "Chords",
        "subtitle": f"{len(CHORDS)} triads",
        "tracks": [
            (track(f"chord:{name}", name, "Chords", i + 1, CHORD_SECS), freqs, CHORD_SECS)
            for i, (name, freqs) in enumerate(CHORDS)
        ],
    },
}

BY_KEY = {t["key"]: (t, freqs, secs) for node in CATALOG.values() for t, freqs, secs in node["tracks"]}


class Failure(Exception):
    """An error answer: a JSON-RPC code and a message rox shows or logs."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


state = {"volume": 30}
out_lock = threading.Lock()
streams = {}
streams_lock = threading.Lock()
stream_ids = iter(range(1, 1 << 62))


def send(frame):
    # Writes come from several threads, so each whole line goes out under a
    # lock. Bytes, so Windows doesn't turn "\n" into "\r\n".
    line = (json.dumps(frame, separators=(",", ":")) + "\n").encode("utf-8")
    with out_lock:
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()


def log(text):
    sys.stderr.write(text + "\n")
    sys.stderr.flush()


# A pre-open and the open that follows it, or a reopen after a failed read,
# ask for the same track; it's made once.
@functools.lru_cache(maxsize=4)
def wav(freqs, secs, volume):
    frames = RATE * secs
    peak = 32767 * volume / 100 / len(freqs)
    steps = [2 * math.pi * f / RATE for f in freqs]

    samples = bytearray(frames * 2)
    for i in range(frames):
        value = sum(math.sin(step * i) for step in steps)
        struct.pack_into("<h", samples, i * 2, int(value * peak))

    header = struct.pack(
        "<4sI4s4sIHHIIHH4sI",
        b"RIFF", 36 + len(samples), b"WAVE",
        b"fmt ", 16, 1, 1, RATE, RATE * 2, 2, 16,
        b"data", len(samples),
    )
    return header + bytes(samples)


def page_of(items, cursor):
    # A cursor is whatever the plugin likes; here it's an offset. rox only
    # hands it back.
    start = int(cursor or 0)
    if start < 0 or start > len(items):
        raise Failure(-32602, f"bad cursor {cursor!r}")

    end = start + PAGE
    return items[start:end], (str(end) if end < len(items) else None)


def node(node_id):
    return {"node": {
        "id": node_id,
        "title": CATALOG[node_id]["title"],
        "subtitle": CATALOG[node_id]["subtitle"],
        "collection": True,
    }}


def tracks_of(node_id):
    if node_id not in CATALOG:
        raise Failure(-32602, f"no node {node_id!r}")

    return [t for t, _, _ in CATALOG[node_id]["tracks"]]


def token_of(tracks):
    # Changes whenever the collection's contents would, so rox can skip a
    # sync that would write nothing.
    return hashlib.sha256("\n".join(t["key"] for t in tracks).encode()).hexdigest()[:16]


def hello(params):
    config = params.get("config") or {}
    volume = config.get("volume")
    if isinstance(volume, int) and 1 <= volume <= 100:
        state["volume"] = volume

    log(f"hello from rox, api {params.get('api')}, on {params.get('platform')}")
    return {"name": "Tones", "version": "0.1.0", "api": API}


def browse(params):
    if params.get("node") is None:
        roots = [node(node_id) for node_id in CATALOG]
        return {"entries": roots, "cursor": None}

    listed, cursor = page_of(tracks_of(params["node"]), params.get("cursor"))
    return {"entries": [{"track": t} for t in listed], "cursor": cursor}


def search(params):
    query = str(params.get("query", "")).strip().lower()
    found = [t for t, _, _ in BY_KEY.values() if query in t["title"].lower()]

    listed, cursor = page_of(found, params.get("cursor"))
    return {"entries": [{"track": t} for t in listed], "cursor": cursor}


def sync(params):
    tracks = tracks_of(params["collection"])
    token = token_of(tracks)

    # Only the first page carries the token rox kept from the last sync.
    if params.get("cursor") is None and params.get("token") == token:
        return {"unchanged": True, "tracks": [], "cursor": None, "token": None}

    listed, cursor = page_of(tracks, params.get("cursor"))
    return {
        "unchanged": False,
        "tracks": listed,
        "cursor": cursor,
        # Sent on the last page only: rox stores it once every page is in.
        "token": token if cursor is None else None,
    }


def open_stream(params):
    key = params["key"]
    if key not in BY_KEY:
        raise Failure(-32000, f"no track {key!r}")

    _, freqs, secs = BY_KEY[key]
    data = wav(tuple(freqs), secs, state["volume"])

    with streams_lock:
        stream = f"s{next(stream_ids)}"
        streams[stream] = data

    return {
        "stream": stream,
        "hint": "wav",
        "length": len(data),
        "seekable": True,
        "live": False,
    }


def read(params):
    with streams_lock:
        data = streams.get(params["stream"])
    if data is None:
        raise Failure(-32000, f"no open stream {params['stream']!r}")

    offset, count = int(params["offset"]), int(params["len"])
    chunk = data[offset:offset + count]

    # An empty answer is the end of the stream.
    return {"data": base64.b64encode(chunk).decode("ascii")}


def close(params):
    with streams_lock:
        streams.pop(params["stream"], None)
    return None


def cover(params):
    # No artwork: a null answer, not an error.
    return None


METHODS = {
    "hello": hello,
    "source.browse": browse,
    "source.search": search,
    "source.sync": sync,
    "source.open": open_stream,
    "source.read": read,
    "source.close": close,
    "source.cover": cover,
}


def answer(rid, method, params):
    handler = METHODS.get(method)
    try:
        if handler is None:
            raise Failure(-32601, f"method not found: {method}")
        send({"jsonrpc": "2.0", "id": rid, "result": handler(params)})
    except Failure as e:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": e.code, "message": str(e)}})
    except Exception as e:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": f"{type(e).__name__}: {e}"}})


def main():
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=4)

    # Ends when stdin closes, whether rox shut down cleanly or not.
    for raw in sys.stdin.buffer:
        if not raw.strip():
            continue

        try:
            frame = json.loads(raw)
        except ValueError as e:
            log(f"a line that isn't JSON: {e}")
            continue

        rid, method, params = frame.get("id"), frame.get("method"), frame.get("params") or {}

        if method == "shutdown":
            send({"jsonrpc": "2.0", "id": rid, "result": None})
            break

        pool.submit(answer, rid, method, params)

    pool.shutdown(wait=False, cancel_futures=True)


if __name__ == "__main__":
    main()
