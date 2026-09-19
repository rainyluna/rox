//! The one question every mark, cue and loop asks before it does
//! anything: is there a position to hang this off?
//!
//! A live stream has no timeline. Its clock counts how long you have been
//! listening, so "two minutes in" means something different every time
//! you sit down, and a mark placed at it points nowhere on the next
//! listen. The player already knows this and draws the strip differently;
//! this is the same fact, asked by everything that would otherwise store
//! or seek to a number.
//!
//! One answer and one reason, shared, so the strips, the menus and the key
//! commands all lock out together and say the same thing when they do.

use gpui::{App, SharedString, div, prelude::*};
use gpui_component::Icon;
use gpui_component::menu::PopupMenuItem;
use rox_panel_kit::Tip;

use crate::panel::AppState;

/// Whether anything position-bound can act right now. False while the
/// playing entry is a live stream, true otherwise, the idle player
/// included: with nothing playing the commands have their own reasons to
/// do nothing, and this isn't one of them.
pub fn allowed(state: &AppState, cx: &App) -> bool {
    !state
        .player
        .read(cx)
        .now_playing()
        .is_some_and(|now| now.live)
}

/// What to tell someone whose click did nothing.
pub fn reason() -> SharedString {
    rox_i18n::t!("position-bound-streaming")
}

/// A menu row that would move a position, offered while nothing has one.
/// Greyed rather than dropped, because a row that vanishes teaches nobody
/// why: this one stays where it was and says what's in the way on hover.
///
/// Built as an element item so the label can carry a tooltip. The stock
/// row is a plain string and has nowhere to hang one.
pub fn locked_item(label: SharedString, icon: &'static str) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        Tip::keyed(label.clone(), reason()).apply(div().child(label.clone()))
    })
    .icon(Icon::default().path(icon))
    .disabled(true)
}
