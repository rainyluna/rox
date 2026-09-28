//! The plugin host's services face: browsing, searching, syncing and picking
//! through a `plugin:<id>` source, the covers its rows show, and the stream
//! opener the engine plays them through (ADR 30). Plugins run as subprocesses
//! behind `rox-plugins`; every call into one runs on the background executor,
//! never the UI thread.
//!
//! One host per enabled plugin record whose folder loads and whose manifest
//! declares a source. Approval, the folder watch and the Plugins page are
//! the production host's, not this one's: records are read once at start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use gpui::{App, Entity, Task};
use serde_json::{Value, json};

use rox_core::settings::{self, PluginRecord, Settings, SyncedCollection};
use rox_library::cue::{PLUGIN_PREFIX, TrackKey};
use rox_library::locator::{Locator, PluginStream};
use rox_library::members::{self, PluginTrack};
use rox_library::store;
use rox_playback::plugin::{Opened, ReadAt};
use rox_plugins::{Host, HostConfig, Options, Status, Stream, hash, manifest, wire};

use crate::catalog::Library;
use crate::player::Player;

const NO_HOST: &str = "no plugin host";

/// A pre-opened stream nobody takes in this long is closed.
const PREOPEN_KEEP: Duration = Duration::from_secs(60);

/// How far ahead of the audible track streams are opened.
const PREOPEN_AHEAD: usize = 2;

/// How long a track has to hold before the ones after it are opened.
const PREOPEN_SETTLE: Duration = Duration::from_secs(2);

/// A sync past this many pages is a plugin looping on its own cursor.
const MAX_SYNC_PAGES: usize = 2000;

/// One page of a browse or search. A `None` cursor is the last page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A folder-like node; `collection` marks one that can be synced.
    Node {
        id: String,
        title: String,
        subtitle: String,
        collection: bool,
    },
    Track(PluginTrack),
}

struct Loaded {
    host: Host,
    label: String,
}

/// Source id to its host. Written once at start.
static HOSTS: LazyLock<RwLock<HashMap<String, Arc<Loaded>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn loaded(source: &str) -> Option<Arc<Loaded>> {
    HOSTS.read().ok()?.get(source).cloned()
}

fn host_for(source: &str) -> Result<Host, String> {
    loaded(source)
        .map(|loaded| loaded.host.clone())
        .ok_or_else(|| NO_HOST.to_string())
}

fn record_id(source: &str) -> Option<&str> {
    source.strip_prefix(PLUGIN_PREFIX)
}

/// Loads every enabled plugin, installs the stream opener, syncs each once,
/// and follows `player` to open upcoming plugin tracks early. Once per app;
/// a later window's call is a no-op. Needs experimental features and
/// `plugins_enabled` in settings.
pub fn start(library: Entity<Library>, player: Entity<Player>, cx: &mut App) {
    static STARTED: AtomicBool = AtomicBool::new(false);

    // Read off the file: nothing seeds the live switch until the Plugins page
    // exists.
    if !settings::experimental() || !Settings::load().plugins_enabled {
        return;
    }
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let records = Settings::load().accounts.plugins;
    let hosts: HashMap<String, Arc<Loaded>> = records
        .iter()
        .filter(|record| record.enabled)
        .filter_map(load)
        .map(|(source, loaded)| (source, Arc::new(loaded)))
        .collect();

    if hosts.is_empty() {
        return;
    }

    let sources: Vec<String> = hosts.keys().cloned().collect();
    if let Ok(mut table) = HOSTS.write() {
        *table = hosts;
    }

    crate::openers::install(Arc::new(open));

    for source in sources {
        let sync = sync_now(library.clone(), &source, cx);
        cx.spawn(async move |_| {
            if let Err(e) = sync.await {
                log::warn!("{source}: the first sync failed: {e}");
            }
        })
        .detach();
    }

    follow(player, cx);

    cx.on_app_quit(|cx| {
        let hosts: Vec<Host> = HOSTS
            .read()
            .map(|table| table.values().map(|loaded| loaded.host.clone()).collect())
            .unwrap_or_default();

        cx.background_executor().spawn(async move {
            for host in hosts {
                host.hang_up_now("rox quit");
            }
        })
    })
    .detach();
}

