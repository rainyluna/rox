# ADR 30: Plugins are subprocesses that bring something external in

**Status:** Decided 2026-09-28, on the numbers in [research 04](../../0R-research/04-plugin-host.md); amended below

Decision: a plugin is a folder the user drops into rox's data directory, holding a
manifest and an entry program. rox runs it as a subprocess and talks to it over its
stdin and stdout. A plugin exists to bring something from outside rox in, and it
declares what it does from a closed set of capabilities. A source files its rows under
`plugin:<id>`, and rox browses, searches, syncs, and streams them through the plugin.
Panels are listed under the plugin's name in Add Panel as presets of core panel kinds.
A plugin source hands rox audio bytes, never a URL. Nothing a plugin supplies executes
in the UI, touches the engine or the ring, or connects to the control socket. rox
publishes the contract, a service-neutral example plugin, and the host. It never
publishes a plugin for a particular service ([scope](../../01-product/03-scope.md)).

The prototype for #8 measured what this record could only estimate, and the numbers are
in [research 04](../../0R-research/04-plugin-host.md). They set the timeouts, the read
size, whole-track buffering and the pre-open, recorded at the end.

The field-level shapes (manifest keys, wire methods, timeouts, size caps) are pinned in
a contract kept with the implementation plans, outside this repository
(`plugins-contract.md`), until the host's implementation doc exists to hold them. What
follows describes them in prose, so this record stands without that file.

**Plugins, where ADR 29 said extensions.** The scope doc and ADR 29 called these
extensions and gave them one job, sources. A source is still the first capability, and a
source needs a panel to browse and search it from. The line this ADR draws covers both:
a plugin reaches outside rox and brings the result back. It never changes how rox itself
behaves. A visualizer is the clearest case of what's out, since rendering is something
rox does rather than something it fetches.

**The host runs a subprocess.** A plugin runs as its own process with the user's
permissions, its own network, and its own filesystem access. rox doesn't sandbox it,
and the enable card says so in those words.

WASM with a spawn capability the host grants was the other candidate, and ADR 29 leaned
toward it (29:86-93). It lost on three counts. Its sandbox had a hole in the middle: a
WASM plugin could only reach a downloader through the spawn capability. The spawned
program runs unsandboxed, so the plugin ended up as trusted as the program it asked for.
A service with an ordinary web API would have needed a fetch capability in rox too,
since a WASM plugin has no network of its own. And every author would pay a toolchain
step to build the artifact, which is ADR 24's objection to WASM for scripts (24:43-48),
for a sandbox that already leaked. Either way the user ends up trusting someone else's
program. A subprocess says so plainly, and writing one needs nothing beyond a JSON
library.

In-process native libraries, the shape foobar2000's components take, were the third
option. They give a plugin the most reach and tie it to the host the most tightly. Rust
has no stable ABI, so it would be a C ABI rebuilt for every OS and architecture. A crash
in any plugin would take the audio engine down with it. A subprocess keeps the reach
without the coupling. A plugin that crashes ends its own track, and the host restarts
it.

A plugin built on a downloader runs that program itself. The manifest lists the programs
a plugin needs, and the Plugins page checks they're on PATH and names any that are
missing. The list informs the user and grants nothing. A subprocess can run whatever it
likes, and a declared list that implied otherwise would be a promise rox can't keep.

**Platforms.** A plugin has to run on the machine it's dropped onto, so the manifest
names its entry per platform. The entry is either a native binary per OS and
architecture (`linux-x86_64`, `windows-x86_64`, `macos-aarch64`, and so on) or a script
with an interpreter. The host resolves the interpreter per OS, since a Windows install
often has `py` where Linux has `python3`. A plugin with no entry for the current
platform is disabled with that reason on its row.

The host covers the rest of the difference. A plugin spawned on Windows gets
`CREATE_NO_WINDOW`, which convert's spawn path already sets because a console program
otherwise opens a window (`rox/src/convert.rs:46-63`). On quit the host kills each
plugin's whole process tree, through a job object on Windows and a process group on
Linux and macOS. The plugin's side is one rule: exit when stdin closes. When rox
crashes, stdin closing is the one signal that reaches a plugin on every OS.

**Identity and trust.** A plugin's identity for trust is the SHA-256 of its whole
folder, manifest included: every file's relative path and contents, in sorted order.
Any change turns the plugin off until the user turns it on again, so an update that
adds a capability or turns on scrobbling can't keep an old approval. A plugin that wrote
into its own folder would change its own hash, so the handshake hands each plugin a
separate data directory to write in. Dependencies outside the folder, like packages
installed globally for an interpreter, aren't covered. An author who wants them covered
vendors them into the folder.

