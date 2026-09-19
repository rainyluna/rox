//! The station directory: radio-browser.info in a window of its own.
//! Finding a station and keeping one are two different jobs. The stations
//! panel holds the list you already have, and this is where that list
//! grows from, a box with a few hundred thousand community-listed streams
//! behind it.
//!
//! A window rather than a strip above the panel, because a search wants
//! the room. Fifty hits carrying a name, a country and a codec each read
//! as a list of their own, and while they're up the stations you keep are
//! nowhere in the question. One window at a time: asking again raises the
//! one that's open, the way every other page here behaves.
//!
//! A hit lands through [`rox_library::stations::put`], the same write a
//! typed URL takes, so the list never learns which way a row arrived. Add
//! and play does that write and then hands the key to the player, which
//! opens a station the way it opens a file. The one extra thing this
//! window does is fetch the hit's favicon and file it in the thumbs
//! database under the station's URL, so a station shows up carrying its
//! logo rather than a blank tile. That fetch is best effort and silent:
//! the station is already added by the time it runs, and a directory full
//! of dead logo links shouldn't read as a failed add.
//!
//! Nothing here removes or edits. The directory is somebody else's
//! database and this window only reads it; a station you already keep is
//! the panel's business.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, FocusHandle, Focusable as _, Global,
    KeyDownEvent, SharedString, Subscription, Window, WindowHandle, div, prelude::*, px, size,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, Root, Sizable as _};

use rox_design::assets::icons;
use rox_design::{palette, tokens};
use rox_library::cue::{TrackKey, source_id};
use rox_library::stations::{self, Station};
use rox_net::sources::radio_browser::{self, Found};
use rox_panel_api::panel::{self, AppState};
use rox_services::catalog::LibraryEvent;

/// How many directory hits one search shows. The directory ranks by votes,
/// so the head of the list is the well-known stations and a page past
/// that is noise.
const RESULT_LIMIT: usize = 50;

/// The window's opening size: tall rather than wide, since a hit is two
/// short lines and what you want is more of them on screen at once.
const DEFAULT_SIZE: (f32, f32) = (560., 680.);

/// The narrowest the window goes before the rows are all truncation: the
/// name line, the two actions beside it, and the search box above.
const MIN: gpui::Size<gpui::Pixels> = gpui::Size {
    width: px(420.),
    height: px(320.),
};

/// The open directory window, if any: opening again raises it rather than
/// stacking a second one, the health window's move.
struct OpenDirectory(WindowHandle<Root>);

impl Global for OpenDirectory {}

/// Open the station directory, or bring the open one to the front.
pub fn open(state: AppState, cx: &mut App) {
    if let Some(open) = cx.try_global::<OpenDirectory>() {
        let handle = open.0;
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            return;
        }
    }

    let bounds = Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx);
    let handle = rox_panel_api::panel::open_child_window(
        cx,
        rox_i18n::t!("directory-window-title"),
        bounds,
        Some(MIN),
        move |window, cx| cx.new(|cx| StationDirectory::new(state, window, cx)),
    );

    cx.set_global(OpenDirectory(handle));
}

struct StationDirectory {
    state: AppState,
    /// The search box. It takes focus when the window opens, since there
    /// is nothing else to do in here first.
    find: Entity<InputState>,
    found: Vec<Found>,
    /// The text the results answer, so an empty result can name it. None
    /// means no search has come back yet.
    found_for: Option<String>,
    searching: bool,
    /// Bumped per search, so a slow reply from an earlier one can't land
    /// over a newer one.
    search_generation: u64,
    /// Why the last search showed nothing, when the reason was the
    /// directory rather than the query.
    failed: Option<SharedString>,
    /// The URLs already in the station list, so a hit shows a check
    /// instead of inviting the same stream in twice.
    held: HashSet<String>,
    focus: FocusHandle,
    _library_changed: Subscription,
    _find_events: Subscription,
}

