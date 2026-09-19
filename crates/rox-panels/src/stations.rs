//! The stations panel: web radio, listed and played. A station is an
//! ordinary track row under the radio source (see
//! [`rox_library::stations`]), so there's no special play verb here and no
//! second queue. A row resolves to a locator and goes to the player the
//! way a file does, and everything downstream (the queue, the history, the
//! visualizers) treats it as the track it is.
//!
//! Listing and playing is the whole of it. This panel used to carry an add
//! row, a directory search, and a results view standing in for the list,
//! which is three forms wedged into a surface people keep open to press
//! play. The directory window finds stations now, and the Sources page in
//! settings keeps the list. What's left here is the list, a double click
//! to play, and two ways over to the surfaces that own the rest.
//!
//! A row shows everything known about a station, which for a long time was
//! a name and a URL and read as a bookmarks file. The logo comes out of
//! the thumbnail pool under the row's path, the genre, codec and bitrate
//! are what the stream said when it was last played, and the URL itself
//! moves to a tooltip: it's the identity, not something anyone reads.
//!
//! Two lines, always. The name holds the top one, with the clocks pinned to
//! its far end while the station plays; the second line is the song on air
//! for the playing row and the stream's own facts for every other. A
//! station is the one row in the app whose contents change while it sits
//! there, and trading the facts away for the song is what keeps that from
//! costing the list a line of height it only ever needs once.

use std::path::Path;

use gpui::{
    AnyElement, App, Context, Div, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, SharedString, Stateful, Subscription, WeakEntity, Window,
    div, img, prelude::*, px, svg,
};
use gpui_component::Icon;
use gpui_component::button::Button;
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use rox_dock::{Panel, PanelEvent, TabPanel};
use rox_library::cue::{TrackKey, source_id};
use rox_library::stations::{self, Heard, Station};
use serde::{Deserialize, Serialize};

use crate::assets::icons;
use crate::catalog::LibraryEvent;
use crate::design::{palette, tokens};
use crate::panel::{self, AppState, PanelChrome, PanelSettings};
use crate::panel_settings;
use crate::player::fmt_time;
use crate::thumbs::Thumb;

/// The settings page the list is kept on, named by the key the settings
/// window lists it under. The panels are a crate below that window, so a
/// jump to one of its pages travels as its nav key.
const SOURCES_PAGE: &str = "settings-page-sources";

/// The logo square on the left of a row. One size for every row, which is
/// also what every row is: two lines, whether the second one carries the
/// song or the stream's facts, so the column of pictures reads as a column
/// instead of stepping in and out around the playing station.
const ART: Pixels = px(40.);

/// The panel's per-view config. The stations themselves live in the
/// library, so a saved layout restores the shared chrome and nothing else.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StationsConfig {
    /// The rename, theme override, and placement locks shared by every
    /// panel.
    #[serde(flatten)]
    pub chrome: PanelChrome,
}

pub struct StationsPanel {
    state: AppState,
    config: StationsConfig,
    /// Each station with whatever the stream filled in for it: the codec
    /// and bitrate columns a row only has once it's been played.
    stations: Vec<(Station, Heard)>,
    /// The station playing when the panel last drew, so its row highlights.
    playing: Option<TrackKey>,
    /// The whole second the playing row's clocks last read. The pump
    /// notifies every 16ms and these count in seconds, so this is what
    /// keeps the list off sixty repaints a second while a station plays.
    clock_secs: u64,
    /// The title revision the list last drew. A station moving to the next
    /// song changes a row without changing anything else the panel reads.
    seen_rev: u64,
    /// Why the last import did nothing, shown over the list. A playlist of
    /// local files is a real thing to pick by mistake, and failing
    /// silently reads as the button being broken.
    notice: Option<SharedString>,
    /// The row under the last right press, what the list's context menu
    /// builds for. None means the press missed the rows, and the menu is
    /// the panel's own.
    menu_row: Option<usize>,
    focus: FocusHandle,
    /// The tab panel that currently hosts this panel, for duplicate and pop-out.
    tab_panel: Option<WeakEntity<TabPanel>>,
    _library_changed: Subscription,
    _player_changed: Subscription,
    _thumbs_changed: Subscription,
}