Approved hashes are kept in `session.json` beside the shader approvals
(`rox-core/src/settings.rs:627-631`), machine-local for the same reason: a copied
settings file mustn't bring someone else's trust decision with it.

The opt-in has two layers, the pattern the AI features already use. A settings switch
reveals the Plugins page, the way the AI switch reveals the MCP and ML Models pages
(`rox/src/settings/window.rs:1455-1460`). Each plugin then has its own switch, and
turning it on is the approving act. The enable card says the plugin runs as a program
with the user's permissions and lists its capabilities and programs in words. On
re-approval it shows what changed in the manifest since the last approval. There's no
second dialog behind the switch, for ADR 24's reason (24:86-90): one gate and one habit,
since a second "are you sure" trains the user to click through both.

The gate doesn't defend the machine against local software. Any program that can write
the plugins folder can write `session.json` too, and rox already runs an ffmpeg dropped
into its data folder with no gate (`rox/src/convert.rs:81-85`). The switch enforces the
rule the shader gate follows, that only a direct user action approves code to run
(`rox-panel-api/src/panel/shader.rs:266-271`). It also shows the user what a plugin
declared before any of it runs. Signing was one alternative, and it needs an authority
rox has chosen not to be. A prompt on every launch was another, and it trains the same
click-through.

Plugin config, secrets included, goes in `accounts.json`, in plaintext, for [ADR
14](14-adr-online-providers.md)'s reason (14:54-57).

**The capability set.** Two capabilities, each granted only if the manifest declares
it:

- `source`: rows under `plugin:<id>`, browsed, searched, synced, streamed, and given
  covers through the plugin.
- `panels`: presets of core panel kinds, listed under the plugin's name. Nothing
  executes.

Several things stay outside the set. UI code and node trees wait on [ADR
24](24-adr-script-panels.md), which is Proposed and has its own gate. A plugin that
returned a node tree would be a script panel with network reach, the fragility the scope
doc's refusal exists for. Audio processing and engine hooks would put plugin code on the
decode path the callback invariant protects ([ADR 19](19-adr-processing-chain.md)) or
into the timeline the engine alone owns ([ADR 16](16-adr-play-queue.md)). A plugin
hands the engine container bytes, and decode stays in core. Controlling playback
somewhere else, which the scope doc's Tapped and Remote tiers need, isn't in the set
either. It gets a capability when a real plugin needs one.

Verbs into rox are out for now because nothing needs one. Every source call is one rox
makes into the plugin, so plugins never call back. When one does, it goes through a
typed allowlist in front of the socket's route arms. `route` takes the method and
params as plain values (`rox/src/integrations/ipc.rs:108-118`), so an allowlist can go
in front of it without a plugin ever holding a socket connection.

Adding a capability is a product decision, per the scope doc, and an amendment here.

**The wire, and why plugins never touch the socket.** A plugin speaks newline-delimited
JSON-RPC 2.0, the shape the control socket uses (`rox-ipc/src/protocol.rs:1-3`), over
the stdin and stdout rox spawned it with. It's a separate wire, so the two can version
apart. The socket authenticates by filesystem permission ([ADR
22](22-adr-control-surface.md), 22:34-36), so it can't tell a plugin from `roxctl`.
Giving plugins a scope on it would mean bolting an identity story onto a surface built
without one. The pipe needs no identity story: rox spawned the process, so nothing else
can be on the other end.

