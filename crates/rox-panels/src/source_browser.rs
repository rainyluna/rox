//! The source browser: one plugin source's tree, browsed, searched and
//! synced (ADR 30). It's a core panel pinned to a `plugin:<id>` source id,
//! and the plugin supplies none of it; every live call goes through
//! [`rox_services::plugins`] on the background executor.
//!
//! A synced collection opens from the library through its membership with no
//! plugin call, so it still browses with the plugin stopped or the network
//! down. A browsed track only becomes a library row when it's played, queued
//! or added to a playlist, through a pick.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Div, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Modifiers,
    MouseButton, MouseDownEvent, ObjectFit, Pixels, ScrollStrategy, SharedString, Subscription,
    Task, UniformListScrollHandle, WeakEntity, Window, div, img, prelude::*, px, svg, uniform_list,
};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::{Icon, Sizable};
use rox_core::QUEUE_CAP;
use rox_core::settings::{Settings, SyncedCollection};
use rox_dock::{Panel, PanelEvent, TabPanel};
use rox_library::cue::{PLUGIN_PREFIX, TrackKey, source_id};
use rox_library::members::{self, PluginTrack};
use rox_services::plugins::{self, Entry, Page};
use serde::{Deserialize, Serialize};

use crate::assets::icons;
use crate::catalog::LibraryEvent;
use crate::design::{palette, tokens};
use crate::panel::{self, AppState, PanelChrome, PanelSettings, Then, Tone, WithIds};
use crate::panel_settings;
use crate::player::fmt_time;
use crate::query::search::{SearchBox, SearchEvent};
use crate::selection::SelectionEvent;
use crate::thumbs::Thumb;

/// Every row is two lines tall, node or track: the list is a uniform_list.
const ROW_H: Pixels = px(40.);

const ART: Pixels = px(32.);

/// The tracks a play queues: up to `cap` around the clicked one, half of
/// them behind it for Prev, the rest ahead, a short side's share going to
/// the other. Answers the range and where the clicked track sits in it.
fn play_window(len: usize, at: usize, cap: usize) -> (std::ops::Range<usize>, usize) {
    let lo = at.saturating_sub(cap / 2);
    let hi = (lo + cap).min(len);
    let lo = hi.saturating_sub(cap).min(lo);

    (lo..hi, at - lo)
}