/// None, with the reason logged, when the folder can't run.
fn load(record: &PluginRecord) -> Option<(String, Loaded)> {
    let source = format!("{PLUGIN_PREFIX}{}", record.id);
    let dir = settings::data_dir().join("plugins").join(&record.id);

    let refuse = |why: String| {
        log::warn!("{source}: not loaded: {why}");
        None
    };

    let manifest = match manifest::load(&dir) {
        Ok(manifest) => manifest,
        Err(e) => return refuse(e),
    };
    if manifest.id != record.id {
        return refuse(format!("the folder holds plugin {:?}", manifest.id));
    }
    let Some(cap) = manifest.capabilities.source.clone() else {
        return refuse("it declares no source".into());
    };

    // No approval check yet: the production host compares this to the
    // approved hash. Logged so a prototype run records what ran.
    match hash::folder_hash(&dir) {
        Ok(hash) => log::info!("{source}: folder hash {hash}"),
        Err(e) => return refuse(e),
    }

    let data_dir = settings::data_dir().join("plugin-data").join(&record.id);
    let config = HostConfig::new(dir, manifest, record.config.clone(), data_dir);
    let label = match record.label.is_empty() {
        true => cap.label,
        false => record.label.clone(),
    };

    Some((
        source,
        Loaded {
            host: Host::new(config),
            label,
        },
    ))
}

/// The engine's end of a plugin stream.
struct Reader(Stream);

impl ReadAt for Reader {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        self.0.read_at(offset, len)
    }
}

/// How the engine's opens were answered this run, for the prototype's
/// measurements.
static OPENED_WARM: AtomicU64 = AtomicU64::new(0);
static OPENED_JOINED: AtomicU64 = AtomicU64::new(0);
static OPENED_COLD: AtomicU64 = AtomicU64::new(0);

/// What the engine calls, on its decode thread, for every plugin entry.
fn open(stream: &PluginStream) -> Result<Opened, String> {
    let host = host_for(&stream.source)?;
    let began = Instant::now();

    let (opened, how) = match take_preopened(&stream.source, &stream.key, host.timeouts().open) {
        Some((opened, joined)) => {
            let counter = if joined { &OPENED_JOINED } else { &OPENED_WARM };
            counter.fetch_add(1, Ordering::Relaxed);
            (
                opened,
                if joined {
                    "joined a pre-open"
                } else {
                    "pre-opened"
                },
            )
        }
        None => {
            OPENED_COLD.fetch_add(1, Ordering::Relaxed);
            (
                Stream::open(&host, &stream.key, Options::default())?,
                "cold",
            )
        }
    };

    log::info!(
        "{}: open {} {how} in {:.0} ms (pre-opened {}, joined {}, cold {} this run)",
        stream.source,
        stream.key,
        began.elapsed().as_secs_f64() * 1000.0,
        OPENED_WARM.load(Ordering::Relaxed),
        OPENED_JOINED.load(Ordering::Relaxed),
        OPENED_COLD.load(Ordering::Relaxed),
    );

    Ok(Opened {
        hint: opened.hint.clone(),
        length: opened.length,
        seekable: opened.seekable,
        buffer_whole: opened.buffer_whole,
        reader: Box::new(Reader(opened)),
    })
}

/// A pre-open's result, filled once, waited on by an engine open that
/// arrives while it's still under way.
type Slot = Arc<(Mutex<Option<Result<Stream, String>>>, Condvar)>;

struct Preopen {
    since: Instant,
    slot: Slot,
}

static PREOPENED: LazyLock<Mutex<HashMap<(String, String), Preopen>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The stream, and whether it was still opening when asked for.
fn take_preopened(source: &str, key: &str, wait: Duration) -> Option<(Stream, bool)> {
    let pending = PREOPENED
        .lock()
        .ok()?
        .remove(&(source.to_string(), key.to_string()))?;

    let (lock, ready) = &*pending.slot;
    let guard = lock.lock().ok()?;
    let joined = guard.is_none();
    let (mut guard, _) = ready
        .wait_timeout_while(guard, wait, |result| result.is_none())
        .ok()?;

    match guard.take()? {
        // Opened on a process that has since exited: open it fresh.
        Ok(stream) if stream.alive() => Some((stream, joined)),
        Ok(_) => None,
        Err(e) => {
            log::info!("{source}: the pre-open of {key} failed: {e}");
            None
        }
    }
}