impl StationsPanel {
    pub fn new(
        state: AppState,
        config: StationsConfig,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // A scan or any other catalog write can move the rows under the
        // list, the same reason every other library-backed panel re-reads
        // on this. An add from the settings page arrives this way too.
        let _library_changed = cx.subscribe(
            &state.library,
            |this: &mut Self, _, event: &LibraryEvent, cx| {
                if matches!(event, LibraryEvent::Updated) {
                    this.refresh(cx);
                }
            },
        );
        // Three things move a row here and all of them arrive on the pump:
        // the station under the cursor, the song it moved to, and its own
        // clock. The clock is read in whole seconds so a station playing
        // doesn't cost the list a repaint every tick.
        let _player_changed = cx.observe(&state.player, |this: &mut Self, _, cx| {
            let player = this.state.player.read(cx);
            let playing = player.now_playing().map(|now| now.key);
            let clock_secs = player
                .now_playing()
                .map(|now| now.position_secs as u64)
                .unwrap_or(0);
            let rev = player.title_rev().unwrap_or(0);

            if this.playing == playing && this.clock_secs == clock_secs && this.seen_rev == rev {
                return;
            }

            this.playing = playing;
            this.clock_secs = clock_secs;
            this.seen_rev = rev;
            cx.notify();
        });

        // A logo landing in the pool repaints the rows waiting on one, the
        // same subscription every other art-drawing panel keeps.
        let _thumbs_changed = cx.observe(&state.thumbs, |_: &mut Self, _, cx| cx.notify());

        let mut panel = StationsPanel {
            playing: state.player.read(cx).now_playing().map(|now| now.key),
            clock_secs: 0,
            seen_rev: 0,
            state,
            config,
            stations: Vec::new(),
            notice: None,
            menu_row: None,
            focus: cx.focus_handle().tab_stop(true),
            tab_panel: None,
            _library_changed,
            _player_changed,
            _thumbs_changed,
        };
        panel.refresh(cx);
        panel
    }

    /// Re-read the station list off the library's database. At panel-open
    /// and edit cadence, never per frame.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.stations = self
            .open_db(cx)
            .and_then(|conn| stations::detailed(&conn).ok())
            .unwrap_or_default();

