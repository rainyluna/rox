//! Taking plugin rows back out of the library from the shared track menu.
//! A row added on its own gets Remove from Library; a row a kept collection
//! holds gets Stop Keeping for that collection, since removing the one row
//! would last only until the next sync.

use gpui::{AnyWindowHandle, App, SharedString, Task};
use gpui_component::Icon;
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use rox_design::assets::icons;
use rox_services::plugin_library;

use crate::panel::{AppState, Tone};
use crate::toast::Toast;

/// Remove from Library for the saved plugin rows among `ids`, then Stop
/// Keeping for each kept collection holding any of them. Nothing for rows
/// of other sources.
pub fn items(menu: PopupMenu, state: &AppState, ids: &[i64], cx: &mut App) -> PopupMenu {
    let removable = plugin_library::removable(state.library.read(cx), ids);
    if removable.is_empty() {
        return menu;
    }

    let mut menu = menu;

    if !removable.saved.is_empty() {
        let (library, saved) = (state.library.clone(), removable.saved);

        menu = menu.item(
            PopupMenuItem::new(rox_i18n::t!("source-browser-remove-from-library"))
                .icon(Icon::default().path(icons::MINUS))
                .on_click(move |_, window, cx| {
                    let task = plugin_library::remove(library.clone(), saved.clone(), cx);
                    let failed = rox_i18n::t!("source-browser-unsave-failed");
                    let what = "plugin rows: removing saved rows".to_string();
                    report(task, what, failed, window.window_handle(), cx);
                }),
        );
    }

    for kept in removable.kept {
        let library = state.library.clone();
        let label = rox_i18n::t!("panel-stop-keeping", title = kept.title.as_str());

        menu = menu.item(
            PopupMenuItem::new(label)
                .icon(Icon::default().path(icons::MINUS))
                .on_click(move |_, window, cx| {
                    let task = plugin_library::stop_keeping(library.clone(), &kept, cx);
                    let failed =
                        rox_i18n::t!("panel-stop-keeping-failed", title = kept.title.as_str());
                    let what = format!("{}: letting go of {}", kept.source, kept.id);
                    report(task, what, failed, window.window_handle(), cx);
                }),
        );
    }

    menu
}

/// The write lands after the menu has closed, so a failure says so in a
/// toast on the window the menu was in.
fn report(
    task: Task<Result<usize, String>>,
    what: String,
    failed: SharedString,
    origin: AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let Err(e) = task.await else {
            return;
        };

        log::warn!("{what} failed: {e}");
        cx.update(|cx| Toast::new(Tone::Bad, e).title(failed).post(origin, cx))
            .ok();
    })
    .detach();
}
