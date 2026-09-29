//! The plugin host's services face: browsing, searching, syncing and picking
//! through a `plugin:<id>` source, the covers its rows show, and the stream
//! opener the engine plays them through (ADR 30). Plugins run as subprocesses
//! behind `rox-plugins`; every call into one runs on the background executor,
//! never the UI thread.
//!
//! The plugins folder is scanned and watched, and [`apply`] keeps one host
//! per plugin that's switched on, loaded, and approved for the exact folder
//! hash it has now. A folder that changed since its approval is switched off
//! until the user switches it on again, which is the approving act. Developer
//! mode, which the user turns on per plugin for one session, approves such a
//! change instead when the manifest declares nothing new. A record whose
//! folder is gone is Missing: nothing starts and nothing is swept.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use gpui::{App, Entity, Global, Task};
use serde_json::{Value, json};

use rox_core::settings::{self, PluginRecord, Settings, SyncedCollection};
use rox_library::cue::{PLUGIN_PREFIX, TrackKey};
use rox_library::locator::{Locator, PluginStream};
use rox_library::members::{self, PluginTrack};
use rox_library::store;
use rox_playback::plugin::{Opened, ReadAt};
use rox_plugins::{Host, HostConfig, Loaded, Options, Status, Stream, loader, wire};

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

/// How long an open or a cover waits for the first [`apply`] before refusing.
const FIRST_APPLY_WAIT: Duration = Duration::from_secs(10);

/// One page of a browse or search. A `None` cursor is the last page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub cursor: Option<String>,
    pub notice: Option<Notice>,
}

/// What the plugin wants said over a page. `setup` means it needs a setting
/// from the user first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub setup: bool,
}

/// Why a plugin source has nothing to answer with, in terms the user can act
/// on from the Plugins page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The Plugins switch is off.
    PluginsOff,
    /// Its own switch is off.
    SwitchedOff,
    /// Its folder changed since the last approval.
    Changed,
    /// Its folder is gone from the plugins folder.
    Missing,
    /// Its folder is there but doesn't load.
    Failed,
    /// It crashed too often and was stopped.
    Stopped,
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

/// A plugin with a host, and what the host was built from: a new hash or new
/// config means a new host.
struct Running {
    host: Host,
    label: String,
    hash: String,
    config: Value,
}

/// Source id to its host. [`apply`] is the only writer.
static HOSTS: LazyLock<RwLock<HashMap<String, Arc<Running>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The plugins folder as the last scan found it. The first read scans, which
/// is the projection's first load at startup, so its rows hide right away.
static FOLDERS: LazyLock<RwLock<Vec<Loaded>>> =
    LazyLock::new(|| RwLock::new(loader::scan(&settings::plugins_dir())));

/// Run at the end of every [`apply`]. See [`after_apply`].
static AFTER_APPLY: OnceLock<fn(&mut App)> = OnceLock::new();

/// The live ids [`apply`] last reloaded the projection for.
static LIVE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Moves whenever the folders or the records might have, so the Plugins
/// page re-reads on a change rather than every frame.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Opens once the first [`apply`] has built the hosts, plugins on or off. A
/// launch restore opens its start entry before that.
static FIRST_APPLY: Gate = Gate::new();

static STARTED: AtomicBool = AtomicBool::new(false);

/// A latch that stays open once opened.
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    const fn new() -> Self {
        Gate {
            open: Mutex::new(false),
            opened: Condvar::new(),
        }
    }

    fn open(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = true;
        }
        self.opened.notify_all();
    }

    fn is_open(&self) -> bool {
        self.open.lock().is_ok_and(|open| *open)
    }

    /// Blocks for at most `limit`. Never on the UI thread: that's where the
    /// first apply runs, so it would wait out the whole limit.
    fn wait(&self, limit: Duration) -> bool {
        let Ok(open) = self.open.lock() else {
            return false;
        };

        self.opened
            .wait_timeout_while(open, limit, |open| !*open)
            .is_ok_and(|(open, _)| *open)
    }
}

/// What `start` hands the rest of the module: the library to reload, and the
/// watch, which stops when this drops.
struct Wiring {
    library: Entity<Library>,
    watch: Option<Arc<loader::Watch>>,
}

impl Global for Wiring {}

fn running(source: &str) -> Option<Arc<Running>> {
    HOSTS.read().ok()?.get(source).cloned()
}

