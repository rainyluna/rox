//! The plugin host's services face: browsing, searching, syncing and picking
//! through a `plugin:<id>` source, and the covers its rows show (ADR 30).
//! Plugins run as subprocesses behind `rox-plugins`; nothing here runs plugin
//! code on the UI thread.
//!
//! No host is wired in yet, so every call answers "no plugin host".

use gpui::{App, Entity, Task};

use rox_library::cue::TrackKey;
use rox_library::members::PluginTrack;

use crate::catalog::Library;

const NO_HOST: &str = "no plugin host";

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

/// `node: None` asks for the roots.
pub fn browse(
    _source: &str,
    _node: Option<String>,
    _cursor: Option<String>,
    _cx: &App,
) -> Task<Result<Page, String>> {
    Task::ready(Err(NO_HOST.to_string()))
}

pub fn search(
    _source: &str,
    _query: String,
    _cursor: Option<String>,
    _cx: &App,
) -> Task<Result<Page, String>> {
    Task::ready(Err(NO_HOST.to_string()))
}

/// Turns a collection's sync on or off. Answers the rows it now holds.
pub fn set_synced(
    _library: Entity<Library>,
    _source: &str,
    _node: &str,
    _title: &str,
    _on: bool,
    _cx: &mut App,
) -> Task<Result<usize, String>> {
    Task::ready(Err(NO_HOST.to_string()))
}

/// Syncs every collection the source has switched on. Answers the rows
/// written.
pub fn sync_now(
    _library: Entity<Library>,
    _source: &str,
    _cx: &mut App,
) -> Task<Result<usize, String>> {
    Task::ready(Err(NO_HOST.to_string()))
}

/// Adds single tracks to the library and answers their keys, for a caller
/// that wants to queue them straight away.
pub fn pick(
    _library: Entity<Library>,
    _source: &str,
    _tracks: Vec<PluginTrack>,
    _cx: &mut App,
) -> Task<Result<Vec<TrackKey>, String>> {
    Task::ready(Err(NO_HOST.to_string()))
}

/// Blocking; called from `fetch_cover`'s thread.
pub fn cover(_source: &str, _key: &str) -> Option<Vec<u8>> {
    None
}

/// `(source id, label)` for every running plugin.
pub fn live_sources() -> Vec<(String, String)> {
    Vec::new()
}