fn preopen(stream: PluginStream, cx: &App) {
    let Ok(host) = host_for(&stream.source) else {
        return;
    };

    let slot: Slot = Arc::new((Mutex::new(None), Condvar::new()));
    {
        let Ok(mut table) = PREOPENED.lock() else {
            return;
        };
        let id = (stream.source.clone(), stream.key.clone());
        if table.contains_key(&id) {
            return;
        }
        table.insert(
            id,
            Preopen {
                since: Instant::now(),
                slot: Arc::clone(&slot),
            },
        );
    }

    cx.background_executor()
        .spawn(async move {
            let began = Instant::now();
            let result = Stream::open(&host, &stream.key, Options::default());
            log::debug!(
                "{}: pre-opened {} in {:.0} ms",
                stream.source,
                stream.key,
                began.elapsed().as_secs_f64() * 1000.0
            );

            let (lock, ready) = &*slot;
            if let Ok(mut filled) = lock.lock() {
                *filled = Some(result);
            }
            ready.notify_all();
        })
        .detach();
}

/// Drops pre-opens nobody took. One still opening goes when it finishes and
/// its last handle drops.
fn sweep() {
    if let Ok(mut table) = PREOPENED.lock() {
        table.retain(|_, preopen| preopen.since.elapsed() < PREOPEN_KEEP);
    }
}

/// Once the audible track has held for [`PREOPEN_SETTLE`], open the next
/// plugin tracks before the engine asks for them. Skipping through faster
/// than that opens nothing: every open is the plugin's work and a request to
/// its service. Live streams aren't opened early either, since that would
/// start a broadcast nobody hears yet.
fn follow(player: Entity<Player>, cx: &mut App) {
    let mut audible: Option<usize> = None;
    // Replacing it drops, and so cancels, the wait for the track before.
    let mut settling: Option<Task<()>> = None;

    cx.observe(&player, move |player, cx| {
        let now = player.read(cx).now_playing().map(|now| now.audible_idx);
        if now == audible {
            return;
        }
        audible = now;
        sweep();

        let wait = cx.spawn(async move |cx| {
            cx.background_executor().timer(PREOPEN_SETTLE).await;

            let upcoming = player.read_with(cx, |player, _| {
                let held = player.now_playing().map(|now| now.audible_idx) == now;
                held.then(|| player.upcoming_locators(PREOPEN_AHEAD))
            });
            let Ok(Some(upcoming)) = upcoming else {
                return;
            };

            cx.update(|cx| {
                for locator in upcoming {
                    if let Locator::Plugin(stream) = locator
                        && !stream.live
                    {
                        preopen(stream, cx);
                    }
                }
            })
            .ok();
        });
        drop(settling.replace(wait));
    })
    .detach();

    // A paused player notifies nothing, so the sweep has its own clock.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PREOPEN_KEEP / 2).await;
            sweep();
        }
    })
    .detach();
}

fn track(wire: wire::Track) -> PluginTrack {
    PluginTrack {
        key: wire.key,
        title: wire.title,
        artist: wire.artist,
        album_artist: wire.album_artist,
        album: wire.album,
        genre: wire.genre,
        year: wire.year,
        disc_no: wire.disc_no,
        track_no: wire.track_no,
        duration_ms: wire.duration_ms,
        codec: wire.codec,
        bitrate_kbps: wire.bitrate_kbps,
        live: wire.live,
    }
}

fn page(wire: wire::Page) -> Page {
    let entries = wire
        .entries
        .into_iter()
        .map(|entry| match entry {
            wire::Entry::Node(node) => Entry::Node {
                id: node.id,
                title: node.title,
                subtitle: node.subtitle,
                collection: node.collection,
            },
            wire::Entry::Track(t) => Entry::Track(track(t)),
        })
        .collect();

    Page {
        entries,
        cursor: wire.cursor,
    }
}

