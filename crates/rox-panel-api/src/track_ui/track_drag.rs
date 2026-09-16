//! The shared payload for dragging tracks. Holds the tracks in drag order so
//! a drop queues them straight through, out-of-library files included, and
//! carries their library ids alongside for a target that wants catalog rows
//! instead (a playlist). One type so library rows, other panels, and external
//! file drops all go through the same enqueue path.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{SharedString, div};

use rox_design::{palette, tokens};
use rox_library::cue::TrackKey;

/// The value carried through a track drag. `keys` is the drag order a drop
/// enqueues; keys rather than paths, so dragging two tracks of one cue rip
/// queues two tracks instead of the image twice. `ids` is the same drag in
/// catalog terms for a target that stores rows rather than plays them. Both
/// are in drag order, and neither is an index into the other: a cue rip
/// spreads one id over several keys, and a file with no catalog row has a key
/// and no id. `title` labels the floating preview. Both lists sit behind an
/// Arc so a row attaches the payload with a refcount bump: a grab inside a big
/// multi-selection would otherwise clone the whole set into every visible
/// selected row on every frame.
#[derive(Clone)]
pub struct PlayDrag {
    pub keys: Arc<[TrackKey]>,
    /// The library track ids behind the keys, in drag order, for a drop
    /// target that wants catalog rows rather than something to play (a
    /// playlist). Empty when the source had none.
    pub ids: Arc<[i64]>,
    pub title: SharedString,
}

impl PlayDrag {
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// The label that floats under the pointer while tracks are dragged. A multi
/// drag shows the grabbed title with a count of the rest.
pub struct PlayDragPreview {
    pub title: SharedString,
    pub extra: usize,
}

impl Render for PlayDragPreview {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let label = if self.extra > 0 {
            SharedString::from(format!("{} +{}", self.title, self.extra))
        } else {
            self.title.clone()
        };
        div()
            .px(tokens::SPACE_SM)
            .py(tokens::SPACE_XS)
            .rounded(tokens::RADIUS)
            .bg(palette::bg_control())
            .text_color(palette::text())
            .child(label)
    }
}
