//! Bookmarks on a strip: the marks the seek strip and the waveform draw
//! along the playing track, the hover readout and the right-click menu
//! over each one, and the color set behind them. Shared so a mark looks
//! and acts the same on both strips and in the bookmarks panel, and so
//! the panels stay out of the color business entirely.

use gpui::{
    App, Bounds, Context, Div, MouseButton, MouseDownEvent, MouseMoveEvent, Path, Pixels, Rgba,
    SharedString, Window, div, prelude::*, px, relative,
};
use gpui_component::Icon;
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use rox_core::fmt::fmt_time;
use rox_design::assets::icons;
use rox_design::{palette, tokens};
use rox_library::bookmarks::Bookmark;
use rox_library::cue::TrackKey;

use crate::openers;
use crate::panel::{AppState, ScrubState};

/// The quick picks a mark can take without opening a picker: a name key
/// and its `#rrggbb`. Mid-saturation hues that hold up on a dark and a
/// light surface alike; the theme accent is the ninth choice and the
/// default, stored as no color at all so it follows the palette.
pub const QUICK_COLORS: &[(&str, &str)] = &[
    ("red", "#e5484d"),
    ("orange", "#f76b15"),
    ("yellow", "#f5d90a"),
    ("green", "#30a46c"),
    ("teal", "#12a594"),
    ("blue", "#0090ff"),
    ("purple", "#8e4ec6"),
    ("pink", "#e93d82"),
];

/// A quick pick's display name.
pub fn quick_color_label(key: &str) -> SharedString {
    match key {
        "red" => rox_i18n::t!("bookmark-color-red"),
        "orange" => rox_i18n::t!("bookmark-color-orange"),
        "yellow" => rox_i18n::t!("bookmark-color-yellow"),
        "green" => rox_i18n::t!("bookmark-color-green"),
        "teal" => rox_i18n::t!("bookmark-color-teal"),
        "blue" => rox_i18n::t!("bookmark-color-blue"),
        "purple" => rox_i18n::t!("bookmark-color-purple"),
        "pink" => rox_i18n::t!("bookmark-color-pink"),
        other => SharedString::from(other.to_string()),
    }
}

/// A stored color resolved for paint: the hex when it parses, the theme
/// accent otherwise, which is also what None means.
pub fn color_of(color: Option<&str>) -> Rgba {
    color
        .and_then(palette::parse_hex)
        .unwrap_or_else(palette::accent)
}

/// One mark placed along a strip.
#[derive(Clone)]
pub struct Mark {
    pub id: i64,
    /// Where along the track, 0 to 1.
    pub fraction: f32,
    pub position_ms: u32,
    pub name: String,
    pub color: Rgba,
}

/// Place a track's bookmarks along its strip. Nothing without a duration:
/// a fraction of an unknown length points nowhere.
pub fn marks(bookmarks: &[Bookmark], duration_secs: Option<f64>) -> Vec<Mark> {
    let Some(duration) = duration_secs.filter(|d| *d > 0.0) else {
        return Vec::new();
    };
    bookmarks
        .iter()
        .map(|b| Mark {
            id: b.id,
            fraction: ((b.position_ms as f64 / 1000.0) / duration).clamp(0.0, 1.0) as f32,
            position_ms: b.position_ms,
            name: b.name.clone(),
            color: color_of(b.color.as_deref()),
        })
        .collect()
}

/// What a mark is called where one line has to do: its name, or its
/// time when it was dropped without one.
pub fn mark_label(name: &str, position_ms: u32) -> String {
    let name = name.trim();
    if name.is_empty() {
        fmt_time(position_ms as f64 / 1000.0)
    } else {
        name.to_string()
    }
}

/// The chevron's footprint: its base width and height in px, and the
/// stroke it's drawn with. It sits on the strip's bottom edge pointing up
/// at the line, so it reads as a tab under the track rather than a second
/// playhead.
pub const MARK_W: f32 = 10.0;
pub const MARK_H: f32 = 6.0;
const MARK_STROKE: f32 = 2.5;
/// The hit target around a chevron, wider than the drawing so a pointer
/// finds it without aiming.
const HIT_W: f32 = 16.0;
/// The chevron's alpha at full weight.
const MARK_ALPHA: u8 = 0xe6;