fn listing(
    source: &str,
    method: &'static str,
    params: Value,
    cx: &App,
) -> Task<Result<Page, String>> {
    let host = match host_for(source) {
        Ok(host) => host,
        Err(e) => return Task::ready(Err(e)),
    };

    cx.background_executor().spawn(async move {
        let timeout = host.timeouts().listing;
        host.call(method, params, timeout)
            .and_then(wire::decode::<wire::Page>)
            .map(page)
    })
}

/// `node: None` asks for the roots.
pub fn browse(
    source: &str,
    node: Option<String>,
    cursor: Option<String>,
    cx: &App,
) -> Task<Result<Page, String>> {
    listing(
        source,
        "source.browse",
        json!({ "node": node, "cursor": cursor }),
        cx,
    )
}

pub fn search(
    source: &str,
    query: String,
    cursor: Option<String>,
    cx: &App,
) -> Task<Result<Page, String>> {
    listing(
        source,
        "source.search",
        json!({ "query": query, "cursor": cursor }),
        cx,
    )
}

/// Every page of one collection. None when the plugin says nothing changed
/// since `token`; the stored token then stands, whatever the answer carried.
fn fetch_collection(
    host: &Host,
    collection: &str,
    token: &str,
) -> Result<Option<(Vec<PluginTrack>, String)>, String> {
    let timeouts = host.timeouts();
    let mut tracks = Vec::new();
    let mut cursor: Option<String> = None;

    for page in 0..MAX_SYNC_PAGES {
        let first = page == 0;
        let timeout = match first {
            true => timeouts.sync_first,
            false => timeouts.listing,
        };

        // Only the first page carries the stored token; the rest carry the cursor.
        let sent = (first && !token.is_empty()).then_some(token);
        let params = json!({ "collection": collection, "token": sent, "cursor": cursor });
        let answer: wire::SyncPage = wire::decode(host.call("source.sync", params, timeout)?)?;

        if first && answer.unchanged {
            return Ok(None);
        }

        tracks.extend(answer.tracks.into_iter().map(track));

        match answer.cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(Some((tracks, answer.token.unwrap_or_default()))),
        }
    }

    Err(format!(
        "{collection} is still paging after {MAX_SYNC_PAGES} pages"
    ))
}

/// Sync one collection into the library: the whole membership in one write,
/// only after the last page. Answers the rows written, zero when unchanged.
fn sync_collection(
    host: &Host,
    db_path: &Path,
    source: &str,
    synced: &SyncedCollection,
) -> Result<usize, String> {
    let began = Instant::now();
    let Some((tracks, token)) = fetch_collection(host, &synced.id, &synced.token)? else {
        log::info!("{source}: {} unchanged", synced.id);
        return Ok(0);
    };

    let mut conn = store::open(db_path).map_err(|e| e.to_string())?;
    members::set_collection(&mut conn, source, &synced.id, &tracks).map_err(|e| e.to_string())?;

    log::info!(
        "{source}: synced {} ({} tracks) in {:.1} s",
        synced.id,
        tracks.len(),
        began.elapsed().as_secs_f64()
    );

    let (id, collection) = (
        record_id(source).unwrap_or_default().to_string(),
        synced.id.clone(),
    );
    Settings::update(move |s| {
        let found = s
            .accounts
            .plugins
            .iter_mut()
            .find(|record| record.id == id)
            .and_then(|record| record.synced.iter_mut().find(|c| c.id == collection));
        if let Some(stored) = found {
            stored.token = token;
        }
    });

    Ok(tracks.len())
}

fn synced_of(source: &str) -> Vec<SyncedCollection> {
    let Some(id) = record_id(source) else {
        return Vec::new();
    };

    Settings::load()
        .accounts
        .plugins
        .into_iter()
        .find(|record| record.id == id)
        .map(|record| record.synced)
        .unwrap_or_default()
}

/// Runs `work` on the background executor with the library's database path,
/// then reloads the projection whatever it answered.
fn write<T: Send + 'static>(
    library: Entity<Library>,
    cx: &mut App,
    work: impl FnOnce(PathBuf) -> Result<T, String> + Send + 'static,
) -> Task<Result<T, String>> {
    let db_path = library.read(cx).db_path();

    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { work(db_path) })
            .await;

        library
            .update(cx, |library, cx| library.reload_projection(cx))
            .ok();

        result
    })
}