/// How near the end of the list a scroll gets before the next page is asked
/// for.
const PAGE_AHEAD: usize = 20;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceBrowserConfig {
    #[serde(flatten)]
    pub chrome: PanelChrome,
    /// `plugin:<id>`. Empty shows a picker over the running sources.
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Crumb {
    id: String,
    title: String,
}

/// Where the list is: a path down the tree, or a search's results.
#[derive(Clone, Debug, Default, PartialEq)]
struct Place {
    trail: Vec<Crumb>,
    query: Option<String>,
}

impl Place {
    /// The node being listed. None at the roots and for a search.
    fn node(&self) -> Option<&Crumb> {
        match self.query {
            Some(_) => None,
            None => self.trail.last(),
        }
    }

    fn is_root(&self) -> bool {
        self.trail.is_empty() && self.query.is_none()
    }
}

/// What an answer did to the list.
#[derive(Debug, PartialEq)]
enum Landed {
    /// A newer request replaced the one this answered.
    Stale,
    /// A first page. `moved` is false when it re-read the place already shown.
    Replaced {
        moved: bool,
    },
    Appended,
    Failed,
}

struct Pending {
    generation: u64,
    /// Some for a first page, which replaces the list and moves it there;
    /// None for the next page of the list shown.
    place: Option<Place>,
}

/// The list and its paging. A failed request leaves the last good list up,
/// so the reason shows above it rather than in place of it.
#[derive(Default)]
struct Listing {
    place: Place,
    entries: Vec<Entry>,
    cursor: Option<String>,
    error: Option<String>,
    pending: Option<Pending>,
    generation: u64,
}

impl Listing {
    fn begin(&mut self, place: Option<Place>) -> u64 {
        self.generation += 1;
        self.pending = Some(Pending {
            generation: self.generation,
            place,
        });

        self.generation
    }

    fn land(&mut self, generation: u64, result: Result<Page, String>) -> Landed {
        let Some(pending) = self.pending.take_if(|p| p.generation == generation) else {
            return Landed::Stale;
        };

        let page = match result {
            Ok(page) => page,

            // The cursor stays, so the same page can be asked for again.
            Err(e) => {
                self.error = Some(e);
                return Landed::Failed;
            }
        };

        self.cursor = page.cursor;
        self.error = None;

        match pending.place {
            Some(place) => {
                let moved = place != self.place;
                self.place = place;
                self.entries = page.entries;
                Landed::Replaced { moved }
            }

            None => {
                self.entries.extend(page.entries);
                Landed::Appended
            }
        }
    }

    /// A failed page doesn't retry on its own; reopening the place does.
    fn wants_more(&self) -> bool {
        self.pending.is_none() && self.error.is_none() && self.cursor.is_some()
    }

    fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// The place asked for last, whether or not it has landed.
    fn target(&self) -> &Place {
        self.pending
            .as_ref()
            .and_then(|pending| pending.place.as_ref())
            .unwrap_or(&self.place)
    }
}

pub struct SourceBrowserPanel {
    state: AppState,
    config: SourceBrowserConfig,
    label: SharedString,
    /// The record's switched-on collections, read at the list's cadence.
    synced: Vec<SyncedCollection>,
    /// Rows each synced collection holds, by node id.
    members: HashMap<String, usize>,
    listing: Listing,
    /// Library ids of listed tracks that are already rows, by key. Resolved
    /// when the list changes, so a click never queries per row.
    ids: HashMap<String, i64>,
    /// Collections whose switch is still working.
    syncing: HashSet<String>,
    /// Track keys a pick is still adding.
    picking: HashSet<String>,
    playing: Option<TrackKey>,
    opening: Option<TrackKey>,
    /// A pick or sync that failed, as a headline and the plugin's reason.
    failure: Option<(SharedString, String)>,
    /// By row: the list only appends until a new place replaces it, which
    /// clears these.
    selected: HashSet<usize>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    /// None means the press missed the rows, and the menu is the panel's own.
    menu_row: Option<usize>,
    search: gpui::Entity<SearchBox>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    tab_panel: Option<WeakEntity<TabPanel>>,
    _library_changed: Subscription,
    _player_changed: Subscription,
    _selection_changed: Subscription,
    _thumbs_changed: Subscription,
    _search_events: Subscription,
}

impl SourceBrowserPanel {
    pub fn new(
        state: AppState,
        config: SourceBrowserConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _library_changed = cx.subscribe(
            &state.library,
            |this: &mut Self, _, event: &LibraryEvent, cx| {
                if matches!(event, LibraryEvent::Updated) {
                    this.library_changed(cx);
                }
            },
        );

        // The pump notifies every tick; only the playing and opening keys
        // matter here.
        let _player_changed = cx.observe(&state.player, |this: &mut Self, player, cx| {
            let player = player.read(cx);
            let playing = player.now_playing().map(|now| now.key);
            let opening = player.opening();

            if this.playing == playing && this.opening == opening {
                return;
            }

            this.playing = playing;
            this.opening = opening;
            cx.notify();
        });

        // Another panel's pick clears the marks here. The clear is local:
        // republishing would fight whoever just published.
        let _selection_changed = cx.subscribe(
            &state.selection,
            |this: &mut Self, _, event: &SelectionEvent, cx| {
                if event.source == cx.entity().entity_id() || this.selected.is_empty() {
                    return;
                }

                this.clear_marks();
                cx.notify();
            },
        );

        let _thumbs_changed = cx.observe(&state.thumbs, |_: &mut Self, _, cx| cx.notify());

        let search = cx.new(|cx| {
            SearchBox::new(rox_i18n::t!("source-browser-search"), "", window, cx)
                .small()
                .icon()
        });
        let _search_events = cx.subscribe_in(&search, window, Self::on_search_event);

        let (playing, opening) = {
            let player = state.player.read(cx);
            (player.now_playing().map(|now| now.key), player.opening())
        };

        let mut panel = SourceBrowserPanel {
            state,
            config,
            label: SharedString::default(),
            synced: Vec::new(),
            members: HashMap::new(),
            listing: Listing::default(),
            ids: HashMap::new(),
            syncing: HashSet::new(),
            picking: HashSet::new(),
            playing,
            opening,
            failure: None,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            menu_row: None,
            search,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle().tab_stop(true),
            tab_panel: None,
            _library_changed,
            _player_changed,
            _selection_changed,
            _thumbs_changed,
            _search_events,
        };

        if !panel.config.source.is_empty() {
            panel.read_record(cx);
            panel.go(Place::default(), cx);
        }

        panel
    }

    fn source(&self) -> &str {
        &self.config.source
    }

    /// Pins the panel to a source. The config is part of the layout dump, so
    /// the tab-panel repaint writes the choice to disk.
    fn choose(&mut self, source: String, cx: &mut Context<Self>) {
        self.config.source = source;
        self.listing = Listing::default();
        self.failure = None;
        self.clear_marks();

        self.read_record(cx);
        self.go(Place::default(), cx);
        panel::refresh_tab_panel(&self.tab_panel, cx);
    }

    /// The label, the synced collections and their row counts. Reads the
    /// settings file and the database, so never per frame.
    fn read_record(&mut self, cx: &App) {
        let record = self.source().strip_prefix(PLUGIN_PREFIX).and_then(|id| {
            Settings::load()
                .accounts
                .plugins
                .into_iter()
                .find(|record| record.id == id)
        });

        let live = plugins::live_sources()
            .into_iter()
            .find(|(source, _)| source == self.source())
            .map(|(_, label)| label);
        let label = live
            .or_else(|| record.as_ref().map(|record| record.label.clone()))
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| self.source().to_string());

        self.label = label.into();
        self.synced = record.map(|record| record.synced).unwrap_or_default();

        let conn = rox_library::store::open(&self.state.library.read(cx).db_path()).ok();
        self.members = match conn {
            Some(conn) => self
                .synced
                .iter()
                .map(|c| {
                    let rows = members::list(&conn, self.source(), &c.id).map_or(0, |k| k.len());
                    (c.id.clone(), rows)
                })
                .collect(),

            None => HashMap::new(),
        };
    }

    fn is_synced(&self, node: &str) -> bool {
        self.synced.iter().any(|c| c.id == node)
    }

    fn key_for(&self, key: &str) -> TrackKey {
        TrackKey {
            source: source_id(self.source()),
            path: key.into(),
            sub: 0,
        }
    }

    /// Lists a place from its first page. A synced collection lands at once
    /// from the library; everything else waits on the plugin.
    fn go(&mut self, place: Place, cx: &mut Context<Self>) {
        let generation = self.listing.begin(Some(place.clone()));

        if let Some(node) = place.node()
            && self.is_synced(&node.id)
        {
            let page = self.library_page(&node.id, cx);
            self.landed(generation, Ok(page), false, cx);
            return;
        }

        let task = match &place.query {
            Some(query) => plugins::search(self.source(), query.clone(), None, cx),

            None => plugins::browse(
                self.source(),
                place.node().map(|node| node.id.clone()),
                None,
                cx,
            ),
        };

        self.await_page(generation, task, place.is_root(), cx);
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if !self.listing.wants_more() {
            return;
        }

        let cursor = self.listing.cursor.clone();
        let place = self.listing.place.clone();
        let generation = self.listing.begin(None);

        let task = match &place.query {
            Some(query) => plugins::search(self.source(), query.clone(), cursor, cx),

            None => plugins::browse(
                self.source(),
                place.node().map(|node| node.id.clone()),
                cursor,
                cx,
            ),
        };

        self.await_page(generation, task, false, cx);
        cx.notify();
    }

    fn await_page(
        &mut self,
        generation: u64,
        task: Task<Result<Page, String>>,
        root: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.landed(generation, result, root, cx))
                .ok();
        })
        .detach();
    }

    /// A root the plugin can't list falls back to the synced collections,
    /// which open without it.
    fn landed(
        &mut self,
        generation: u64,
        result: Result<Page, String>,
        root: bool,
        cx: &mut Context<Self>,
    ) {
        let landed = match result {
            Err(e) if root => {
                let fallback = self.synced_page();
                let landed = self.listing.land(generation, Ok(fallback));
                if landed != Landed::Stale {
                    self.listing.error = Some(e);
                }
                landed
            }

            result => self.listing.land(generation, result),
        };

        if landed == Landed::Stale {
            return;
        }

        if landed == (Landed::Replaced { moved: true }) {
            self.clear_marks();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }

        self.resolve_ids(cx);
        cx.notify();
    }

    fn synced_page(&self) -> Page {
        let entries = self
            .synced
            .iter()
            .map(|c| Entry::Node {
                id: c.id.clone(),
                title: c.title.clone(),
                subtitle: String::new(),
                collection: true,
            })
            .collect();

        Page {
            entries,
            cursor: None,
        }
    }

    /// A synced collection's tracks as the library holds them, in the
    /// plugin's order.
    fn library_page(&self, node: &str, cx: &App) -> Page {
        let library = self.state.library.read(cx);
        let keys = rox_library::store::open(&library.db_path())
            .ok()
            .and_then(|conn| members::list(&conn, self.source(), node).ok())
            .unwrap_or_default();

        let entries = keys
            .iter()
            .filter_map(|key| {
                let (_, meta) = library.resolve_key(key)?;
                Some(Entry::Track(PluginTrack {
                    key: key.path.to_string_lossy().into_owned(),
                    title: meta.title,
                    artist: meta.artist,
                    album_artist: meta.album_artist,
                    album: meta.album,
                    genre: meta.genre,
                    year: meta.year,
                    track_no: meta.track_no,
                    duration_ms: meta.duration_ms,
                    codec: meta.codec,
                    bitrate_kbps: meta.bitrate_kbps,
                    ..PluginTrack::default()
                }))
            })
            .collect();

        Page {
            entries,
            cursor: None,
        }
    }

    fn resolve_ids(&mut self, cx: &App) {
        let library = self.state.library.read(cx);
        let source = source_id(self.source());

        self.ids = self
            .listing
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Track(track) => {
                    let key = TrackKey {
                        source: source.clone(),
                        path: track.key.clone().into(),
                        sub: 0,
                    };
                    library.id_for_key(&key).map(|id| (track.key.clone(), id))
                }

                Entry::Node { .. } => None,
            })
            .collect();
    }

    /// A synced collection being shown re-reads, since its rows may have
    /// just moved; anything the plugin listed stays as it was.
    fn library_changed(&mut self, cx: &mut Context<Self>) {
        if self.source().is_empty() {
            return;
        }

        self.read_record(cx);
        self.resolve_ids(cx);

        let place = self.listing.place.clone();
        let shows_synced = place.node().is_some_and(|node| self.is_synced(&node.id));
        if shows_synced && !self.listing.loading() {
            self.go(place, cx);
        }

        cx.notify();
    }

    fn set_synced(&mut self, node: String, title: String, on: bool, cx: &mut Context<Self>) {
        let task = plugins::set_synced(
            self.state.library.clone(),
            self.source(),
            &node,
            &title,
            on,
            cx,
        );

        // The record is written before the task starts, so the switch reads
        // the new state now and spins until the rows follow.
        self.read_record(cx);
        self.syncing.insert(node.clone());
        self.failure = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.syncing.remove(&node);

                if let Err(e) = result {
                    this.failure =
                        Some((rox_i18n::t!("source-browser-sync-failed", title = title), e));
                }

                this.read_record(cx);

                // Inside the collection, the list swaps between the plugin's
                // and the library's.
                let place = this.listing.place.clone();
                if place.node().is_some_and(|crumb| crumb.id == node) {
                    this.go(place, cx);
                }

                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hands `then` the tracks' keys once every one is a library row. Tracks
    /// that already are skip the pick.
    fn with_picked(
        &mut self,
        tracks: Vec<PluginTrack>,
        then: impl FnOnce(Vec<TrackKey>, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<TrackKey> = tracks.iter().map(|t| self.key_for(&t.key)).collect();
        let unpicked: Vec<PluginTrack> = tracks
            .into_iter()
            .filter(|t| !self.ids.contains_key(&t.key))
            .collect();

        if unpicked.is_empty() {
            then(keys, cx);
            return;
        }

        let marked: Vec<String> = unpicked.iter().map(|t| t.key.clone()).collect();
        self.picking.extend(marked.iter().cloned());
        self.failure = None;
        cx.notify();

        let task = plugins::pick(self.state.library.clone(), self.source(), unpicked, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                for key in &marked {
                    this.picking.remove(key);
                }

                match result {
                    Ok(_) => {
                        this.resolve_ids(cx);
                        then(keys, cx);
                    }

                    Err(e) => {
                        this.failure = Some((rox_i18n::t!("source-browser-pick-failed"), e));
                    }
                }

                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The tracks become the playing context, as a library run does, so
    /// playback carries on through them (ADR 16).
    fn play(&mut self, tracks: Vec<PluginTrack>, start: usize, cx: &mut Context<Self>) {
        let player = self.state.player.clone();
        self.with_picked(
            tracks,
            move |keys, cx| player.update(cx, |player, cx| player.play_at(keys, start, cx)),
            cx,
        );
    }

    /// The listed tracks around a row, from it onward. Every one is picked
    /// first, since only rows can be queued.
    fn play_from(&mut self, ix: usize, cx: &mut Context<Self>) {
        let rows: Vec<usize> = (0..self.listing.entries.len())
            .filter(|&row| self.track_at(row).is_some())
            .collect();
        let Some(at) = rows.iter().position(|&row| row == ix) else {
            return;
        };

        let (window, start) = play_window(rows.len(), at, QUEUE_CAP);
        let tracks = self.tracks_at(&rows[window]);
        self.play(tracks, start, cx);
    }

    fn queue(&mut self, tracks: Vec<PluginTrack>, next: bool, cx: &mut Context<Self>) {
        let player = self.state.player.clone();
        self.with_picked(
            tracks,
            move |keys, cx| {
                player.update(cx, |player, cx| match next {
                    true => player.play_next(keys, cx),
                    false => player.enqueue(keys, cx),
                })
            },
            cx,
        );
    }

    fn track_at(&self, ix: usize) -> Option<&PluginTrack> {
        match self.listing.entries.get(ix)? {
            Entry::Track(track) => Some(track),
            Entry::Node { .. } => None,
        }
    }

    fn tracks_at(&self, rows: &[usize]) -> Vec<PluginTrack> {
        rows.iter()
            .filter_map(|&ix| self.track_at(ix).cloned())
            .collect()
    }

    fn selected_rows(&self) -> Vec<usize> {
        let mut rows: Vec<usize> = self.selected.iter().copied().collect();
        rows.sort_unstable();
        rows
    }

    /// A node opens; a track plays.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        match self.listing.entries.get(ix) {
            Some(Entry::Node { id, title, .. }) => {
                let mut place = self.listing.place.clone();
                // A node opened from search results starts a trail of its own.
                if place.query.take().is_some() {
                    place.trail.clear();
                }
                place.trail.push(Crumb {
                    id: id.clone(),
                    title: title.clone(),
                });
                self.go(place, cx);
            }

            Some(Entry::Track(_)) => self.play_from(ix, cx),

            None => {}
        }
    }

    /// Keeps the first `depth` crumbs. The crumb being shown reloads, which
    /// is how a failed page is asked for again.
    fn go_up(&mut self, depth: usize, cx: &mut Context<Self>) {
        let mut trail = self.listing.place.trail.clone();
        if self.listing.place.query.is_some() {
            trail.clear();
        }
        trail.truncate(depth);

        self.go(Place { trail, query: None }, cx);
    }

    fn on_search_event(
        &mut self,
        search: &gpui::Entity<SearchBox>,
        event: &SearchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // A search is a request to the plugin's service, so it runs on
            // Enter rather than per keystroke. Clearing the box goes back.
            SearchEvent::Submitted => {
                let query = search.read(cx).query().trim().to_string();
                if query.is_empty() || self.source().is_empty() {
                    return;
                }

                let trail = self.listing.place.trail.clone();
                self.go(
                    Place {
                        trail,
                        query: Some(query),
                    },
                    cx,
                );
            }

            // The box keeps its text when a crumb or a result navigates away,
            // so only a clear over results goes back.
            SearchEvent::Changed => {
                let cleared = search.read(cx).query().trim().is_empty();
                let target = self.listing.target();
                if cleared && target.query.is_some() {
                    let trail = target.trail.clone();
                    self.go(Place { trail, query: None }, cx);
                }
            }

            SearchEvent::Dismissed => window.focus(&self.focus),

            SearchEvent::FocusChanged => cx.notify(),
        }
    }

    fn clear_marks(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
    }

    fn select(&mut self, ix: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        if ix >= self.listing.entries.len() {
            return;
        }

        if modifiers.shift {
            let anchor = self.anchor.unwrap_or(ix);
            let range = anchor.min(ix)..=anchor.max(ix);
            // Ctrl+shift stacks the range onto the set.
            if modifiers.secondary() {
                self.selected.extend(range);
            } else {
                self.selected = range.collect();
            }
            if self.anchor.is_none() {
                self.anchor = Some(ix);
            }
        } else if modifiers.secondary() {
            if !self.selected.insert(ix) {
                self.selected.remove(&ix);
            }
            self.anchor = Some(ix);
        } else {
            self.selected = HashSet::from([ix]);
            self.anchor = Some(ix);
        }

        self.cursor = Some(ix);
        self.publish_selection(cx);
        cx.notify();
    }

    /// Every call publishes, an empty set included, so a pick here replaces
    /// one made elsewhere. Only tracks that are rows have an id to publish.
    fn publish_selection(&self, cx: &mut Context<Self>) {
        let ids: Vec<i64> = self
            .selected_rows()
            .into_iter()
            .filter_map(|ix| self.track_at(ix))
            .filter_map(|track| self.ids.get(&track.key).copied())
            .collect();
        let source = cx.entity_id();

        self.state
            .selection
            .update(cx, |selection, cx| selection.set(ids, source, cx));
    }

    fn step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let len = self.listing.entries.len();
        if len == 0 {
            return;
        }

        let target = match self.cursor {
            Some(ix) => ix.saturating_add_signed(delta).min(len - 1),
            None => 0,
        };

        self.select(
            target,
            Modifiers {
                shift: extend,
                ..Modifiers::default()
            },
            cx,
        );
        self.scroll.scroll_to_item(target, ScrollStrategy::Center);
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // The search box's own keys bubble up through the panel; its Enter
        // searches and must not play the selection too.
        if self.search.read(cx).is_focused(window, cx) {
            return;
        }

        let modifiers = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();

        if modifiers.secondary() && key == "a" {
            self.selected = (0..self.listing.entries.len()).collect();
            self.anchor = Some(0);
            self.cursor = Some(0);
            self.publish_selection(cx);
            cx.notify();
            return;
        }

        match key {
            "up" => self.step(-1, modifiers.shift, cx),

            "down" => self.step(1, modifiers.shift, cx),

            // One node opens; any tracks in the set play, nodes skipped.
            "enter" => {
                let rows = self.selected_rows();
                match rows.as_slice() {
                    [ix] => self.activate(*ix, cx),

                    rows => {
                        let tracks = self.tracks_at(rows);
                        if !tracks.is_empty() {
                            self.play(tracks, 0, cx);
                        }
                    }
                }
            }

            "escape" if !self.selected.is_empty() => {
                self.clear_marks();
                self.publish_selection(cx);
                cx.notify();
            }

            _ => {}
        }
    }

    fn body(&mut self, cx: &mut Context<Self>) -> Div {
        let root =
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(palette::bg_root())
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.on_key(event, window, cx)
                }));

        if self.source().is_empty() {
            return root.child(self.picker(cx));
        }

        let banners = [
            self.listing.error.clone().map(|reason| {
                (
                    rox_i18n::t!("source-browser-failed", source = self.label.to_string()),
                    reason,
                )
            }),
            self.failure.clone(),
        ];

        root.child(self.header(cx))
            .children(self.breadcrumb(cx))
            .children(banners.into_iter().flatten().map(|(headline, reason)| {
                div().flex_none().p(tokens::SPACE_SM).child(panel::banner(
                    Tone::Bad,
                    headline,
                    vec![reason.into()],
                ))
            }))
            // One menu over the whole list. A menu per row shares one element
            // state across the rows and never opens.
            .child(self.list(cx).context_menu({
                let weak = cx.entity().downgrade();
                move |menu, window, cx| {
                    let Some(this) = weak.upgrade() else {
                        return menu;
                    };
                    this.update(cx, |this, cx| this.row_menu(menu, window, cx))
                }
            }))
    }

    fn header(&mut self, cx: &mut Context<Self>) -> Div {
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(tokens::SPACE_SM)
            .px(tokens::SPACE_SM)
            .py(tokens::SPACE_XS)
            .border_b_1()
            .border_color(palette::border())
            .child(
                div()
                    .flex_none()
                    .max_w(px(200.))
                    .truncate()
                    .text_sm()
                    .text_color(palette::text_bright())
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.search.update(cx, |search, cx| search.element(cx))),
            )
            .when(self.listing.loading(), |header| {
                header.child(Spinner::new().xsmall().color(palette::accent().into()))
            })
    }

    /// None at the roots, where there's nothing to climb back to.
    fn breadcrumb(&self, cx: &mut Context<Self>) -> Option<Div> {
        let place = &self.listing.place;
        if place.is_root() {
            return None;
        }

        let mut crumbs: Vec<(Option<usize>, SharedString)> =
            vec![(Some(0), rox_i18n::t!("source-browser-home"))];

        match &place.query {
            Some(query) => crumbs.push((
                None,
                rox_i18n::t!("source-browser-results", query = query.clone()),
            )),

            None => crumbs.extend(
                place
                    .trail
                    .iter()
                    .enumerate()
                    .map(|(ix, crumb)| (Some(ix + 1), SharedString::from(crumb.title.clone()))),
            ),
        }

        let last = crumbs.len() - 1;
        let row = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(tokens::SPACE_XS)
            .px(tokens::SPACE_SM)
            .py(tokens::SPACE_XS)
            .min_w_0()
            .overflow_hidden()
            .text_xs()
            .border_b_1()
            .border_color(palette::border());

        let row = crumbs
            .into_iter()
            .enumerate()
            .fold(row, |row, (ix, (depth, title))| {
                let crumb = div()
                    .id(("source-crumb", ix))
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .text_color(match ix == last {
                        true => palette::text(),
                        false => palette::text_muted(),
                    })
                    .child(title);

                let crumb = match depth {
                    Some(depth) => crumb
                        .cursor_pointer()
                        .hover(|crumb| crumb.text_color(palette::text_bright()))
                        .on_click(cx.listener(move |this, _, _, cx| this.go_up(depth, cx))),

                    None => crumb,
                };

                let row = match ix {
                    0 => row,
                    _ => row.child(
                        svg()
                            .flex_none()
                            .path(icons::CHEVRON_RIGHT)
                            .size(px(12.))
                            .text_color(palette::text_faint()),
                    ),
                };
                row.child(crumb)
            });

        Some(row)
    }

    fn list(&mut self, cx: &mut Context<Self>) -> Div {
        let count = self.listing.entries.len();

        let list = if count == 0 {
            self.empty_state()
        } else {
            let this = cx.entity().downgrade();
            div().flex_1().min_h_0().w_full().flex().flex_col().child(
                uniform_list("source-rows", count, move |range, _, cx| {
                    this.upgrade()
                        .map(|this| this.update(cx, |this, cx| this.rows(range, cx)))
                        .unwrap_or_default()
                })
                .track_scroll(self.scroll.clone())
                .flex_1()
                .w_full(),
            )
        };

        // A right press clears the target on the way down; a row's handler
        // runs after and sets it back.
        list.capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
            if event.button == MouseButton::Right {
                this.menu_row = None;
            }
        }))
    }

    /// The visible rows. Nearing the end of what's listed asks for the next
    /// page, so a list shorter than the panel keeps paging until it fills.
    fn rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if range.end + PAGE_AHEAD >= self.listing.entries.len() {
            self.load_more(cx);
        }

        range
            .filter_map(|ix| {
                let entry = self.listing.entries.get(ix)?.clone();
                Some(match entry {
                    Entry::Node {
                        id,
                        title,
                        subtitle,
                        collection,
                    } => self.node_row(ix, id, title, subtitle, collection, cx),

                    Entry::Track(track) => self.track_row(ix, &track, cx),
                })
            })
            .collect()
    }

    fn row_shell(&self, ix: usize, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let selected = self.selected.contains(&ix);

        div()
            .id(("source-row", ix))
            .h(ROW_H)
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(tokens::SPACE_SM)
            .px(tokens::SPACE_SM)
            .cursor_pointer()
            .when(selected, |row| {
                row.bg(palette::alpha(palette::accent(), 0x26))
            })
            .hover(|row| row.bg(palette::bg_control_hover()))
            // The press picks, not the click, so the highlight is already
            // right when a right press opens the menu.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    if event.click_count == 1 {
                        this.select(ix, event.modifiers, cx);
                    }
                }),
            )
            // The press keeps going so the list's handler sees it. A press
            // outside the set picks just that row, so the menu never acts on
            // unseen picks.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.menu_row = Some(ix);
                    if !this.selected.contains(&ix) {
                        window.focus(&this.focus);
                        this.select(ix, Modifiers::default(), cx);
                    }
                }),
            )
    }

    /// `busy` spins beside the title while a pick or an open runs, the
    /// library row's way.
    fn two_lines(title: SharedString, second: SharedString, playing: bool, busy: bool) -> Div {
        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .min_w_0()
            .when(busy, |line| {
                line.child(Spinner::new().xsmall().color(palette::accent().into()))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(match playing {
                        true => palette::accent(),
                        false => palette::text(),
                    })
                    .child(title),
            );

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(1.))
            .child(title)
            .when(!second.is_empty(), |lines| {
                lines.child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(palette::text_muted())
                        .child(second),
                )
            })
    }

    fn glyph(path: &'static str) -> AnyElement {
        svg()
            .flex_none()
            .path(path)
            .size(px(16.))
            .text_color(palette::text_faint())
            .into_any_element()
    }

    fn node_row(
        &self,
        ix: usize,
        id: String,
        title: String,
        subtitle: String,
        collection: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let synced = self.is_synced(&id);

        // Plugin text renders as it came, never through the translator.
        let second = match (synced, self.members.get(&id)) {
            (true, Some(&count)) if !self.syncing.contains(&id) => {
                rox_i18n::t!("source-browser-members", count = count as u64)
            }
            _ => SharedString::from(subtitle),
        };

        let switch = collection.then(|| {
            let (node, name) = (id.clone(), title.clone());
            let control = match self.syncing.contains(&id) {
                true => Spinner::new()
                    .xsmall()
                    .color(palette::accent().into())
                    .into_any_element(),

                false => panel::toggle(
                    synced,
                    move |this: &mut Self, on, cx| {
                        this.set_synced(node.clone(), name.clone(), on, cx)
                    },
                    cx,
                )
                .into_any_element(),
            };

            // The switch's press stops here, or it would open the row too.
            div()
                .id(("source-sync", ix))
                .flex_none()
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new(rox_i18n::t!("source-browser-sync"))
                        .build(window, cx)
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(control)
        });

        self.row_shell(ix, cx)
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.activate(ix, cx)))
            .child(Self::glyph(match collection {
                true => icons::LIST_MUSIC,
                false => icons::FOLDER,
            }))
            .child(Self::two_lines(title.into(), second, false, false))
            .children(switch)
            .child(Self::glyph(icons::CHEVRON_RIGHT))
            .into_any_element()
    }

    fn track_row(&self, ix: usize, track: &PluginTrack, cx: &mut Context<Self>) -> AnyElement {
        let key = self.key_for(&track.key);
        let playing = self.playing.as_ref() == Some(&key);
        let busy = self.picking.contains(&track.key) || self.opening.as_ref() == Some(&key);

        let duration = match track.duration_ms {
            0 => SharedString::default(),
            ms => fmt_time(f64::from(ms) / 1000.0).into(),
        };

        self.row_shell(ix, cx)
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.activate(ix, cx);
                }
            }))
            .child(self.art(&track.key, cx))
            .child(Self::two_lines(
                track.title.clone().into(),
                track.artist.clone().into(),
                playing,
                busy,
            ))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(palette::text_muted())
                    .child(duration),
            )
            .into_any_element()
    }

    /// Fetched through the plugin until it's stored, so a track that isn't a
    /// library row yet still shows its cover.
    fn art(&self, key: &str, cx: &mut Context<Self>) -> AnyElement {
        let source = self.config.source.clone();
        let thumb = self
            .state
            .thumbs
            .update(cx, |thumbs, cx| thumbs.get_plugin(&source, key, cx));

        match thumb {
            Thumb::Ready(image) => img(image)
                .flex_none()
                .size(ART)
                .overflow_hidden()
                .object_fit(ObjectFit::Cover)
                .rounded(tokens::RADIUS)
                .into_any_element(),

            _ => div()
                .flex_none()
                .size(ART)
                .flex()
                .items_center()
                .justify_center()
                .rounded(tokens::RADIUS)
                .bg(palette::bg_control())
                .child(Self::glyph(icons::MUSIC))
                .into_any_element(),
        }
    }

    fn empty_state(&self) -> Div {
        let text = match self.listing.loading() {
            true => SharedString::default(),
            false => rox_i18n::t!("source-browser-empty"),
        };

        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p(tokens::SPACE_MD)
            .text_sm()
            .text_color(palette::text_muted())
            .child(text)
    }

    /// Read when drawn: the running sources are a lock and a small map.
    fn picker(&self, cx: &mut Context<Self>) -> Div {
        let sources = plugins::live_sources();

        let body = div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(tokens::SPACE_SM)
            .p(tokens::SPACE_MD)
            .text_center()
            .child(div().text_lg().child(rox_i18n::t!("source-browser-pick")));

        if sources.is_empty() {
            return body.child(
                div()
                    .text_sm()
                    .text_color(palette::text_muted())
                    .child(rox_i18n::t!("source-browser-no-sources")),
            );
        }

        body.children(sources.into_iter().map(|(source, label)| {
            let chosen = source.clone();
            crate::settings::ui::small_button(
                SharedString::from(label),
                icons::GLOBE,
                false,
                cx.listener(move |this, _, _, cx| this.choose(chosen.clone(), cx)),
            )
        }))
    }

    fn row_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let Some(row) = self.menu_row else {
            return self.dropdown_menu(menu, window, cx);
        };

        let rows = match self.selected.contains(&row) {
            true => self.selected_rows(),
            false => vec![row],
        };
        let tracks = self.tracks_at(&rows);
        if tracks.is_empty() {
            return self.dropdown_menu(menu, window, cx);
        }

        let play_label = match tracks.len() {
            1 => rox_i18n::t!("source-browser-play"),
            n => rox_i18n::t!("source-browser-play-count", count = n as u64),
        };

        // Rows already in the library get the whole shared menu. Anything
        // else gets the actions a pick can serve first.
        let ids: Option<Vec<i64>> = tracks
            .iter()
            .map(|track| self.ids.get(&track.key).copied())
            .collect();

        // One row plays on through the list like a double click; a set plays
        // as a run of its own.
        let single = (rows.len() == 1).then_some(row);
        let (panel, play_tracks) = (cx.entity().downgrade(), tracks.clone());
        let on_play = move |_: &mut Window, cx: &mut App| {
            let tracks = play_tracks.clone();
            panel
                .update(cx, |this, cx| match single {
                    Some(ix) => this.play_from(ix, cx),
                    None => this.play(tracks, 0, cx),
                })
                .ok();
        };

        let menu = match ids {
            Some(ids) => panel::track_actions(
                menu,
                self.state.clone(),
                ids,
                play_label,
                window,
                cx,
                on_play,
            ),

            None => self.unpicked_menu(menu, tracks, play_label, on_play, window, cx),
        };

        self.dropdown_menu(menu.separator(), window, cx)
    }

    fn unpicked_menu(
        &self,
        menu: PopupMenu,
        tracks: Vec<PluginTrack>,
        play_label: SharedString,
        on_play: impl Fn(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let panel = cx.entity().downgrade();

        let item = |label: SharedString,
                    icon: &'static str,
                    act: fn(&mut Self, Vec<PluginTrack>, &mut Context<Self>)| {
            let (panel, tracks) = (panel.clone(), tracks.clone());
            PopupMenuItem::new(label)
                .icon(Icon::default().path(icon))
                .on_click(move |_, _, cx| {
                    let tracks = tracks.clone();
                    panel.update(cx, |this, cx| act(this, tracks, cx)).ok();
                })
        };

        let menu = menu
            .item(
                PopupMenuItem::new(play_label)
                    .icon(Icon::default().path(icons::PLAY))
                    .on_click(move |_, window, cx| on_play(window, cx)),
            )
            .item(item(
                rox_i18n::t!("panel-play-next"),
                icons::SKIP_FORWARD,
                |this, tracks, cx| this.queue(tracks, true, cx),
            ))
            .item(item(
                rox_i18n::t!("panel-add-to-queue"),
                icons::LIST_MUSIC,
                |this, tracks, cx| this.queue(tracks, false, cx),
            ));

        let with_ids: WithIds = Rc::new(move |then: Then, cx: &mut App| {
            let tracks = tracks.clone();
            panel
                .update(cx, |this, cx| {
                    let library = this.state.library.clone();
                    this.with_picked(
                        tracks,
                        move |keys, cx| {
                            let ids = {
                                let library = library.read(cx);
                                keys.iter()
                                    .filter_map(|key| library.id_for_key(key))
                                    .collect()
                            };
                            then(ids, cx);
                        },
                        cx,
                    );
                })
                .ok();
        });

        panel::playlist_item_deferred(menu, self.state.clone(), with_ids, window, cx)
    }
}