impl StationDirectory {
    fn new(state: AppState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let find = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rox_i18n::t!("directory-placeholder"))
        });
        window.focus(&find.focus_handle(cx));

        let _find_events = cx.subscribe_in(
            &find,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.search(cx);
                }
            },
        );

        // A station added or removed anywhere else moves the checks in
        // this list, the same reason every library-backed view re-reads on
        // this.
        let _library_changed = cx.subscribe(
            &state.library,
            |this: &mut Self, _, event: &LibraryEvent, cx| {
                if matches!(event, LibraryEvent::Updated) {
                    this.refresh(cx);
                }
            },
        );

        let mut this = StationDirectory {
            state,
            find,
            found: Vec::new(),
            found_for: None,
            searching: false,
            search_generation: 0,
            failed: None,
            held: HashSet::new(),
            focus: cx.focus_handle(),
            _library_changed,
            _find_events,
        };
        this.refresh(cx);
        this
    }

    /// Re-read which stations are already kept. At open and edit cadence,
    /// never per frame.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.held = self
            .open_db(cx)
            .and_then(|conn| stations::all(&conn).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|station| station.url)
            .collect();

        cx.notify();
    }

    /// This window's own connection to the library database, opened per
    /// call rather than held: an add happens at human pace, and a held
    /// connection would sit through every scan.
    fn open_db(&self, cx: &App) -> Option<rox_library::rusqlite::Connection> {
        let path = self.state.library.read(cx).db_path();

        rox_library::store::open(&path).ok()
    }

    /// Ask the directory what matches the box. The call blocks, so it goes
    /// to the background executor and the reply comes back through the
    /// generation stamp.
    fn search(&mut self, cx: &mut Context<Self>) {
        let text = self.find.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }

        self.search_generation += 1;
        let generation = self.search_generation;
        self.searching = true;
        self.failed = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let query = text.clone();
            let result = cx
                .background_executor()
                .spawn(async move { radio_browser::search(&query, RESULT_LIMIT) })
                .await;

            this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }

                this.searching = false;
                match result {
                    Ok(found) => {
                        this.found = found;
                        this.found_for = Some(text);
                    }
                    // A directory that's down says so where the hits would
                    // have been. There's nothing else to show.
                    Err(reason) => {
                        this.found.clear();
                        this.found_for = None;
                        this.failed = Some(rox_i18n::t!("directory-failed", reason = reason));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// One hit into the station list, and onto the deck when `play` is
    /// set. The results stay up, so a search can be added from more than
    /// once.
    fn add(&mut self, ix: usize, play: bool, cx: &mut Context<Self>) {
        let Some(hit) = self.found.get(ix).cloned() else {
            return;
        };

        let station = station_from(&hit);
        if !self.write(&station, cx) {
            return;
        }

        self.cache_favicon(&hit, cx);

        if play {
            let key = key_for(&station.url);
            self.state
                .player
                .update(cx, |player, cx| player.play_now(vec![key], cx));
        }
    }

    /// Write the station, then have the library rebuild its projection so
    /// the row shows up everywhere else too, not only in the panel's list.
    fn write(&mut self, station: &Station, cx: &mut Context<Self>) -> bool {
        let Some(mut conn) = self.open_db(cx) else {
            return false;
        };
        if let Err(e) = stations::put(&mut conn, std::slice::from_ref(station)) {
            log::warn!("station directory: writing {} failed: {e}", station.url);
            return false;
        }

        self.state
            .library
            .update(cx, |library, cx| library.reload_projection(cx));
        self.refresh(cx);
        true
    }

    /// Fetch the hit's logo and file it under the station's URL, which is
    /// the row's path and so the key everything else asks art by. Off the
    /// UI thread, and quiet either way: a station with a dead favicon link
    /// is still a station that was added.
    fn cache_favicon(&self, hit: &Found, cx: &mut Context<Self>) {
        let url = hit.favicon.trim().to_string();
        if url.is_empty() {
            return;
        }

        let Some(conn) = self.state.thumbs.read(cx).store_conn() else {
            return;
        };
        let key = hit.url.clone();
        let thumbs = self.state.thumbs.clone();

        // Back to the main thread once the logo is filed, because the row
        // for this station may already be on screen: it painted before the
        // fetch finished, was told there was no art, and that answer is
        // cached as definitive. Without the nudge the tile stays blank
        // until something else invalidates the whole cache.
        cx.spawn(async move |_, cx| {
            let stored = cx
                .background_executor()
                .spawn(async move {
                    // Only a store that took the image is worth waking
                    // the cache for.
                    rox_services::station_art::fetch_and_store(&url, &key, &conn).then_some(key)
                })
                .await;

            let Some(key) = stored else {
                return;
            };

            thumbs
                .update(cx, |thumbs, cx| {
                    thumbs.forget(std::path::Path::new(&key), cx);
                })
                .ok();
        })
        .detach();
    }

    /// The box and its button. Enter does the same thing the button does.
    fn search_row(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(tokens::SPACE_XS)
            .p(tokens::SPACE_SM)
            .border_b_1()
            .border_color(palette::border())
            .child(div().flex_1().min_w_0().child(Input::new(&self.find)))
            .child(
                Button::new("directory-search")
                    .icon(Icon::default().path(icons::SEARCH))
                    .tooltip(rox_i18n::t!("directory-search"))
                    .on_click(cx.listener(|this, _, _, cx| this.search(cx))),
            )
    }

    /// What the last search turned up: a wait line, the reason it failed,
    /// a no-match line naming the text, or the hits themselves. Before the
    /// first search there's nothing to say that the box's own placeholder
    /// doesn't already.
    fn results(&self, cx: &mut Context<Self>) -> Div {
        let centered = |line: SharedString| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(tokens::SPACE_MD)
                .text_sm()
                .text_center()
                .text_color(palette::text_muted())
                .child(line)
        };

        if self.searching {
            return centered(rox_i18n::t!("directory-searching"));
        }

        if let Some(failed) = self.failed.clone() {
            return centered(failed);
        }

        if self.found.is_empty() {
            let Some(text) = self.found_for.clone() else {
                return div().flex_1();
            };

            return centered(rox_i18n::t!("directory-none", text = text));
        }

        let rows: Vec<AnyElement> = self
            .found
            .iter()
            .enumerate()
            .map(|(ix, hit)| self.result_row(ix, hit, cx))
            .collect();

        div().flex_1().min_h_0().w_full().flex().flex_col().child(
            div()
                .id("directory-results")
                .size_full()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(rows),
        )
    }

    /// One hit: its name over what the directory knows about the stream,
    /// and the two ways to take it. A station already kept says so instead
    /// of offering the add again.
    fn result_row(&self, ix: usize, hit: &Found, cx: &mut Context<Self>) -> AnyElement {
        let actions: AnyElement = if self.held.contains(&hit.url) {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(tokens::SPACE_XS)
                .text_xs()
                .text_color(palette::text_muted())
                .child(Icon::default().path(icons::CHECK))
                .child(rox_i18n::t!("directory-added"))
                .into_any_element()
        } else {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(tokens::SPACE_XS)
                .child(
                    Button::new(("directory-add", ix))
                        .icon(Icon::default().path(icons::PLUS))
                        .label(rox_i18n::t!("directory-add"))
                        .small()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| this.add(ix, false, cx))),
                )
                .child(
                    Button::new(("directory-add-play", ix))
                        .icon(Icon::default().path(icons::PLAY))
                        .label(rox_i18n::t!("directory-add-play"))
                        .small()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| this.add(ix, true, cx))),
                )
                .into_any_element()
        };

        div()
            .id(("directory-hit", ix))
            .flex()
            .items_center()
            .gap(tokens::SPACE_SM)
            .px(tokens::SPACE_SM)
            .py(tokens::SPACE_XS)
            .w_full()
            .min_w_0()
            .hover(|row| row.bg(palette::bg_control_hover()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(palette::text())
                            .child(SharedString::from(hit.name.clone())),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(palette::text_muted())
                            .child(SharedString::from(meta_line(hit))),
                    ),
            )
            .child(actions)
            .into_any_element()
    }
}