/// Turns a collection's sync on or off. Answers the rows it now holds.
pub fn set_synced(
    library: Entity<Library>,
    source: &str,
    node: &str,
    title: &str,
    on: bool,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let (host, id) = match (host_for(source), record_id(source)) {
        (Ok(host), Some(id)) => (host, id.to_string()),
        (Err(e), _) => return Task::ready(Err(e)),
        (_, None) => return Task::ready(Err(NO_HOST.to_string())),
    };

    let collection = SyncedCollection {
        id: node.to_string(),
        title: title.to_string(),
        token: String::new(),
    };
    let stored = collection.clone();
    Settings::update(move |s| {
        let Some(record) = s.accounts.plugins.iter_mut().find(|r| r.id == id) else {
            return;
        };

        record.synced.retain(|c| c.id != stored.id);
        if on {
            record.synced.push(stored);
        }
    });

    let source = source.to_string();
    write(library, cx, move |db_path| match on {
        true => sync_collection(&host, &db_path, &source, &collection),

        false => {
            let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
            members::drop_collection(&mut conn, &source, &collection.id)
                .map(|_| 0)
                .map_err(|e| e.to_string())
        }
    })
}

/// Syncs every collection the source has switched on. Answers the rows
/// written.
pub fn sync_now(
    library: Entity<Library>,
    source: &str,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let host = match host_for(source) {
        Ok(host) => host,
        Err(e) => return Task::ready(Err(e)),
    };

    let source = source.to_string();
    let collections = synced_of(&source);

    write(library, cx, move |db_path| {
        // Starting the plugin is part of the first sync, and the one
        // failure worth stopping on.
        host.ensure()?;

        let mut written = 0;
        for collection in &collections {
            match sync_collection(&host, &db_path, &source, collection) {
                Ok(rows) => written += rows,
                Err(e) => log::warn!("{source}: syncing {} failed: {e}", collection.id),
            }
        }

        Ok(written)
    })
}

/// Adds single tracks to the library and answers their keys, for a caller
/// that wants to queue them straight away.
pub fn pick(
    library: Entity<Library>,
    source: &str,
    tracks: Vec<PluginTrack>,
    cx: &mut App,
) -> Task<Result<Vec<TrackKey>, String>> {
    if loaded(source).is_none() {
        return Task::ready(Err(NO_HOST.to_string()));
    }

    let source = source.to_string();
    write(library, cx, move |db_path| {
        let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
        members::pick(&mut conn, &source, &tracks).map_err(|e| e.to_string())
    })
}

/// Blocking; called from `fetch_cover`'s thread.
pub fn cover(source: &str, key: &str) -> Option<Vec<u8>> {
    let host = host_for(source).ok()?;
    let answer = host
        .call("source.cover", json!({ "key": key }), host.timeouts().cover)
        .inspect_err(|e| log::debug!("{source}: cover for {key}: {e}"))
        .ok()?;

    if answer.is_null() {
        return None;
    }

    let cover: wire::Cover = wire::decode(answer).ok()?;
    cover.bytes().ok().filter(|bytes| !bytes.is_empty())
}

/// The manifest's half of the scrobble gate: false for a plugin that isn't
/// loaded, so nothing can scrobble on a record alone.
pub fn declares_scrobble(source: &str) -> bool {
    loaded(source).is_some_and(|loaded| {
        loaded
            .host
            .manifest()
            .capabilities
            .source
            .as_ref()
            .is_some_and(|cap| cap.scrobble)
    })
}

/// `(source id, label)` for every plugin that's loaded and not stopped.
pub fn live_sources() -> Vec<(String, String)> {
    let Ok(table) = HOSTS.read() else {
        return Vec::new();
    };

    let mut live: Vec<(String, String)> = table
        .iter()
        .filter(|(_, loaded)| !matches!(loaded.host.status(), Status::Stopped(_)))
        .map(|(source, loaded)| (source.clone(), loaded.label.clone()))
        .collect();
    live.sort();

    live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_that_isnt_loaded_declares_nothing() {
        assert!(!declares_scrobble("plugin:never-loaded"));
        assert!(!declares_scrobble("subsonic:abc"));
    }
}
