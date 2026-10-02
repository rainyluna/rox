//! What a plugin has put in the library, and taking it back out (ADR 29's
//! second amendment, ADR 30). The reads open their own connection, the way
//! the source browser's do; the writes go through `plugins`, so taking a
//! row out here is the same act as in the source browser.
//!
//! A row a kept collection holds is never taken out on its own: the next
//! sync would put it back. Letting go of it means letting go of the
//! collection.

use std::collections::BTreeMap;

use gpui::{App, Entity, Task};
use rox_core::settings::{Settings, SyncedCollection};
use rox_library::cue::PLUGIN_PREFIX;
use rox_library::members::{self, InLibrary};
use rox_library::store;

use crate::catalog::Library;
use crate::plugins;

/// A collection the user keeps, as a menu names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub source: String,
    /// The plugin's node id.
    pub id: String,
    pub title: String,
}

/// What can take a set of rows back out of the library.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removable {
    /// The paths added one at a time, by source.
    pub saved: Vec<(String, Vec<String>)>,
    pub kept: Vec<Kept>,
}

impl Removable {
    pub fn is_empty(&self) -> bool {
        self.saved.is_empty() && self.kept.is_empty()
    }
}

/// The saved rows among `ids` and the kept collections holding any of them.
/// Empty when the library has no database yet or the read fails.
pub fn removable(library: &Library, ids: &[i64]) -> Removable {
    if ids.is_empty() {
        return Removable::default();
    }

    let db = library.db_path();
    if !db.exists() {
        return Removable::default();
    }

    let holds = match store::open(&db).and_then(|conn| members::holds(&conn, ids)) {
        Ok(holds) => holds,
        Err(e) => {
            log::warn!("plugin rows: reading what holds {} rows: {e}", ids.len());
            return Removable::default();
        }
    };

    let mut saved: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (source, path) in holds.saved {
        saved.entry(source).or_default().push(path);
    }

    // Read only when there's a collection to name, since it's a file read.
    let records = match holds.kept.is_empty() {
        true => Vec::new(),
        false => Settings::load().accounts.plugins,
    };
    let kept = holds
        .kept
        .into_iter()
        .map(|(source, id)| {
            let title = records
                .iter()
                .filter(|record| source.strip_prefix(PLUGIN_PREFIX) == Some(record.id.as_str()))
                .flat_map(|record| record.synced.iter())
                .find(|collection| collection.id == id)
                .map(|collection| collection.title.clone())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| id.clone());

            Kept { source, id, title }
        })
        .collect();

    Removable {
        saved: saved.into_iter().collect(),
        kept,
    }
}

/// Remove from Library, the same write the source browser makes: each
/// source's saved rows go back to being only played, or go altogether if
/// nothing else holds them. Answers the rows that went.
pub fn remove(
    library: Entity<Library>,
    saved: Vec<(String, Vec<String>)>,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let writes: Vec<_> = saved
        .into_iter()
        .map(|(source, paths)| plugins::unsave(library.clone(), &source, paths, cx))
        .collect();

    cx.spawn(async move |_| {
        let mut gone = 0;
        for write in writes {
            gone += write.await?;
        }

        Ok(gone)
    })
}

/// Stop keeping a collection, exactly as the source browser's keep switch
/// does. Every row only it held leaves the library.
pub fn stop_keeping(
    library: Entity<Library>,
    kept: &Kept,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let collection = SyncedCollection {
        id: kept.id.clone(),
        title: kept.title.clone(),
        ..SyncedCollection::default()
    };

    plugins::set_synced(library, &kept.source, collection, false, cx)
}

/// How many of the plugin's rows are in the library. Zero when the library
/// has no database yet or the read fails.
pub fn in_library(library: &Library, source: &str) -> InLibrary {
    let db = library.db_path();
    if !db.exists() {
        return InLibrary::default();
    }

    store::open(&db)
        .and_then(|conn| members::in_library(&conn, source))
        .inspect_err(|e| log::warn!("{source}: counting its library rows: {e}"))
        .unwrap_or_default()
}