impl Render for StationDirectory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Tinted by the playing track and claiming the widget theme while
        // it holds focus, like every other page that opens beside the
        // workspace.
        let player = self.state.player.entity_id();
        palette::note_focus(player, window.is_window_active(), cx);

        panel::window_body(player, || {
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(palette::bg_elevated())
                .text_color(palette::text_bright())
                .text_sm()
                .track_focus(&self.focus)
                // The box passes an idle escape through, so this catches
                // it wherever focus happens to be and closes the window.
                .on_key_down(cx.listener(|_, event: &KeyDownEvent, window, _| {
                    if event.keystroke.key == "escape" {
                        window.remove_window();
                    }
                }))
                .child(self.search_row(cx))
                .child(self.results(cx))
                .into_any_element()
        })
    }
}

/// A directory hit as the station list stores it. The first tag becomes
/// the genre, which is the one the directory puts first and the only one
/// a row has a column for.
fn station_from(hit: &Found) -> Station {
    Station {
        url: hit.url.clone(),
        name: hit.name.clone(),
        genre: hit
            .tags
            .split(',')
            .map(str::trim)
            .find(|tag| !tag.is_empty())
            .unwrap_or_default()
            .to_string(),
    }
}

/// The line under a hit's name: country, codec and bitrate, whichever of
/// them the directory knows.
fn meta_line(hit: &Found) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !hit.country.is_empty() {
        parts.push(hit.country.clone());
    }
    if !hit.codec.is_empty() {
        parts.push(hit.codec.clone());
    }
    if hit.bitrate_kbps > 0 {
        parts.push(rox_i18n::t!("directory-bitrate", kbps = hit.bitrate_kbps).to_string());
    }

    parts.join(", ")
}