/// Paint the marks over a strip, the seek strip's and the waveform's
/// shared look. `weight` scales the alpha, for a strip fading its shape in
/// or out. Goes on after the played fill and before the playhead, so the
/// head still crosses over a mark it reaches.
pub fn paint_marks(marks: &[Mark], weight: f32, bounds: Bounds<Pixels>, window: &mut Window) {
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    if w <= 0.0 || h <= MARK_H || marks.is_empty() {
        return;
    }
    let alpha = (MARK_ALPHA as f32 * weight.clamp(0.0, 1.0)) as u8;
    if alpha == 0 {
        return;
    }
    let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let at = |x: f32, y: f32| gpui::point(px(x0 + x), px(y0 + y));
    let solid = (
        gpui::point(0., 1.),
        gpui::point(0., 1.),
        gpui::point(0., 1.),
    );
    let bottom = h - 1.0;
    let top = bottom - MARK_H;
    let half = MARK_W / 2.0;
    // The inner edge's inset along the base: the stroke measured across
    // the slope, so the band reads the same thickness up its whole arm.
    let inset = MARK_STROKE * (half * half + MARK_H * MARK_H).sqrt() / MARK_H;
    for mark in marks {
        let x = mark.fraction.clamp(0.0, 1.0) * w;
        // A chevron band: the outer triangle with its inner triangle taken
        // out, as four triangles round the ring.
        let apex = at(x, top);
        let bl = at(x - half, bottom);
        let br = at(x + half, bottom);
        let apex_in = at(x, top + MARK_STROKE);
        let bl_in = at(x - half + inset, bottom);
        let br_in = at(x + half - inset, bottom);
        let mut path = Path::new(apex);
        path.push_triangle((apex, br, br_in), solid);
        path.push_triangle((apex, br_in, apex_in), solid);
        path.push_triangle((apex, apex_in, bl_in), solid);
        path.push_triangle((apex, bl_in, bl), solid);
        window.paint_path(path, palette::alpha(mark.color, alpha));
    }
}

/// How far behind the playhead a mark has to be before Previous goes to
/// it, so a second press steps back past the mark just landed on instead
/// of pinning to it, the way Previous on a track works.
const PREV_GRACE_SECS: f64 = 1.5;

/// Jump the playing track to its next bookmark, or the one before the
/// playhead. Nothing playing, or no mark that way, does nothing.
pub fn step(state: &AppState, forward: bool, cx: &mut App) {
    let Some(now) = state.player.read(cx).now_playing() else {
        return;
    };
    let marks = state.library.read(cx).bookmarks_for(&now.key);
    let target = step_target(
        marks.iter().map(|m| m.position_ms as f64 / 1000.0),
        now.position_secs,
        forward,
    );
    if let Some(secs) = target {
        state.player.read(cx).seek_to(secs);
    }
}

/// The mark a step lands on, from marks in ascending order.
fn step_target(marks: impl Iterator<Item = f64>, at: f64, forward: bool) -> Option<f64> {
    if forward {
        marks.filter(|&secs| secs > at + 0.05).reduce(f64::min)
    } else {
        marks
            .filter(|&secs| secs < at - PREV_GRACE_SECS)
            .reduce(f64::max)
    }
}