impl PanelSettings for SourceBrowserPanel {
    fn state(&self) -> AppState {
        self.state.clone()
    }

    fn chrome(&self) -> &PanelChrome {
        &self.config.chrome
    }

    fn chrome_mut(&mut self) -> &mut PanelChrome {
        &mut self.config.chrome
    }

    fn set_custom_title(&mut self, title: Option<String>, cx: &mut Context<Self>) {
        self.config.chrome.title = title;
        panel::refresh_tab_panel(&self.tab_panel, cx);
        cx.notify();
    }
}

impl EventEmitter<PanelEvent> for SourceBrowserPanel {}

impl Focusable for SourceBrowserPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for SourceBrowserPanel {
    fn panel_name(&self) -> &'static str {
        "source browser"
    }

    rox_panel_api::opens_settings!();

    /// The source's own label once one is pinned.
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let default = match self.label.is_empty() {
            true => rox_i18n::t!("source-browser-title"),
            false => self.label.clone(),
        };

        panel::title_text(self.config.chrome.title.as_deref(), default)
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        self.config.chrome.title.clone().map(SharedString::from)
    }

    fn locked(&self, _cx: &App) -> bool {
        self.config.chrome.locked
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }

    fn content_context_menu(&self, _cx: &App) -> bool {
        true
    }

    fn min_size(&self, _cx: &App) -> gpui::Size<gpui::Pixels> {
        crate::panel::chrome_min_size(
            &self.config.chrome,
            gpui::size(
                rox_dock::resizable::PANEL_MIN_SIZE,
                rox_dock::resizable::PANEL_MIN_SIZE,
            ),
        )
    }

    fn max_size(&self, cx: &App) -> gpui::Size<gpui::Pixels> {
        crate::panel::chrome_max_size(&self.config.chrome, self.min_size(cx))
    }

    fn dump(&self, _cx: &App) -> rox_dock::PanelState {
        let mut state = rox_dock::PanelState::new(self);
        state.info = rox_dock::PanelInfo::panel(
            serde_json::to_value(self.config.clone()).unwrap_or(serde_json::Value::Null),
        );
        state
    }

    fn on_added_to(
        &mut self,
        tab_panel: WeakEntity<TabPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_panel = Some(tab_panel.clone());
        self.state
            .tab_hosts
            .update(cx, |hosts, _| hosts.report(tab_panel));
    }

    fn on_removed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.tab_panel = None;
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let menu =
            panel_settings::rename_item(menu, &cx.entity(), self.tab_panel.clone(), window, cx);
        let menu = panel_settings::settings_item(menu, &cx.entity(), cx);
        let menu = panel::duplicate_item(
            menu,
            &cx.entity(),
            self.tab_panel.clone(),
            |this, window, cx| {
                let (state, config) = {
                    let panel = this.read(cx);
                    (panel.state.clone(), panel.config.clone())
                };
                SourceBrowserPanel::new(state, config, window, cx)
            },
        );
        panel::popout_item(
            menu,
            &cx.entity(),
            self.tab_panel.clone(),
            self.state.clone(),
            window,
        )
    }
}