        cx.notify();
    }

    /// The panel's own connection to the library database, the idiom the
    /// metadata panel and the library panel's own side reads already use.
    /// Opened per call rather than held, since an edit here happens at
    /// human pace and a held connection would sit through every scan.
    fn open_db(&self, cx: &App) -> Option<rox_library::rusqlite::Connection> {
        let path = self.state.library.read(cx).db_path();

        rox_library::store::open(&path).ok()
    }

    /// Import a `.pls` or `.m3u` of stream URLs, which is how most people
    /// already have their stations.
    fn import(&self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.pop() else {
                return;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                return;
            };

            let found = stations::import(&text);
            this.update(cx, |this, cx| {
                // A playlist of local files is a real thing to pick by
                // mistake, and it imports as nothing. Say so.
                if found.is_empty() {
                    this.notice = Some(rox_i18n::t!("stations-import-empty"));
                    cx.notify();
                    return;
                }

                if this.write(&found, cx) {
                    this.notice = None;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Write stations, then have the library rebuild its projection so the
    /// new rows show up everywhere else too, not only in this list.
    fn write(&mut self, stations: &[Station], cx: &mut Context<Self>) -> bool {
        let Some(mut conn) = self.open_db(cx) else {
            return false;
        };
        if let Err(e) = stations::put(&mut conn, stations) {
            log::warn!("stations: writing {} rows failed: {e}", stations.len());
            return false;
        }

        self.state
            .library
            .update(cx, |library, cx| library.reload_projection(cx));
        self.refresh(cx);
        true
    }

    fn remove(&mut self, url: &str, cx: &mut Context<Self>) {
        let Some(mut conn) = self.open_db(cx) else {
            return;
        };
        if let Err(e) = stations::remove(&mut conn, url) {
            log::warn!("stations: removing a station failed: {e}");
            return;
        }

        self.state
            .library
            .update(cx, |library, cx| library.reload_projection(cx));
        self.refresh(cx);
    }

    /// Play a station, which goes through the same path any track does:
    /// the key resolves to a locator and the player opens it.
    fn play(&self, url: &str, cx: &mut Context<Self>) {
        let key = key_for(url);
        self.state
            .player
            .update(cx, |player, cx| player.play_now(vec![key], cx));
    }

    fn body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette::bg_root())
            .track_focus(&self.focus)
            .children(self.notice.clone().map(|notice| {
                div()
                    .flex_none()
                    .w_full()
                    .p(tokens::SPACE_SM)
                    .border_b_1()
                    .border_color(palette::border())
                    .text_xs()
                    .text_color(palette::tone_warn())
                    .child(notice)
            }))
            // One menu over the whole list, built for whichever row the
            // press landed on, the shape every other list panel uses. A
            // menu per row shares one element state across the rows and
            // never opens.
            .child(self.list(window, cx).context_menu({
                let weak = cx.entity().downgrade();
                move |menu, window, cx| {
                    let Some(this) = weak.upgrade() else {
                        return menu;
                    };
                    this.update(cx, |this, cx| this.row_menu(menu, window, cx))
                }
            }))
    }

    /// The rows, or the empty state when there are none. Either way the
    /// element records where a right press landed, so the menu the body
    /// hangs on it knows whether it's for a row or for the panel.
    fn list(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let list = if self.stations.is_empty() {
            self.empty_state(cx)
        } else {
            let rows: Vec<AnyElement> = self
                .stations
                .iter()
                .enumerate()
                .map(|(ix, (station, heard))| self.row(ix, station, heard, cx))
                .collect();

            div().flex_1().min_h_0().w_full().flex().flex_col().child(
                div()
                    .id("stations-list")
                    .size_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
        };

        // A right press clears the target on the way down; a row's own
        // handler runs after and sets it back, so by the time the menu
        // builds the target is the row under the pointer or nothing.
        list.capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
            if event.button == MouseButton::Right {
                this.menu_row = None;
            }
        }))
    }

    /// No stations yet: the two doors out, since neither the finding nor
    /// the keeping happens in here any more.
    fn empty_state(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(tokens::SPACE_SM)
            .p(tokens::SPACE_MD)
            .text_center()
            .child(div().text_lg().child(rox_i18n::t!("stations-empty-title")))
            .child(
                div()
                    .text_sm()
                    .text_color(palette::text_muted())
                    .child(rox_i18n::t!("stations-empty")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(tokens::SPACE_XS)
                    .child(crate::settings::ui::small_button(
                        rox_i18n::t!("stations-find"),
                        icons::SEARCH,
                        false,
                        cx.listener(|this, _, _, cx| this.find(cx)),
                    ))
                    .child(crate::settings::ui::small_button(
                        rox_i18n::t!("stations-manage"),
                        icons::SETTINGS,
                        false,
                        |_, window, cx| manage(window, cx),
                    )),
            )
    }

    /// The right-click menu: play and remove for the row under the press,
    /// then the panel's own items; the panel menu alone when the press
    /// missed the rows.
    fn row_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let Some(station) = self
            .menu_row
            .and_then(|ix| self.stations.get(ix))
            .map(|(station, _)| station)
        else {
            return self.dropdown_menu(menu, window, cx);
        };

        let play_url = station.url.clone();
        let remove_url = station.url.clone();
        let homepage = self.homepage(&station.url, cx);
        let player = self.state.player.clone();
        let panel = cx.entity().downgrade();

        let menu = menu
            .item(
                PopupMenuItem::new(rox_i18n::t!("stations-play"))
                    .icon(Icon::default().path(icons::PLAY))
                    .on_click(move |_, _, cx| {
                        let key = key_for(&play_url);
                        player.update(cx, |player, cx| player.play_now(vec![key], cx));
                    }),
            )
            .item(
                PopupMenuItem::new(rox_i18n::t!("stations-remove"))
                    .icon(Icon::default().path(icons::TRASH))
                    .on_click(move |_, _, cx| {
                        panel
                            .update(cx, |this, cx| this.remove(&remove_url, cx))
                            .ok();
                    }),
            );

        // Only the playing station has one to open. The homepage comes off
        // the stream's own headers and `tracks` has no column to keep it
        // in, so for every other row there's nothing to offer.
        let menu = match homepage {
            Some(homepage) => menu.item(
                PopupMenuItem::new(rox_i18n::t!("stations-homepage"))
                    .icon(Icon::default().path(icons::EXTERNAL_LINK))
                    .on_click(move |_, _, cx| cx.open_url(&homepage)),
            ),

            None => menu,
        };

        self.dropdown_menu(menu.separator(), window, cx)
    }

    /// One station: its logo, its name, and a line of whatever is known
    /// about the stream, with the URL moved to the tooltip. The playing
    /// row trades that second line for the song on air and pins the clocks
    /// beside the name. A double click plays, the library's move; the
    /// right-click menu holds the same play and the remove.
    fn row(
        &self,
        ix: usize,
        station: &Station,
        heard: &Heard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_air = self
            .playing
            .as_ref()
            .is_some_and(|key| key.path.to_string_lossy() == station.url);

        let url = station.url.clone();
        let play_url = station.url.clone();
        let facts = facts(heard);

        div()
            .id(("station-row", ix))
            .flex()
            .flex_row()
            .items_center()
            .gap(tokens::SPACE_SM)
            .px(tokens::SPACE_SM)
            .py(tokens::SPACE_XS)
            .w_full()
            .min_w_0()
            .cursor_pointer()
            // The playing row takes the highlight role, the same faint cut
            // the library and the bookmarks list pick a playing track out
            // with.
            .when(on_air, |row| {
                row.bg(palette::alpha(palette::highlight(), 0x12))
            })
            .hover(|row| row.bg(palette::bg_control_hover()))
            .child(self.art(&station.url, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(tokens::SPACE_SM)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(if on_air {
                                        palette::text_bright()
                                    } else {
                                        palette::text()
                                    })
                                    .child(SharedString::from(display_name(station))),
                            )
                            .children(on_air.then(|| self.clocks(cx)).flatten()),
                    )
                    // The playing row's second line is the song, which
                    // moves; every other row's is the stream's facts,
                    // which don't. One line either way.
                    .children(match on_air {
                        true => Some(self.song_line(cx)),

                        false => facts.map(|facts| {
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(palette::text_muted())
                                .child(SharedString::from(facts))
                        }),
                    }),
            )
            // The URL is the station's identity, not something anyone
            // reads off a list, so it's here rather than on the row.
            .tooltip(move |window, cx| Tooltip::new(url.clone()).build(window, cx))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.play(&play_url, cx);
                }
            }))
            // Mark the row for the list's menu; the press itself keeps
            // going so the list's own handler sees it too.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, _| this.menu_row = Some(ix)),
            )
            .into_any_element()
    }

    /// The station's picture out of the thumbnail pool, keyed by the row's
    /// path the way every other art lookup in the app is. A station added
    /// through the directory has its logo there already; one that was
    /// typed in gets it the first time it plays and names a homepage.
    fn art(&self, url: &str, cx: &mut Context<Self>) -> AnyElement {
        let thumb = self
            .state
            .thumbs
            .update(cx, |thumbs, cx| thumbs.get(Path::new(url), cx));

        match thumb {
            Thumb::Ready(image) => img(image)
                .flex_none()
                .size(ART)
                .overflow_hidden()
                .object_fit(ObjectFit::Cover)
                .rounded(tokens::RADIUS)
                .into_any_element(),

            // Pending and Missing draw the same mark. A logo that arrives
            // replaces it, and one that never does leaves a row that still
            // reads as a station rather than as a hole.
            _ => div()
                .flex_none()
                .size(ART)
                .flex()
                .items_center()
                .justify_center()
                .rounded(tokens::RADIUS)
                .bg(palette::bg_control())
                .child(
                    svg()
                        .path(icons::RADIO)
                        .size(px(18.))
                        .text_color(palette::text_faint()),
                )
                .into_any_element(),
        }
    }

    /// The playing row's second line: what the station is playing. Nothing
    /// is invented before the first announcement, which on some stations
    /// never comes.
    fn song_line(&self, cx: &App) -> Div {
        let song = self
            .state
            .player
            .read(cx)
            .live_over(None)
            .map(|meta| song_text(&meta.artist, &meta.title))
            .unwrap_or_else(|| rox_i18n::t!("stations-on-air").to_string());

        div()
            .truncate()
            .text_xs()
            .text_color(palette::accent())
            .child(SharedString::from(song))
    }

    /// The playing row's clocks, pinned to the end of the name line: how
    /// long this song has been on and how long the station has. The
    /// station's own clock always reads; the song's only reads once the
    /// stream has named one, which is what gives it a start. None before
    /// the position clock resolves.
    fn clocks(&self, cx: &App) -> Option<Stateful<Div>> {
        let player = self.state.player.read(cx);
        let station_secs = player.now_playing().map(|now| now.position_secs)?;
        let clocks = match player.song_elapsed() {
            Some(song_secs) => format!("{} / {}", fmt_time(song_secs), fmt_time(station_secs)),

            None => fmt_time(station_secs),
        };

        Some(
            div()
                .id("station-clocks")
                .flex_none()
                .text_xs()
                .text_color(palette::text_muted())
                .tooltip(|window, cx| {
                    Tooltip::new(rox_i18n::t!("stations-clocks")).build(window, cx)
                })
                .child(SharedString::from(clocks)),
        )
    }

    /// The homepage the playing station named in its headers, for the row
    /// that station is. None for every other row: the header is read at the
    /// connect and nothing stores it, so a station that isn't playing has
    /// no homepage anybody here knows about.
    fn homepage(&self, url: &str, cx: &App) -> Option<String> {
        if self.playing.as_ref()?.path.to_string_lossy() != url {
            return None;
        }

        let homepage = self.state.player.read(cx).station_info()?.homepage;

        (!homepage.trim().is_empty()).then(|| homepage.trim().to_string())
    }

    /// The directory window, where a station is searched for and added.
    fn find(&self, cx: &mut App) {
        rox_panel_api::openers::station_directory(self.state.clone(), cx);
    }
}