The host calls a handshake (API version, the plugin's config, its data directory),
browse, search, sync, open, read, close, cover, and shutdown. The plugin calls nothing
back. Every inbound frame parses into a typed shape that rejects unknown fields, and a
frame that doesn't parse is a plugin error, never a panic. Strings, page sizes, and
frames are capped, frames at the socket's 1 MiB (`rox-ipc/src/server.rs:21-22`). Every
call has a timeout, which rox's existing spawn path doesn't have: convert's runner polls
for a cancel with no wall-clock limit (`rox/src/convert.rs:898-913`).

There's one process per enabled plugin, started on first use, with its stderr logged
under the plugin's id. A crash restarts it with backoff, and a plugin that keeps
crashing is disabled with the reason on its row. It's shut down on disable and on quit.

**Browsing, searching, and syncing.** A source plugin exposes a tree. Its root nodes are
whatever the service has for the user, such as liked tracks, saved albums, playlists,
and followed artists. Browse pages through a node's children, which are more nodes or
tracks, with a cursor. Search takes a query and returns the same kind of page, so a
search can turn up albums and playlists as well as tracks.

Every track has a key the plugin chooses. rox treats it as opaque, and it has to stay
stable across sessions and plugin versions, because it's the row's path. A plugin that
changes its key scheme orphans every row it made.

A node the plugin marks as a collection can be synced. The user switches sync on for
their liked tracks or one playlist, and rox pages through it and writes every track as
a row. A version token lets the plugin answer that nothing changed, since services
rate-limit a client that re-reads ten thousand likes on every launch. Synced rows are
local, so library search covers them without touching the network.

Browse and search in the plugin's panel are live and do touch the network. That's why
searching a plugin is its own surface and never part of the shared query ([ADR
15](15-adr-global-filter.md)): the scope doc's local-first constraint says search never
depends on the network. Tracks the user plays, queues, or adds to a playlist from the
panel become rows one pick at a time, since the queue and playlists are made of library
rows. How synced and picked rows are kept and pruned is ADR 29's second amendment.

**Audio arrives as bytes.** Opening a plugin track asks the plugin to open a stream. It
answers with a stream id, a container hint, the total length when it knows it, and
whether the stream can seek. rox then pulls bytes with reads at an offset and length,
and closes the stream when it's done. The engine decodes those bytes like a file, so
gapless, ReplayGain, and visualizers work unchanged. A seek is a read at a new offset.
A live stream reads front to back and can't seek. How the engine opens and pre-opens
these streams is ADR 29's first amendment.

Everything specific to the service stays inside the plugin: auth, URLs and when they
expire, CDNs, request headers, and joining a segmented stream into one continuous file.
The plugin knows its service and rox doesn't.

The alternative was a URL. The plugin would resolve a track to a URL and headers, and
rox would fetch it with its own HTTP source. That fails in four places. A streaming
service's URLs expire, sometimes while the track is queued and sometimes halfway through
it. A fresh URL for the same track can point at a different encode, and `HttpSource`
resumes a reconnect at the old byte offset without checking the new response's length
(`rox-playback/src/http.rs:706-722`), so the decoder would get the middle of a
different file. Many services serve segmented streams, and rox has no assembler for
them. And rox would fetch any URL a plugin named, with any headers it named, which
needs an address policy of its own. All four become the plugin's problem once the
plugin serves bytes.

A plugin that only has decoded samples can serve them in a container rox already
decodes, such as WAV, so the PCM contract ADR 29 deferred may never be needed. That's
inferred from the formats the engine reads. Nobody has tried it.

Reads go over the same pipe as every other call, base64 in the JSON result. Encoding
adds a third to the byte count, so a lossless stream at about 1 Mbit/s becomes about 170
KB/s on the pipe. That's arithmetic, and the prototype measures it. A second channel
with binary frames was the alternative. It saves the encoding and costs every author a
second connection and a binary framing to write, for a bandwidth problem the arithmetic
doesn't show. A read stays under the 1 MiB frame cap once encoded. The host reads ahead,
so the decoder rarely waits on a read. Requests have ids, so a plugin can answer reads
while a slow search is still running. A plugin that answers one call at a time stalls
its own playback behind its own searches, and the example plugin shows how to avoid
that.

**The manifest.** An id (a lowercase slug, fixed once), a display name, the author's
version, the API version it targets, the entry per platform, author metadata in
`WorkspaceMeta`'s shape minus the dates (`rox-core/src/settings.rs:2446-2468`), the
declared capabilities, the programs it needs, and a JSON Schema for the plugin's config.
The Plugins page renders a small subset of JSON Schema as rows: strings, secrets,
numbers, booleans, and enums. There are no per-locale strings, so a plugin's labels
show in its author's language.

Unknown top-level keys are rejected, and so are unknown keys inside the entry, since
that's how the plugin runs. Inside `meta` and `capabilities` unknown keys are ignored,
so a field added there later doesn't refuse a plugin on an older rox; a new top-level
key bumps the API version. That's new for rox: the workspace bundle reads leniently so
an old look still loads, and nothing in the workspace rejects an unknown field today. A
manifest is a different kind of file. A key the host doesn't know is a key it can't
enforce, and a plugin that relies on one should fail loudly.

The API version is an integer. The host supports a range of versions and accepts a
plugin that targets any version in it, so one rox release doesn't break every plugin at
once. Additive changes, such as a new optional field, don't bump it. The prototype runs
at 0. Version 1 is fixed after the prototype, once the wire has had a real plugin on the
other end.

**Rows under `plugin:<id>`.** A plugin's source id is `plugin:` plus its manifest id,
fixed once and kept in rows and layouts for good. ADR 29's identity rules apply
unchanged. What changes is how the services layer finds which non-local ids are live.
Today that's a `subsonic:` prefix check in three places: hiding rows whose source is
switched off (`rox-services/src/sources.rs:340-342`), sweeping rows whose account was
removed (`sources.rs:208-214`), and fetching covers (`sources.rs:582-600`). Under those
checks a removed plugin's rows would never be swept. One function that returns the live
ids, from Subsonic accounts and plugin records together, replaces all three.

A plugin whose folder disappears shows as missing on the Plugins page, and its rows are
hidden like a switched-off source's. Nothing is swept, because deleting the old folder
is how many people will update a plugin. The rows go only when the user removes the
plugin on the Plugins page.

A plugin source scrobbles only if its manifest declares it, and the user can still turn
that off. Nothing in the scrobblers checks where a row came from
(`rox-services/src/lastfm.rs`, `listenbrainz.rs` and `librefm.rs` have no source test),
so without an explicit default every plugin play would scrobble.

Capture is never available to plugin rows. It only tees streams that contain ICY
metadata: `rox-playback/src/http.rs:774-784` wraps a body only when the server announces
a metadata interval, and the tee is installed inside that wrapper (`icy.rs:105`). A
plugin stream never passes through `HttpSource`, so capture can't reach it today.
Recording the refusal here gives a later change to capture a rule to check against.

**Panels: a Plugins category in Add Panel.** Add Panel grows a Plugins section listing
each enabled plugin by name, and under each, the panels it declares. A declared panel is
a preset of a core panel kind: the same saved dump a panel preset already is, built by
the path presets already take (`rox/src/panel_presets.rs:49-74`). The manifest names a
core panel and its config. It can't name a kind the binary doesn't have, or hold a
container.

A plugin source's own panel is a new core panel kind, the source browser, pinned to one
source id. It shows the plugin's tree, a search box, and a sync switch on each
collection. The nearest existing panel is the station directory, which searches a
remote directory and writes the stations the user picks
(`rox/src/station_directory.rs:188-222`). The source browser is core code, and a plugin
supplies none of it.

The alternative kept plugin names off the menu: the same source browser, reached from
the ordinary catalog with a source picked in its config. The mechanism is the same.
Only the name on the entry differs. It's the cheaper option. The catalog is a static
list (`rox/src/panel_catalog.rs:632`) whose build step is a plain function pointer
(`panel_catalog.rs:67-76`), so a section built at runtime is new code. Plugin-supplied
labels also can't go through the two tests that hold every catalog label to a message
key (`panel_catalog.rs:695`, `:721`). It lost on product value. A plugin's panels under
its own name are how a user finds what the plugin added.

A plugin panel keeps its core `panel_name` and records its plugin in an optional owner
field on the chrome every panel config flattens in
(`rox-panel-api/src/panel.rs:982-986`). The other shape, a panel name registered per
plugin, breaks the placeholder a layout gets for a missing panel. The dock builds its
invalid-panel stand-in only for a name nothing registered
(`rox-dock/src/panel.rs:414-436`), so a registered plugin name would need a placeholder
of its own. With the owner field, a layout that outlives its plugin restores the core
panel with the plugin's rows hidden, and an older build reads the config as it always
did.

A workspace bundle that contains plugin panels lists the plugins it needs in a
`requires` field, derived from those owner fields. The apply card names any that are
missing. The field doesn't exist yet (`WorkspaceBundle`,
`rox-core/src/settings.rs:2350-2352`) and arrives with the first plugin panel. An older
build drops it silently, which costs that build only the warning.

