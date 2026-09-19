# ADR 29: Sources behind one trait, in-process first, host deferred

**Status:** Proposed

Proposal: a source is a library provider plus a playback provider, the pair the
[scope doc](../../01-product/03-scope.md) already names. The library provider hands over
rows: tracks with their tags, artwork references, playlists, everything browse and search
need to work the way they do for local files. The playback provider answers one question,
where a track's bytes come from, and it answers with a playable reference: a URL, the
headers that authorize it, and a container hint, since a URL carries no extension to
probe off. Transport and decode stay in core. A source never opens an audio device,
never touches the ring, and never runs on the decode thread.

A PCM contract exists on paper for the case a reference can't express. librespot
decrypts and decodes Spotify's stream inside the extension, so what comes back is
samples plus a format description rather than a container rox can hand to symphonia.
Designing that half now means designing against one imagined implementor, so it waits
until a source forces it. When it lands it has to meet the single-stream engine from
[ADR 3](03-adr-gapless.md) rather than sit beside it as a second playback path.

**Sources start in-process.** The first two are Rust implementations of the trait
compiled into the binary, the way the enrichment providers already work.
[ADR 14](14-adr-online-providers.md) made this argument for its own domain and it
transfers whole: "First-party HTTP fetchers written by us don't need a sandbox, so making
them wait on one would be paying for isolation nobody asked for. If providers do
eventually ship as extensions, the per-domain trait is the surface the host would expose
anyway, so nothing here is wasted." The trait gets written either way. Writing it against
two implementations that run is how it comes out the right shape, and it's the only way
to find out which calls actually cross it.

**Identity is source-qualified in memory as well as on disk.** Half of that already
exists. The `tracks` table has a `source` column defaulting to `'local'`, the unique key
is `(source, path, sub)` after the subsong migration, and every local write path in
`rox-library/src/store.rs` scopes itself with `source = 'local'` rather than assuming
it. The other half doesn't. The projection loader, `scan_range`, selects every column
but that one, so the in-memory catalog the entire UI reads off can't tell two sources
apart. And `TrackKey` in `rox-library/src/cue.rs` is `{ path, sub }`, with nowhere to
put a source, so the currency the player and panels trade in is ambiguous the moment a
second source has a row whose path collides.

Closing that gap is the part of this decision that gets expensive if it slips. Adding
the column to the projection and the field to `TrackKey` touches browse views and queue
entries once, mechanically, while every row in every library is local. Doing it after a
source ships turns it into a data migration plus an audit of every call site that
quietly assumed a path was unique, which is the retrofit the scope doc's "don't paint
sources into a corner" constraint was written to avoid.

**Two first sources: Subsonic, then radio.** Subsonic and its OpenSubsonic extensions
are the library-provider case. A real catalog with browse, search, artwork and
playlists, over a documented API, against a server the user runs. That last part is what
earns it the first slot: when it breaks, it broke because our client is wrong, not
because a company changed something overnight. The fragility that put Spotify and
YouTube behind extensions in the first place doesn't apply to a server the user
administers.

Web radio is the transport case. No catalog, an unbounded stream, metadata in band.
Between them the two exercise both halves of the contract, which one source alone can't.

The alternatives were building the host first, and making radio the first example
extension. Building the host first means designing a boundary with no implementor, and a
boundary with no implementor is wrong in ways nobody finds out about until something has
to live inside it. Radio first looks cheap because it's small. It's also the least
representative part of the surface: an unbounded stream with in-band metadata is the
hardest case in the transport half, and it proves nothing at all about browse, search,
or identity, which is where the retrofit cost actually sits.

**The host mechanism stays open.** It gets its own ADR, decided on what these two
sources show: which calls crossed the trait and how often, how big the payloads were,
whether anything needed audio bytes rather than a reference to them.
[ADR 24](24-adr-script-panels.md) says "WASM stays the right answer for the source and
playback extension host, which is a different problem with different constraints". This
narrows that line rather than contradicting it. WASM stays the likely answer for the
host once there is a host. What's added here is that the host is not what ships first,
and the trait it would expose gets written and used before the mechanism is chosen.

**The HTTP transport lives in `rox-playback`, as a stated exception.** The layering says
all wire calls go in `rox-net`, blocking, on the background executor, and `rox-playback`
has no HTTP client today. This puts one there: a `MediaSource` over HTTP that turns a
seek into a ranged GET, plus the ICY metadata stripper radio needs to keep in-band
titles out of the decoder.

It goes there because it isn't a wire call in the sense that rule is about. It's a byte
transport the decode loop pulls from synchronously, so it can't run on the background
executor at all, and pretending otherwise would put a channel hop in the middle of the
decode path. `rox-net` also can't host it as things stand. Doing so means either a
dependency on `rox-playback`, when today it depends on `rox-core` alone, or re-exporting
a symphonia trait it has no other reason to know about.

The alternative, written down so flipping it stays cheap: the reader moves to a
`rox-net::stream` module, `rox-playback` gains a dependency on `rox-net`, and only the
`MediaSource` impl stays behind. That's a file move and one Cargo.toml line. The types
on either side of the seam are the same in both arrangements.

**Both sources are Full tier, and radio shows the tier model is incomplete.** The scope
doc grades sources Full, Tapped, Remote by capability, where Full means the source
provides decodable audio and therefore gets gapless, ReplayGain, and visualizers. Radio
is Full by that definition: it plays through the engine and the visualizers work.
Gapless and ReplayGain have nothing to act on, because there's no next track to prepare
and no per-track measurement to apply. The tier grades what the source hands over, not
what the content supports, and for a live stream those come apart. That's an amendment
to what Full means, not a fourth tier. A "Full, live" tier would have one member and no
second axis behind it.