/// The key a station plays under: the radio source, the stream URL as the
/// path, and no subsong. The same shape [`rox_library::stations`] writes
/// its rows with, which is what makes the resolve find them.
fn key_for(url: &str) -> TrackKey {
    TrackKey {
        source: source_id(stations::SOURCE),
        path: url.into(),
        sub: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(tags: &str, country: &str, codec: &str, kbps: u16) -> Found {
        Found {
            name: "Deep Space One".into(),
            url: "https://ice2.somafm.com/deepspaceone-128-aac".into(),
            tags: tags.into(),
            country: country.into(),
            codec: codec.into(),
            bitrate_kbps: kbps,
            ..Found::default()
        }
    }

    /// The first tag is the genre; a hit with no tags lands with none, the
    /// same as a typed station.
    #[test]
    fn a_hit_becomes_a_station_with_its_first_tag_as_genre() {
        let station = station_from(&hit(" ambient, space, drone", "US", "AAC", 128));

        assert_eq!(station.url, "https://ice2.somafm.com/deepspaceone-128-aac");
        assert_eq!(station.name, "Deep Space One");
        assert_eq!(station.genre, "ambient");
        assert_eq!(station_from(&hit("", "", "", 0)).genre, "");
    }

    /// The meta line only names what the directory knows, so a hit with
    /// nothing known shows nothing rather than a row of blanks.
    #[test]
    fn the_meta_line_skips_what_the_directory_does_not_know() {
        assert_eq!(meta_line(&hit("", "US", "AAC", 128)), "US, AAC, 128 kbps");
        assert_eq!(meta_line(&hit("", "", "MP3", 0)), "MP3");
        assert_eq!(meta_line(&hit("", "", "", 0)), "");
    }

    /// A station's key names the radio source and carries the stream URL
    /// where a file would carry its path. That's the whole play path: no
    /// special verb, just a key the resolve knows how to look up.
    #[test]
    fn a_station_plays_under_the_radio_source() {
        let key = key_for("https://host/jazz");

        assert_eq!(&*key.source, stations::SOURCE);
        assert_eq!(key.path.to_string_lossy(), "https://host/jazz");
        assert_eq!(key.sub, 0);
        assert!(!key.is_local(), "a station is never a file on disk");
    }
}