impl gpui::Render for SourceBrowserPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chrome = self.config.chrome.clone();
        panel::themed(&chrome, || self.body(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> Entry {
        Entry::Node {
            id: id.to_string(),
            title: id.to_string(),
            subtitle: String::new(),
            collection: false,
        }
    }

    fn page(ids: &[&str], cursor: Option<&str>) -> Page {
        Page {
            entries: ids.iter().map(|id| node(id)).collect(),
            cursor: cursor.map(str::to_string),
        }
    }

    fn at(id: &str) -> Place {
        Place {
            trail: vec![Crumb {
                id: id.to_string(),
                title: id.to_string(),
            }],
            query: None,
        }
    }

    fn ids(listing: &Listing) -> Vec<String> {
        listing
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Node { id, .. } => id.clone(),
                Entry::Track(track) => track.key.clone(),
            })
            .collect()
    }

    #[test]
    fn a_second_page_appends() {
        let mut listing = Listing::default();

        let first = listing.begin(Some(at("liked")));
        assert_eq!(
            listing.land(first, Ok(page(&["a", "b"], Some("2")))),
            Landed::Replaced { moved: true }
        );
        assert!(listing.wants_more());

        let second = listing.begin(None);
        assert!(!listing.wants_more(), "one page in flight at a time");
        assert_eq!(
            listing.land(second, Ok(page(&["c"], None))),
            Landed::Appended
        );

        assert_eq!(ids(&listing), ["a", "b", "c"]);
        assert!(!listing.wants_more(), "a null cursor is the last page");
    }

    #[test]
    fn a_search_resets_the_list() {
        let mut listing = Listing::default();
        let first = listing.begin(Some(at("liked")));
        listing.land(first, Ok(page(&["a", "b"], Some("2"))));

        let search = Place {
            trail: Vec::new(),
            query: Some("tones".into()),
        };
        let asked = listing.begin(Some(search.clone()));
        assert_eq!(
            listing.land(asked, Ok(page(&["x"], None))),
            Landed::Replaced { moved: true }
        );

        assert_eq!(ids(&listing), ["x"]);
        assert_eq!(listing.place, search);
        assert_eq!(listing.cursor, None);
    }

    #[test]
    fn a_failed_page_keeps_the_list() {
        let mut listing = Listing::default();
        let first = listing.begin(Some(at("liked")));
        listing.land(first, Ok(page(&["a", "b"], Some("2"))));

        let second = listing.begin(None);
        assert_eq!(
            listing.land(second, Err("the plugin timed out".into())),
            Landed::Failed
        );

        assert_eq!(ids(&listing), ["a", "b"]);
        assert_eq!(listing.error.as_deref(), Some("the plugin timed out"));
        assert_eq!(listing.cursor.as_deref(), Some("2"), "kept for a retry");
        assert!(!listing.wants_more(), "a failure doesn't retry on its own");

        // Opening somewhere else fails too, and the list still stands.
        let away = listing.begin(Some(at("mixes")));
        listing.land(away, Err("no plugin host".into()));
        assert_eq!(ids(&listing), ["a", "b"]);
        assert_eq!(listing.place, at("liked"));
    }

    #[test]
    fn an_answer_for_a_replaced_request_is_dropped() {
        let mut listing = Listing::default();
        let slow = listing.begin(Some(at("liked")));
        let fast = listing.begin(Some(at("mixes")));

        assert_eq!(
            listing.land(fast, Ok(page(&["m"], None))),
            Landed::Replaced { moved: true }
        );
        assert_eq!(listing.land(slow, Ok(page(&["l"], None))), Landed::Stale);
        assert_eq!(ids(&listing), ["m"]);
    }

    #[test]
    fn rereading_a_place_is_not_a_move() {
        let mut listing = Listing::default();
        let first = listing.begin(Some(at("liked")));
        listing.land(first, Ok(page(&["a"], None)));

        let again = listing.begin(Some(at("liked")));
        assert_eq!(
            listing.land(again, Ok(page(&["a", "b"], None))),
            Landed::Replaced { moved: false }
        );
    }

    #[test]
    fn a_play_runs_on_from_the_clicked_track() {
        assert_eq!(
            play_window(20, 3, 1000),
            (0..20, 3),
            "a short list plays whole"
        );

        // Capped: half behind for Prev, the rest ahead.
        assert_eq!(play_window(100, 50, 10), (45..55, 5));

        // Near an end, the short side's share goes to the other.
        assert_eq!(play_window(100, 2, 10), (0..10, 2));
        assert_eq!(play_window(100, 98, 10), (90..100, 8));
    }

    #[test]
    fn the_config_keeps_its_source_through_a_dump() {
        let config = SourceBrowserConfig {
            chrome: PanelChrome {
                title: Some("Mine".into()),
                ..PanelChrome::default()
            },
            source: "plugin:example-tones".into(),
        };

        let info = rox_dock::PanelInfo::panel(serde_json::to_value(&config).unwrap());
        let back: SourceBrowserConfig = panel::config_from_info(&info);

        assert_eq!(back.source, "plugin:example-tones");
        assert_eq!(back.chrome.title.as_deref(), Some("Mine"));
    }

    #[test]
    fn a_layout_without_a_source_opens_the_picker() {
        let info = rox_dock::PanelInfo::panel(serde_json::json!({}));
        let back: SourceBrowserConfig = panel::config_from_info(&info);

        assert!(back.source.is_empty());
    }

    #[test]
    fn a_node_under_search_results_starts_its_own_trail() {
        let place = Place {
            trail: vec![Crumb {
                id: "liked".into(),
                title: "Liked".into(),
            }],
            query: Some("tones".into()),
        };

        assert_eq!(place.node(), None, "search results list no node");
        assert!(!place.is_root());
        assert!(Place::default().is_root());
    }
}