fn host_for(source: &str) -> Result<Host, String> {
    running(source)
        .map(|running| running.host.clone())
        .ok_or_else(|| NO_HOST.to_string())
}

fn record_id(source: &str) -> Option<&str> {
    source.strip_prefix(PLUGIN_PREFIX)
}

fn source_of(id: &str) -> String {
    format!("{PLUGIN_PREFIX}{id}")
}

/// Every folder the last scan found, loadable or not, sorted by id.
pub fn loaded() -> Vec<Loaded> {
    FOLDERS
        .read()
        .map(|folders| folders.clone())
        .unwrap_or_default()
}

/// Whether the plugin's folder is there with a manifest that parses. A
/// record without one is Missing, and its rows hide.
pub fn present(id: &str) -> bool {
    FOLDERS.read().is_ok_and(|folders| {
        folders
            .iter()
            .any(|folder| folder.id == id && folder.manifest.is_some())
    })
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The running host's status, None while the plugin has no host.
pub fn status(id: &str) -> Option<Status> {
    running(&source_of(id)).map(|running| running.host.status())
}

/// Installs the stream opener, follows `player` to open upcoming plugin
/// tracks early, expires old picks, and starts every plugin that's switched
/// on and approved.
/// Once per app; a later window's call is a no-op.
pub fn start(library: Entity<Library>, player: Entity<Player>, cx: &mut App) {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    expire_picks(&library, cx);
    cx.set_global(Wiring {
        library,
        watch: None,
    });
    crate::openers::install(Arc::new(open));
    follow(player, cx);

    // The first scan hashes every plugin folder, which stays off the UI
    // thread. Not on the pool either: cover fetches there wait for the apply
    // after it, and on a fixed-size pool they could hold every thread.
    let (scanned, scan) = async_channel::bounded::<()>(1);
    let spawned = std::thread::Builder::new()
        .name("plugin-scan".into())
        .spawn(move || {
            drop(loaded());
            let _ = scanned.try_send(());
        });
    if let Err(e) = spawned {
        log::warn!("plugins: scanning on the UI thread, no scan thread: {e}");
    }

    cx.spawn(async move |cx| {
        let _ = scan.recv().await;
        cx.update(apply).ok();
    })
    .detach();

    cx.on_app_quit(|cx| {
        let hosts: Vec<Host> = HOSTS
            .read()
            .map(|table| table.values().map(|running| running.host.clone()).collect())
            .unwrap_or_default();

        cx.background_executor().spawn(async move {
            for host in hosts {
                host.hang_up_now("rox quit");
            }
        })
    })
    .detach();
}

/// Plugins run only with the Plugins switch on.
pub fn allowed() -> bool {
    settings::plugins_enabled()
}

/// Played-but-never-added tracks nobody came back to go (ADR 29). At launch
/// only, so a queue in use is never touched; the saved queue and the last
/// track are kept, since they restore by row id.
fn expire_picks(library: &Entity<Library>, cx: &mut App) {
    let db_path = library.read(cx).db_path();
    let expired = cx.background_executor().spawn(async move {
        let session = Settings::load().session;
        let keep: HashSet<i64> = session
            .last_queue
            .iter()
            .flat_map(|queue| queue.entries.iter().map(|entry| entry.id))
            .chain(session.last_track.map(|track| track.id))
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let expired = store::open(&db_path).and_then(|mut conn| {
            members::expire_picks(&mut conn, now - members::PICK_KEEP_SECS, &keep)
        });
        match expired {
            Ok(count) => count,
            Err(e) => {
                log::warn!("plugins: expiring old picks failed: {e}");
                0
            }
        }
    });

    let library = library.clone();
    cx.spawn(async move |cx| {
        let count = expired.await;
        if count > 0 {
            log::info!("plugins: {count} played but never added tracks expired");
            library
                .update(cx, |library, cx| library.reload_projection(cx))
                .ok();
        }
    })
    .detach();
}

/// Brings the hosts in line with the records, the folders and the approvals,
/// then reloads the projection if that changed which rows browse. Run after
/// anything that moves one of them.
///
/// Plugins run only with the Plugins switch on. A switched-on record whose
/// folder isn't the approved one is switched off here, whatever the gate,
/// unless Developer mode approves it first.
pub fn apply(cx: &mut App) {
    let Some(library) = cx.try_global::<Wiring>().map(|w| w.library.clone()) else {
        return;
    };
    let on = allowed();
    if on {
        watch(cx);
    }

    let folders = loaded();
    let folder = |id: &str| folders.iter().find(|folder| folder.id == id);

    let mut records = Settings::load().accounts.plugins;
    if redevelop(&records, &folders) {
        records = Settings::load().accounts.plugins;
    }

    // Changed on disk. A Missing record keeps its switch: deleting the old
    // folder is how many people update a plugin. So does a Developer mode
    // folder that can't load, since it can't run and a save mid-edit is how
    // that happens.
    let changed: Vec<String> = records
        .iter()
        .filter(|record| record.enabled)
        .filter(|record| {
            folder(&record.id).is_some_and(|folder| {
                !settings::plugin_approved(&record.id, &folder.hash)
                    && !(developing(&record.id) && !folder.runs())
            })
        })
        .map(|record| record.id.clone())
        .collect();

    if !changed.is_empty() {
        log::info!("plugins: switched off, changed since approval: {changed:?}");
        let off = changed.clone();
        Settings::update(move |s| {
            for record in &mut s.accounts.plugins {
                if off.contains(&record.id) {
                    record.enabled = false;
                }
            }
        });
    }

    let wanted: HashMap<String, (Loaded, PluginRecord)> = records
        .into_iter()
        .filter(|_| on)
        .filter(|record| record.enabled && !changed.contains(&record.id))
        .filter_map(|record| {
            let folder = folder(&record.id)?;
            let declares = folder
                .manifest
                .as_ref()
                .is_some_and(|m| m.capabilities.source.is_some());

            (folder.runs() && declares).then(|| (source_of(&record.id), (folder.clone(), record)))
        })
        .collect();

    let (stopping, fresh) = reconcile(wanted);

    if !stopping.is_empty() {
        cx.background_executor()
            .spawn(async move {
                for host in stopping {
                    host.stop("switched off");
                }
            })
            .detach();
    }

    // A fresh host syncs what it has switched on, which also starts it.
    for source in fresh {
        let sync = sync_now(library.clone(), &source, cx);
        cx.spawn(async move |_| {
            if let Err(e) = sync.await {
                log::warn!("{source}: the first sync failed: {e}");
            }
        })
        .detach();
    }

    if live_moved() || !changed.is_empty() {
        library.update(cx, |library, cx| library.reload_projection(cx));
    }

    FIRST_APPLY.open();
    bump();

    if let Some(hook) = AFTER_APPLY.get() {
        hook(cx);
    }
}

/// The app's hook for state that follows which plugins run, since apply
/// also runs off the folder watch where no caller is around to follow up.
/// Set once; a second call is ignored.
pub fn after_apply(hook: fn(&mut App)) {
    let _ = AFTER_APPLY.set(hook);
}

/// Swaps the host table to `wanted`, keeping a host whose folder and config
/// haven't moved. Answers the hosts to stop and the sources that got one.
fn reconcile(wanted: HashMap<String, (Loaded, PluginRecord)>) -> (Vec<Host>, Vec<String>) {
    let Ok(mut table) = HOSTS.write() else {
        return (Vec::new(), Vec::new());
    };

    let mut stopping = Vec::new();
    table.retain(|source, running| {
        let keep = wanted.get(source).is_some_and(|(folder, record)| {
            running.hash == folder.hash && running.config == record.config
        });
        if !keep {
            stopping.push(running.host.clone());
        }

        keep
    });

    let mut fresh = Vec::new();
    for (source, (folder, record)) in wanted {
        if table.contains_key(&source) {
            continue;
        }

        let Some(manifest) = folder.manifest.clone() else {
            continue;
        };
        let label = match record.label.is_empty() {
            true => manifest
                .capabilities
                .source
                .as_ref()
                .map(|cap| cap.label.clone())
                .unwrap_or_default(),
            false => record.label.clone(),
        };

        log::info!("{source}: loaded, folder hash {}", folder.hash);
        let config = HostConfig::new(
            folder.dir.clone(),
            manifest,
            record.config.clone(),
            settings::plugin_data_dir(&record.id),
        );
        table.insert(
            source.clone(),
            Arc::new(Running {
                host: Host::new(config),
                label,
                hash: folder.hash.clone(),
                config: record.config.clone(),
            }),
        );
        fresh.push(source);
    }

    (stopping, fresh)
}

/// Whether the ids whose rows browse moved since the last call. The first
/// call only records them: the projection's first load read the same.
fn live_moved() -> bool {
    let live = crate::sources::live_ids(&Settings::load().accounts);
    let Ok(mut last) = LIVE.lock() else {
        return false;
    };

    let moved = last.as_ref().is_some_and(|last| *last != live);
    *last = Some(live);

    moved
}

/// Scans the plugins folder off the UI thread, then applies whatever moved.
pub fn rescan(cx: &mut App) {
    let dir = settings::plugins_dir();

    cx.spawn(async move |cx| {
        let found = cx
            .background_executor()
            .spawn(async move { loader::scan(&dir) })
            .await;

        let moved = match FOLDERS.write() {
            Ok(mut folders) if *folders != found => {
                *folders = found;
                true
            }
            _ => false,
        };

        if moved {
            cx.update(|cx| {
                apply(cx);
                cx.refresh_windows();
            })
            .ok();
        }
    })
    .detach();
}

/// Starts following the plugins folder, once. A folder that appears later is
/// picked up through its parent.
fn watch(cx: &mut App) {
    let Some(wiring) = cx.try_global::<Wiring>() else {
        return;
    };
    if wiring.watch.is_some() {
        return;
    }

    let (tx, events) = async_channel::unbounded::<()>();
    let watch = match loader::Watch::new(&settings::plugins_dir(), move || {
        let _ = tx.try_send(());
    }) {
        Ok(watch) => Arc::new(watch),
        Err(e) => {
            log::warn!("plugins: not watching the plugins folder: {e}");
            return;
        }
    };

    cx.global_mut::<Wiring>().watch = Some(Arc::clone(&watch));

    cx.spawn(async move |cx| {
        while events.recv().await.is_ok() {
            watch.arm();
            if cx.update(rescan).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Switching on is the approving act: the folder's hash goes in the
/// machine's approvals, and the record keeps the manifest for the next
/// diff. Creates the record the first time.
pub fn approve(folder: &Loaded, cx: &mut App) {
    record_approval(folder);
    apply(cx);
}

/// The approval's writes, without the [`apply`] that follows them.
fn record_approval(folder: &Loaded) {
    let Some(manifest) = folder.manifest.as_ref() else {
        return;
    };

    settings::approve_plugin(&folder.id, &folder.hash);

    let id = folder.id.clone();
    let hash = folder.hash.clone();
    let document = folder.document.clone();
    let label = manifest
        .capabilities
        .source
        .as_ref()
        .map(|cap| cap.label.clone())
        .unwrap_or_default();
    Settings::update(move |s| {
        let records = &mut s.accounts.plugins;
        let at = match records.iter().position(|record| record.id == id) {
            Some(at) => at,
            None => {
                records.push(PluginRecord {
                    id: id.clone(),
                    ..Default::default()
                });
                records.len() - 1
            }
        };

        let record = &mut records[at];
        // Scrobbling newly declared starts on: the card just showed it. A
        // user who turned it off under the same declaration keeps it off.
        if declares_scrobble_in(&document) && !declares_scrobble_in(&record.approved_manifest) {
            record.scrobble = true;
        }
        record.enabled = true;
        record.label = label;
        record.hash = hash;
        record.approved_manifest = document;
    });
}

/// Developer mode's half of [`apply`]: approves every switched-on folder in
/// it that changed, loads, and declares nothing its last approval didn't.
/// Answers whether it approved any.
fn redevelop(records: &[PluginRecord], folders: &[Loaded]) -> bool {
    let mut approved = false;
    for record in records.iter().filter(|r| r.enabled && developing(&r.id)) {
        let Some(folder) = folders.iter().find(|f| f.id == record.id) else {
            continue;
        };
        if !folder.runs() || settings::plugin_approved(&record.id, &folder.hash) {
            continue;
        }

        // A new capability, program, entry or scrobble ask goes to the card.
        if !changes(&record.approved_manifest, &folder.document).is_empty() {
            continue;
        }

        log::info!(
            "plugin:{}: developer mode approved {}",
            record.id,
            folder.hash
        );
        record_approval(folder);
        approved = true;
    }

    approved
}

/// Plugins in Developer mode. Never saved, so it ends with the session.
static DEVELOPING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn developing(id: &str) -> bool {
    DEVELOPING
        .lock()
        .is_ok_and(|developing| developing.contains(id))
}

/// Developer mode approves the plugin's folder on its own whenever it
/// changes, for the rest of the session, so a save restarts the plugin
/// instead of switching it off. Switching the plugin off ends it.
pub fn set_developing(id: &str, on: bool) {
    if let Ok(mut developing) = DEVELOPING.lock() {
        match on {
            true => developing.insert(id.to_string()),
            false => developing.remove(id),
        };
    }

    bump();
}

fn declares_scrobble_in(document: &Value) -> bool {
    document["capabilities"]["source"]["scrobble"]
        .as_bool()
        .unwrap_or(false)
}

/// Switches a plugin that has a record. Switching one on here doesn't
/// approve anything: a folder that isn't the approved one goes straight back
/// off in [`apply`].
pub fn set_enabled(id: &str, on: bool, cx: &mut App) {
    if !on {
        set_developing(id, false);
    }

    edit(id, move |record| record.enabled = on);
    apply(cx);
}

/// The record's half of the scrobble gate; the manifest's is
/// [`declares_scrobble`].
pub fn set_scrobble(id: &str, on: bool) {
    edit(id, move |record| record.scrobble = on);
}

/// One config value. The host picks it up when [`apply`] next runs, which
/// restarts it with the new config.
pub fn set_config(id: &str, key: &str, value: Value) {
    let key = key.to_string();
    edit(id, move |record| {
        if !record.config.is_object() {
            record.config = json!({});
        }
        if let Some(config) = record.config.as_object_mut() {
            config.insert(key, value);
        }
    });
}

fn edit(id: &str, change: impl FnOnce(&mut PluginRecord) + Send + 'static) {
    let id = id.to_string();
    Settings::update(move |s| {
        if let Some(record) = s.accounts.plugins.iter_mut().find(|r| r.id == id) {
            change(record);
        }
    });
}

/// Stops the plugin, drops its rows with their membership, forgets its
/// record and its approval, and leaves its folder where it is. Answers the
/// rows removed.
pub fn remove(id: &str, cx: &mut App) -> Task<Result<usize, String>> {
    let Some(library) = cx.try_global::<Wiring>().map(|w| w.library.clone()) else {
        return Task::ready(Err(NO_HOST.to_string()));
    };

    let source = source_of(id);
    let stopping = HOSTS
        .write()
        .ok()
        .and_then(|mut table| table.remove(&source));

    settings::revoke_plugin(id);
    set_developing(id, false);
    let gone = id.to_string();
    Settings::update(move |s| s.accounts.plugins.retain(|record| record.id != gone));
    // The write below reloads the projection, so apply needn't.
    live_moved();
    bump();

    write(library, cx, move |db_path| {
        // Stopped before the delete, so a sync in flight can't land rows
        // behind it.
        if let Some(running) = stopping {
            running.host.stop("removed");
        }

        let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
        members::remove_source(&mut conn, &source).map_err(|e| e.to_string())
    })
}

/// What changed in a manifest since it was approved, for the enable card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    CapabilityAdded(String),
    CapabilityRemoved(String),
    ProgramAdded(String),
    /// The scrobble declaration, on or off.
    Scrobble(bool),
    /// The entry, which is how the plugin runs.
    Entry,
}

/// Compares the manifests as written, so a capability this build doesn't
/// know yet still shows.
pub fn changes(approved: &Value, now: &Value) -> Vec<Change> {
    let keys = |doc: &Value| -> Vec<String> {
        doc["capabilities"]
            .as_object()
            .map(|caps| caps.keys().cloned().collect())
            .unwrap_or_default()
    };
    let programs = |doc: &Value| -> Vec<String> {
        doc["programs"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let (was, is) = (keys(approved), keys(now));
    let mut found: Vec<Change> = is
        .iter()
        .filter(|key| !was.contains(key))
        .map(|key| Change::CapabilityAdded(key.clone()))
        .collect();
    found.extend(
        was.iter()
            .filter(|key| !is.contains(key))
            .map(|key| Change::CapabilityRemoved(key.clone())),
    );

    let before = programs(approved);
    found.extend(
        programs(now)
            .into_iter()
            .filter(|program| !before.contains(program))
            .map(Change::ProgramAdded),
    );

    let scrobbles = declares_scrobble_in(now);
    if scrobbles != declares_scrobble_in(approved) {
        found.push(Change::Scrobble(scrobbles));
    }

    if approved["entry"] != now["entry"] {
        found.push(Change::Entry);
    }

    found
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

/// For the blocking paths that can run before the first [`apply`]. Answers
/// at once when [`start`] never ran, since then nothing opens the gate.
fn await_first_apply(source: &str) {
    if FIRST_APPLY.is_open() || !STARTED.load(Ordering::Relaxed) {
        return;
    }

    let began = Instant::now();
    let ready = FIRST_APPLY.wait(FIRST_APPLY_WAIT);
    log::info!(
        "{source}: waited {:.0} ms for the first apply{}",
        began.elapsed().as_secs_f64() * 1000.0,
        if ready { "" } else { ", and gave up" }
    );
}

/// What the engine calls, on its decode thread, for every plugin entry.
fn open(stream: &PluginStream) -> Result<Opened, String> {
    await_first_apply(&stream.source);

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
        notice: wire.notice.map(|notice| Notice {
            text: notice.text,
            setup: notice.kind == wire::NoticeKind::Setup,
        }),
    }
}

fn listing(
    source: &str,
    method: &'static str,
    params: Value,
    cx: &App,
) -> Task<Result<Page, String>> {
    let source = source.to_string();

    cx.background_executor().spawn(async move {
        // A panel restored at launch lists before the first apply has built
        // the hosts, and would otherwise read that as no host at all.
        await_first_apply(&source);
        let host = host_for(&source)?;

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
    bump();

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
    bump();

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
    if running(source).is_none() {
        return Task::ready(Err(NO_HOST.to_string()));
    }

    let source = source.to_string();
    write(library, cx, move |db_path| {
        let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
        members::pick(&mut conn, &source, &tracks).map_err(|e| e.to_string())
    })
}

/// Add to Library: the tracks show in the library from now on, rather
/// than only playing (ADR 29).
pub fn save(
    library: Entity<Library>,
    source: &str,
    tracks: Vec<PluginTrack>,
    cx: &mut App,
) -> Task<Result<Vec<TrackKey>, String>> {
    let source = source.to_string();
    write(library, cx, move |db_path| {
        let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
        members::save(&mut conn, &source, &tracks).map_err(|e| e.to_string())
    })
}

/// Remove from Library: tracks added one at a time go back to being only
/// played, or go altogether if nothing else holds them.
pub fn unsave(
    library: Entity<Library>,
    source: &str,
    paths: Vec<String>,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let source = source.to_string();
    write(library, cx, move |db_path| {
        let mut conn = store::open(&db_path).map_err(|e| e.to_string())?;
        members::unsave(&mut conn, &source, &paths).map_err(|e| e.to_string())
    })
}

/// Blocking; called from `fetch_cover`'s thread. A miss there is held for
/// minutes, so one asked before the first apply waits for it.
pub fn cover(source: &str, key: &str) -> Option<Vec<u8>> {
    await_first_apply(source);

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
    running(source).is_some_and(|running| {
        running
            .host
            .manifest()
            .capabilities
            .source
            .as_ref()
            .is_some_and(|cap| cap.scrobble)
    })
}

/// A running plugin as the Add Panel pickers list it.
#[derive(Clone, Debug)]
pub struct RunningPlugin {
    pub id: String,
    /// The manifest's display name.
    pub name: String,
    /// Its declared panels, past the External Sources panel every one gets.
    pub panels: Vec<rox_plugins::manifest::DeclaredPanel>,
}

/// Every running plugin, by display name. Running is switched on and
/// approved, so nothing lists for code nobody agreed to. Empty while
/// plugins are off.
pub fn running_plugins() -> Vec<RunningPlugin> {
    if !allowed() {
        return Vec::new();
    }
    let Ok(table) = HOSTS.read() else {
        return Vec::new();
    };

    let mut plugins: Vec<RunningPlugin> = table
        .iter()
        .filter_map(|(source, running)| {
            let manifest = running.host.manifest();
            Some(RunningPlugin {
                id: record_id(source)?.to_string(),
                name: manifest.name.clone(),
                panels: manifest.capabilities.panels.clone(),
            })
        })
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

    plugins
}

/// Whether `source` has a host that isn't stopped. Cheap enough to ask
/// while drawing.
pub fn answers(source: &str) -> bool {
    allowed()
        && running(source)
            .is_some_and(|running| !matches!(running.host.status(), Status::Stopped(_)))
}

/// Why `source` can't answer, or None when it has a host that's running.
/// Reads the settings file, so it's for after a call failed, not per frame.
pub fn unavailable(source: &str) -> Option<Unavailable> {
    if !allowed() {
        return Some(Unavailable::PluginsOff);
    }

    if let Some(running) = running(source) {
        return match running.host.status() {
            Status::Stopped(_) => Some(Unavailable::Stopped),
            _ => None,
        };
    }

    let id = record_id(source)?;
    let folders = loaded();
    let Some(folder) = folders.iter().find(|folder| folder.id == id) else {
        return Some(Unavailable::Missing);
    };
    if !folder.runs() {
        return Some(Unavailable::Failed);
    }

    let record = Settings::load()
        .accounts
        .plugins
        .into_iter()
        .find(|record| record.id == id);
    let approved_before = record.as_ref().is_some_and(|r| !r.hash.is_empty());
    if approved_before && !settings::plugin_approved(id, &folder.hash) {
        return Some(Unavailable::Changed);
    }

    Some(Unavailable::SwitchedOff)
}

/// Whether the plugin has a host right now: switched on, approved, loaded.
pub fn is_running(id: &str) -> bool {
    running(&source_of(id)).is_some()
}

/// `(source id, label)` for every plugin that's loaded and not stopped.
pub fn live_sources() -> Vec<(String, String)> {
    let Ok(table) = HOSTS.read() else {
        return Vec::new();
    };

    let mut live: Vec<(String, String)> = table
        .iter()
        .filter(|(_, running)| !matches!(running.host.status(), Status::Stopped(_)))
        .map(|(source, running)| (source.clone(), running.label.clone()))
        .collect();
    live.sort();

    live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_wakes_a_waiter_when_it_opens() {
        let gate = Arc::new(Gate::new());
        let opener = Arc::clone(&gate);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            opener.open();
        });

        assert!(gate.wait(Duration::from_secs(10)));
        assert!(gate.is_open());
        thread.join().unwrap();
    }

    #[test]
    fn a_gate_nobody_opens_gives_up_at_its_limit() {
        let gate = Gate::new();
        let began = Instant::now();

        assert!(!gate.wait(Duration::from_millis(50)));
        assert!(began.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn a_plugin_that_isnt_loaded_declares_nothing() {
        assert!(!declares_scrobble("plugin:never-loaded"));
        assert!(!declares_scrobble("subsonic:abc"));
    }

    fn manifest(caps: Value, programs: Value, entry: &str) -> Value {
        json!({
            "id": "tones",
            "entry": { "script": { "path": entry, "interpreter": "python3" } },
            "capabilities": caps,
            "programs": programs,
        })
    }

    #[test]
    fn an_unchanged_manifest_has_no_changes() {
        let doc = manifest(json!({ "source": { "label": "T" } }), json!(["a"]), "t.py");
        assert!(changes(&doc, &doc).is_empty());
    }

    #[test]
    fn the_diff_names_what_an_approval_would_grant() {
        let before = manifest(
            json!({ "source": { "label": "T" } }),
            json!(["a", "b"]),
            "t.py",
        );
        let after = manifest(
            json!({ "source": { "label": "T", "scrobble": true }, "panels": [] }),
            json!(["b", "c"]),
            "u.py",
        );

        assert_eq!(
            changes(&before, &after),
            vec![
                Change::CapabilityAdded("panels".into()),
                Change::ProgramAdded("c".into()),
                Change::Scrobble(true),
                Change::Entry,
            ]
        );

        let dropped = manifest(json!({}), json!(["b"]), "t.py");
        assert_eq!(
            changes(&before, &dropped),
            vec![Change::CapabilityRemoved("source".into())]
        );
    }

    #[test]
    fn a_first_approval_diffs_against_nothing() {
        let doc = manifest(json!({ "source": { "label": "T" } }), json!([]), "t.py");
        assert_eq!(
            changes(&Value::Null, &doc),
            vec![Change::CapabilityAdded("source".into()), Change::Entry]
        );
    }
}