This narrows the scope doc's refusal of scripted UI extensions for presets only: a
plugin can put entries on the menu, and every entry is data. ADR 24's script panels stay
Proposed and separate, and plugin-supplied node trees wait on them.

**What the #8 prototype measured.** The prototype ran one external plugin, built on a
downloader the user installs, through the real folder, manifest, version check and wire,
never linking rox's crates. ADR 29 is the reason (29:78-81). What it found settles the
open numbers:

- A cold open took 2.3 to 3.8 s when the plugin ran its downloader, a median of 2.7 s
  over twenty. The open timeout stays at 20 s. Starting a plugin and its handshake took
  under 0.8 s, so the handshake's 5 s stays.
- A cold open does stall the engine. A pause pressed during one waits out the rest of
  it, measured at up to 2.3 s, because commands drain at the top of the decode loop.
  Pre-opening the next two entries answered every sequential advance in the test from a
  stream already open, in under 2 ms, so the stall is confined to jumps and the first
  play, and the transport, the waveform and the track's row show that wait while it
  lasts. The pre-open waits for the audible track to hold for 2 s, so skipping through a
  queue doesn't pay for opens it throws away.
- The pipe is not a bandwidth problem. Base64 in JSON carried 14 Mbit/s at the worst
  read size through the real plugin and 130 to 148 Mbit/s through a plugin with no
  network behind it, and decoding a full read cost the host 0.3 ms. A second binary
  channel stays unneeded.