/// The interactive layer over a strip's marks: a hit target per chevron
/// that seeks on a click, reports its hover, and opens the mark's menu on
/// a right click, plus the readout over the hovered one. Laid over the
/// strip's own hover layer, so a pointer on a chevron reads the mark and
/// not the time under it.
///
/// `hovered` is the panel's record of which mark the pointer is on, kept by
/// the panel because it outlives one render; `on_hover` is how the layer
/// updates it.
#[allow(clippy::too_many_arguments)]
pub fn overlay<V: 'static>(
    state: &AppState,
    key: &TrackKey,
    marks: &[Mark],
    hovered: Option<i64>,
    scrub: &ScrubState,
    on_hover: impl Fn(&mut V, Option<i64>, &mut Context<V>) + Clone + 'static,
    cx: &mut Context<V>,
) -> Div {
    let mut layer = div().absolute().inset_0();
    for mark in marks {
        let id = mark.id;
        let secs = mark.position_ms as f64 / 1000.0;
        let player = state.player.clone();
        let menu_state = state.clone();
        let menu_key = key.clone();
        let hover_scrub = scrub.clone();
        let on_hover = on_hover.clone();
        let hit = div()
            .id(("bookmark-mark", id as u64))
            .size_full()
            .cursor_pointer()
            // The strip's own readout would keep tracking the pointer under
            // the chevron; clearing it here and stopping the move leaves
            // the mark's readout as the only one showing.
            .on_mouse_move(cx.listener(move |_, _: &MouseMoveEvent, _, cx| {
                hover_scrub.set_hover(None);
                cx.stop_propagation();
            }))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                on_hover(this, hovered.then_some(id), cx);
                cx.notify();
            }))
            // A click lands exactly on the mark, not on the pixel under the
            // pointer, and the strip's own seek stays out of it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    player.read(cx).seek_to(secs);
                    cx.stop_propagation();
                }),
            )
            // The mark's own menu opens off the window-level handler the
            // wrapper below registers, which runs ahead of this; stopping
            // here keeps the press from the dock's body handler, which
            // would open the panel dropdown over it.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .context_menu(move |menu, window, cx| {
                menu_for(menu, menu_state.clone(), menu_key.clone(), id, window, cx)
            });
        // Each slot carries its own id so the context menus inside them,
        // which all share one, get element state of their own.
        layer = layer.child(
            div()
                .id(("bookmark-slot", id as u64))
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(mark.fraction))
                .w(px(HIT_W))
                .ml(px(-HIT_W / 2.0))
                .child(hit),
        );
    }
    if let Some(mark) = hovered.and_then(|id| marks.iter().find(|m| m.id == id)) {
        layer = layer.child(readout(mark));
    }
    layer
}

/// The hovered mark's readout: its name over its time, or the time alone,
/// in the seek preview's pill, centered over the chevron.
fn readout(mark: &Mark) -> Div {
    let time = fmt_time(mark.position_ms as f64 / 1000.0);
    let name = mark.name.trim();
    div()
        .absolute()
        .top(tokens::SPACE_XS)
        .left(relative(mark.fraction))
        .w_0()
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .px(tokens::SPACE_SM)
                .py(px(2.))
                .rounded(tokens::RADIUS)
                .bg(palette::bg_menu_opaque())
                .border_1()
                .border_color(palette::border())
                .text_sm()
                .text_color(palette::text())
                .flex()
                .flex_col()
                .items_center()
                .when(!name.is_empty(), |d| d.child(name.to_string()))
                .child(
                    div()
                        .when(!name.is_empty(), |d| {
                            d.text_xs().text_color(palette::text_muted())
                        })
                        .child(time),
                ),
        )
}

/// A mark's actions: rename, color, move to the playhead, remove. The
/// strips and the bookmarks panel share it; a caller with a play row of its
/// own puts that ahead of this.
pub fn menu_for(
    menu: PopupMenu,
    state: AppState,
    key: TrackKey,
    id: i64,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let rename_state = state.clone();
    let menu = menu.item(
        PopupMenuItem::new(rox_i18n::t!("bookmark-menu-rename"))
            .icon(Icon::default().path(icons::PENCIL))
            .on_click(move |_, _, cx| openers::bookmark_edit(rename_state.clone(), id, cx)),
    );
    let menu = color_submenu(menu, state.clone(), vec![id], window, cx);
    // Move only offers itself while the mark's track is the one playing:
    // the playhead is the target, and on any other track it points at
    // nothing to do with this mark.
    let playing = state
        .player
        .read(cx)
        .now_playing()
        .filter(|now| now.key == key);
    let menu = match playing {
        Some(now) => {
            let move_state = state.clone();
            let position_ms = (now.position_secs.max(0.0) * 1000.0).round() as u32;
            menu.item(
                PopupMenuItem::new(rox_i18n::t!("bookmark-menu-move"))
                    .icon(Icon::default().path(icons::LOCATE))
                    .on_click(move |_, _, cx| {
                        move_state
                            .library
                            .update(cx, |library, cx| library.move_bookmark(id, position_ms, cx));
                    }),
            )
        }
        None => menu,
    };
    let remove_state = state;
    menu.separator().item(
        PopupMenuItem::new(rox_i18n::t!("bookmark-menu-remove"))
            .icon(Icon::default().path(icons::TRASH))
            .on_click(move |_, _, cx| {
                remove_state
                    .library
                    .update(cx, |library, cx| library.remove_bookmark(id, cx));
            }),
    )
}