/// The settings window on its Sources page, where the list is kept along
/// with the folders and the Subsonic server.
fn manage(window: &mut Window, cx: &mut App) {
    panel_settings::open_app_page(SOURCES_PAGE, window, cx);
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

/// The row's second line: the genre, the codec and the bitrate, whichever
/// of them the row actually holds. None when it holds none, which is a
/// station that has never been played and came from nowhere that knew
/// anything about it; a line of separators standing in for three missing
/// values is worse than no line.
fn facts(heard: &Heard) -> Option<String> {
    let bitrate = match heard.bitrate_kbps {
        0 => String::new(),

        kbps => rox_i18n::t!("directory-bitrate", kbps = kbps).to_string(),
    };

    let facts: Vec<&str> = [heard.genre.as_str(), heard.codec.as_str(), bitrate.as_str()]
        .into_iter()
        .filter(|fact| !fact.is_empty())
        .collect();

    (!facts.is_empty()).then(|| facts.join(", "))
}

/// The song on air as one line. A station that sends one unsplittable
/// field leaves the artist empty, and the title alone is then the whole of
/// what it said.
fn song_text(artist: &str, title: &str) -> String {
    if artist.is_empty() {
        return title.to_string();
    }

    format!("{artist} - {title}")
}

/// What a row shows for a station with no name of its own. The list is
/// sorted by name, so a blank one would sit at the top reading as nothing
/// at all.
fn display_name(station: &Station) -> String {
    if station.name.trim().is_empty() {
        station.url.clone()
    } else {
        station.name.clone()
    }
}

impl PanelSettings for StationsPanel {
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

impl EventEmitter<PanelEvent> for StationsPanel {}

impl Focusable for StationsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for StationsPanel {
    fn panel_name(&self) -> &'static str {
        "stations"
    }

    rox_panel_api::opens_settings!();

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        panel::title_text(
            self.config.chrome.title.as_deref(),
            rox_i18n::t!("stations-title"),
        )
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

    /// The layout dump stores the panel's config; the builder registered
    /// in `workspace::register_panels` reads it back.
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

    /// Find sits on the tab bar because it's the one thing here worth
    /// finding without a menu: a panel with no stations in it is waiting
    /// on the directory.
    fn toolbar_buttons(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Button>> {
        Some(vec![
            Button::new("stations-find")
                .icon(Icon::default().path(icons::SEARCH))
                .tooltip(rox_i18n::t!("stations-find"))
                .on_click(cx.listener(|this, _, _, cx| this.find(cx))),
        ])
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        // The list is kept in settings, and a `.pls` still imports from
        // here, since the panel is where somebody looking at their
        // stations already is.
        let menu = menu
            .item(
                PopupMenuItem::new(rox_i18n::t!("stations-manage"))
                    .icon(Icon::default().path(icons::SETTINGS))
                    .on_click(move |_, window, cx| manage(window, cx)),
            )
            .item({
                let panel = cx.entity().downgrade();
                PopupMenuItem::new(rox_i18n::t!("stations-import"))
                    .icon(Icon::default().path(icons::DOWNLOAD))
                    .on_click(move |_, window, cx| {
                        panel.update(cx, |this, cx| this.import(window, cx)).ok();
                    })
            })
            .separator();

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
                StationsPanel::new(state, config, window, cx)
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

impl Render for StationsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chrome = self.config.chrome.clone();
        panel::themed(&chrome, || self.body(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(url: &str, name: &str) -> Station {
        Station {
            url: url.to_string(),
            name: name.to_string(),
            genre: String::new(),
        }
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

    /// A row never draws blank. An unnamed station shows its URL, which is
    /// the only thing it has.
    #[test]
    fn an_unnamed_station_shows_its_url() {
        assert_eq!(
            display_name(&station("https://host/jazz", "")),
            "https://host/jazz"
        );
        assert_eq!(
            display_name(&station("https://host/jazz", " ")),
            "https://host/jazz"
        );
        assert_eq!(display_name(&station("https://host/jazz", "Jazz")), "Jazz");
    }

    /// The second line is whatever the row actually holds. A station with
    /// nothing filled in yet has no second line at all, rather than a row
    /// of commas standing in for what nobody knows.
    #[test]
    fn the_second_line_carries_only_what_is_known() {
        assert_eq!(facts(&Heard::default()), None);

        assert_eq!(
            facts(&Heard {
                genre: "Jazz".into(),
                codec: "mp3".into(),
                bitrate_kbps: 128,
            })
            .as_deref(),
            Some("Jazz, mp3, 128 kbps")
        );

        assert_eq!(
            facts(&Heard {
                codec: "aac".into(),
                ..Heard::default()
            })
            .as_deref(),
            Some("aac")
        );
    }

    /// A station that sends one unsplittable field leaves the artist
    /// empty, and the title alone is then the whole announcement.
    #[test]
    fn the_song_line_survives_half_a_title() {
        assert_eq!(song_text("Miles Davis", "So What"), "Miles Davis - So What");
        assert_eq!(song_text("", "So What"), "So What");
    }
}