- Reads are 256 KiB. That got nearly all of 512 KiB's throughput through the real plugin
  and four times 64 KiB's, and a seek cost the same one round trip, a median of 68 to 87
  ms, at every size.
- A seekable stream up to 64 MB is downloaded whole while it plays, rather than read a
  chunk ahead, and so is a server's file answered by range. The seekbar shows what's
  downloaded, a seek inside it touches nothing but memory, and the waveform is decoded
  from the same bytes, so the track is fetched once. Plugin tracks of 3.5 to 5.7 MB
  downloaded in under a second. Anything larger, unseekable or live reads as it plays,
  and a plugin may ask for that too, for a service that meters or throttles fast
  downloads: it can lower the buffering, never raise the cap. A pre-opened stream
  downloads only once it plays.
- A sync's first page may take 60 s, since a plugin may list a whole collection there to
  learn whether it changed. Every other listing page keeps 15 s.
- A plugin in an interpreted language cost about a thousand lines of standard-library
  Python and a test suite of about eight hundred, on macOS and Linux. The per-step time
  wasn't recorded.

The first page's 60 s is a margin, not a measurement: no collection of thousands was
synced.

**The contract for the implementing layer.** rox-library gains a plugin origin beside
Subsonic's (`rox-library/src/cue.rs:68`), a third `Locator` variant for a plugin
stream, the membership table, and the source in playlist member snapshots. rox-playback
opens a plugin stream through an opener passed in with the queue, per ADR 29's
amendment. rox-services gains the one live-id function, collection sync, the pre-open,
and a plugin module that installs each enabled plugin's opener. rox-core gains plugin
records in `accounts.json` (the id, the switch, a cached label, the folder hash and
manifest at last approval, the scrobble choice, the synced collections, and config),
the Plugins switch in `settings.json`, the approved hashes in `session.json`, and a
plugins folder and per-plugin data folders under the data directory. rox doesn't create
those until the user asks, as with MilkDrop's (`rox-core/src/settings.rs:182-186`). A
new host crate owns the manifest, the wire, the process lifecycle, and the per-OS spawn.
The settings window gets a Plugins page behind the switch, Add Panel gets the Plugins
section and the source browser, panel chrome gets the owner field, and the bundle gets
`requires`. All of it stays behind the experimental gate until the opt-in ships.

**Amended 2026-09-29: one switch opts in, and an author can approve their own saves.**
The switch that lets plugins run moves to the head of the Plugins page, and the page is
always listed. It replaces the two layers above: Experimental Panels on one page, then
Enable Plugins on another, before the Plugins page appeared at all, made plugins hard to
find. The experimental gate is gone with it. The Plugins switch is the opt-in the last
paragraph was waiting on, and it's off by default.

Every save an author makes changes the folder's hash, which switched their plugin off
and put the card in front of them each time. Developer mode, a per-plugin toggle beside a
switched-on plugin's switch, approves those changes on its own for the rest of the session, as
long as the manifest's diff against the last approval is empty. A diff that isn't empty
still goes to the card, so a plugin can't gain a capability, a program, scrobbling or a
new entry without the user seeing it. The toggle is never saved, so a launch approves
nothing by itself. This narrows "any change turns the plugin off" and keeps the shader
gate's rule: turning the toggle on is the direct user action, and it approves the saves
that follow it. There's still no second dialog.

A browse or search page can carry a notice: a line of the plugin's own text, marked
`info` or `setup`, which rox shows above the page with a way to the plugin's settings for
`setup`. Before it, a plugin with nothing to list until the user set something up could
only answer an empty page or an error, and neither says what to do. It joins API 1
rather than starting API 2, since a plugin that doesn't send one is unchanged. A host
from before it refuses a page that carries one, so `hello` now lists the optional
features the host reads, and a plugin sends a notice only when `notice` is among them. When the source itself
can't answer, rox says why in its own words (plugins off, the plugin switched off,
changed on disk, gone, failing to load, stopped after crashes) and offers the Plugins
page, rather than passing on the host's internal error.