/// The Color flyout over one or more marks: the accent, the quick picks
/// with the first mark's current one checked, and, for a single mark, the
/// picker for anything else. A pick lands on every id at once, which is
/// what a multi-selection in the bookmarks panel asks for.
pub fn color_submenu(
    menu: PopupMenu,
    state: AppState,
    ids: Vec<i64>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    menu.submenu_with_icon(
        Some(Icon::default().path(icons::PALETTE)),
        rox_i18n::t!("bookmark-menu-color"),
        window,
        cx,
        move |menu, _, cx| color_menu(menu, state.clone(), ids.clone(), cx),
    )
}

fn color_menu(menu: PopupMenu, state: AppState, ids: Vec<i64>, cx: &App) -> PopupMenu {
    let current = ids
        .first()
        .and_then(|&id| state.library.read(cx).bookmark(id))
        .and_then(|b| b.color)
        .map(|c| c.to_ascii_lowercase());
    let recolor = |state: &AppState, color: Option<&'static str>, ids: &[i64], cx: &mut App| {
        state.library.update(cx, |library, cx| {
            for &id in ids {
                library.set_bookmark_color(id, color, cx);
            }
        });
    };
    let accent_state = state.clone();
    let accent_ids = ids.clone();
    let mut menu = menu.item(
        PopupMenuItem::new(rox_i18n::t!("bookmark-color-accent"))
            .checked(current.is_none())
            .on_click(move |_, _, cx| recolor(&accent_state, None, &accent_ids, cx)),
    );
    for (key, hex) in QUICK_COLORS {
        let pick_state = state.clone();
        let pick_ids = ids.clone();
        menu = menu.item(
            PopupMenuItem::new(quick_color_label(key))
                .checked(current.as_deref() == Some(*hex))
                .on_click(move |_, _, cx| recolor(&pick_state, Some(hex), &pick_ids, cx)),
        );
    }
    // The picker edits one mark's row; over a set there's no one row to
    // seed it from, so the quick picks are the whole offer.
    let [id] = ids[..] else {
        return menu;
    };
    let custom_state = state;
    menu.separator().item(
        PopupMenuItem::new(rox_i18n::t!("bookmark-color-custom"))
            .checked(current.is_some_and(|c| !QUICK_COLORS.iter().any(|(_, hex)| *hex == c)))
            .on_click(move |_, _, cx| openers::bookmark_edit(custom_state.clone(), id, cx)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(position_ms: u32, name: &str, color: Option<&str>) -> Bookmark {
        Bookmark {
            id: position_ms as i64,
            track_id: 1,
            position_ms,
            name: name.into(),
            color: color.map(str::to_string),
            created: 0,
        }
    }

    #[test]
    fn marks_place_along_the_duration_and_need_one() {
        let set = vec![
            mark(30_000, "", None),
            mark(150_000, "Drop", Some("#0090ff")),
        ];
        let placed = marks(&set, Some(120.0));
        assert_eq!(placed.len(), 2);
        assert!((placed[0].fraction - 0.25).abs() < 1e-6);
        // Past the end clamps onto the strip rather than off it.
        assert_eq!(placed[1].fraction, 1.0);
        assert!(marks(&set, None).is_empty());
        assert!(marks(&set, Some(0.0)).is_empty());
    }

    #[test]
    fn a_nameless_mark_is_called_by_its_time() {
        assert_eq!(mark_label("", 65_000), "1:05");
        assert_eq!(mark_label("  Chorus ", 65_000), "Chorus");
    }

    #[test]
    fn a_step_finds_the_neighbouring_mark_with_a_grace_going_back() {
        let marks = [10.0, 40.0, 70.0];
        let step = |at, forward| step_target(marks.iter().copied(), at, forward);
        assert_eq!(step(0.0, true), Some(10.0));
        assert_eq!(step(40.0, true), Some(70.0));
        assert_eq!(step(70.0, true), None);
        // Just past a mark, Previous goes to it; landed on it, to the one before.
        assert_eq!(step(45.0, false), Some(40.0));
        assert_eq!(step(40.5, false), Some(10.0));
        assert_eq!(step(5.0, false), None);
    }

    #[test]
    fn a_bad_color_falls_back_to_the_accent() {
        let accent = palette::accent();
        let read = |c: Option<&str>| {
            let c = color_of(c);
            (c.r, c.g, c.b)
        };
        assert_eq!(read(None), (accent.r, accent.g, accent.b));
        assert_eq!(read(Some("nonsense")), (accent.r, accent.g, accent.b));
        assert_ne!(read(Some("#0090ff")), (accent.r, accent.g, accent.b));
    }
}
