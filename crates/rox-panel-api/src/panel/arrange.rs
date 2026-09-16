//! The arrangement editor the composable strips share: the shown items as
//! chips in display order, a tray of the hidden ones below. Drag along a
//! well reorders, drag between the wells and the tray shows and hides, and
//! the chips' plus and x do the same by click. The config behind it is one
//! ordered list per panel; an item off the list is hidden. Most items show
//! at most once per row, so each line can have its own copy; one the
//! catalog marks repeatable keeps its tray chip while shown, so a well
//! can hold several. A panel whose layout stacks rows edits them through
//! [`arrange_rows_editor`]: one well per row, a button below adding the
//! next; the flat [`arrange_editor`] is the same thing capped at one well.
//!
//! Most panels know their catalog at compile time and declare it as a
//! static slice of [`ArrangeSpec`]. A panel whose items are whatever the
//! user made of them, the custom controls strip whose chips are buttons
//! that didn't exist when the binary was built, hands over a `Vec` of
//! [`ArrangeEntry`] instead, with its labels already resolved.
//! [`Arrangement`] is the seam between those two: everything below it
//! works over the one type, and both forms convert into it.

use std::rc::Rc;

use gpui::{Context, Div, SharedString, Stateful, Window, div, prelude::*, px, svg};

use rox_design::assets::icons;
use rox_design::{palette, tokens};

/// One arrangeable item of a strip: its chip label and icon, and the
/// config value it stands for. Each panel declares its catalog as a
/// static slice in stock order; that order is where a re-shown item
/// slots back in.
pub struct ArrangeSpec<V: 'static> {
    /// The item's message key, resolved to its chip label at render time.
    /// It doubles as the chip's element id: a key is stable across
    /// locales where a translated label isn't, so a drag doesn't lose
    /// its state when the language changes under it.
    pub key: &'static str,
    pub icon: Option<&'static str>,
    pub value: V,
    /// Whether one row may hold more than one of this item. A repeatable
    /// item keeps its tray chip while shown, so another copy is always
    /// one plus away; the spacers and dividers, mostly. A non-repeatable
    /// item is unique per row, not per editor: each row can have its own.
    pub repeats: bool,
}

/// One arrangeable item a panel builds at runtime, carrying the text an
/// [`ArrangeSpec`] would have looked up. A catalog that only exists while
/// the app runs has no message key to name its items by, so it spells out
/// both halves the key stood for.
#[derive(Clone)]
pub struct ArrangeEntry<V> {
    /// The chip's element id, under the same rule the static form's key
    /// is: stable across locales and across a rebuild of the registry, or
    /// a drag loses its state mid-gesture. Derive it from the item's own
    /// identity, never from its label.
    pub id: SharedString,
    pub label: SharedString,
    pub icon: Option<&'static str>,
    pub value: V,
    pub repeats: bool,
}

/// The catalog an editor works over, in whichever form its panel has it.
/// The built side is refcounted rather than copied: every drag and drop
/// handler on the page needs the catalog, and they outlive the render
/// that made them.
pub enum Arrangement<V: 'static> {
    /// A catalog baked into the binary, which is nearly every panel.
    Stock(&'static [ArrangeSpec<V>]),
    /// A catalog assembled this frame out of what the user configured.
    Built(Rc<[ArrangeEntry<V>]>),
}

impl<V: 'static> Clone for Arrangement<V> {
    /// By hand rather than derived, so the item type doesn't have to be
    /// `Clone` for the registry to be.
    fn clone(&self) -> Self {
        match self {
            Arrangement::Stock(specs) => Arrangement::Stock(specs),

            Arrangement::Built(entries) => Arrangement::Built(entries.clone()),
        }
    }
}

impl<V: 'static> From<&'static [ArrangeSpec<V>]> for Arrangement<V> {
    fn from(specs: &'static [ArrangeSpec<V>]) -> Self {
        Arrangement::Stock(specs)
    }
}

impl<V: 'static> From<Vec<ArrangeEntry<V>>> for Arrangement<V> {
    fn from(entries: Vec<ArrangeEntry<V>>) -> Self {
        Arrangement::Built(entries.into())
    }
}

impl<V: PartialEq + Copy + 'static> Arrangement<V> {
    /// Whether the catalog allows `value` twice on one row.
    fn repeats(&self, value: V) -> bool {
        match self {
            Arrangement::Stock(specs) => specs
                .iter()
                .find(|spec| spec.value == value)
                .is_some_and(|spec| spec.repeats),

            Arrangement::Built(entries) => entries
                .iter()
                .find(|entry| entry.value == value)
                .is_some_and(|entry| entry.repeats),
        }
    }

    /// Where `value` sits in the catalog, which is the order a re-shown
    /// item slots back at. An item the catalog doesn't carry ranks last.
    fn rank(&self, value: V) -> usize {
        let place = match self {
            Arrangement::Stock(specs) => specs.iter().position(|spec| spec.value == value),

            Arrangement::Built(entries) => entries.iter().position(|entry| entry.value == value),
        };

        place.unwrap_or(usize::MAX)
    }

    /// The catalog as the editor draws it. A stock catalog resolves its
    /// message keys here, once per render, which is what keeps a
    /// translated label out of the element ids.
    fn entries(&self) -> Vec<ArrangeEntry<V>> {
        match self {
            Arrangement::Stock(specs) => specs
                .iter()
                .map(|spec| ArrangeEntry {
                    id: SharedString::new_static(spec.key),
                    label: rox_i18n::t!(spec.key),
                    icon: spec.icon,
                    value: spec.value,
                    repeats: spec.repeats,
                })
                .collect(),

            Arrangement::Built(entries) => entries.to_vec(),
        }
    }
}

/// The value a chip drag carries. The type is generic over the item enum,
/// so a drop only ever dispatches to editors of the same panel kind; the
/// editor id guards the one case left, two settings windows of the same
/// kind open at once. `from` is the (row, index) place the drag left, or
/// None off the tray: with repeatable items on a well, the place is the
/// identity a value alone can't give.
#[derive(Clone)]
struct ArrangeDrag<V: Clone + 'static> {
    editor: &'static str,
    value: V,
    from: Option<(usize, usize)>,
    label: SharedString,
    icon: Option<&'static str>,
}

/// The chip that floats under the pointer while one is dragged.
struct ChipPreview {
    label: SharedString,
    icon: Option<&'static str>,
}

impl Render for ChipPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        chip(self.label.clone(), self.icon, false)
            .border_1()
            .border_color(palette::border_light())
    }
}

/// The chip look shared by the bar, the tray, and the drag preview.
fn chip(label: SharedString, icon: Option<&'static str>, dimmed: bool) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(tokens::SPACE_XS)
        .px(tokens::SPACE_SM)
        .py(tokens::SPACE_XS)
        .rounded(tokens::RADIUS)
        .bg(palette::bg_control())
        .children(icon.map(|icon| {
            svg().path(icon).size(px(14.)).text_color(if dimmed {
                palette::text_faint()
            } else {
                palette::text_muted()
            })
        }))
        .child(
            div()
                .text_sm()
                .whitespace_nowrap()
                .text_color(if dimmed {
                    palette::text_muted()
                } else {
                    palette::text()
                })
                .child(label),
        )
}

/// The trailing glyph on a chip: the x that hides a shown item, the plus
/// that brings a hidden one back. Faint until hovered so the chips stay
/// quiet.
fn chip_action(icon: &'static str) -> Div {
    div()
        .flex_none()
        .rounded(tokens::RADIUS)
        .cursor_pointer()
        .child(
            svg()
                .path(icon)
                .size(px(12.))
                .text_color(palette::text_faint())
                .hover(|s| s.text_color(palette::text())),
        )
}

/// The bordered row a zone's chips sit in; wraps when the chips outgrow
/// the line, and holds its height while empty so it stays a drop target.
fn well() -> Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(tokens::SPACE_XS)
        .p(tokens::SPACE_XS)
        .rounded(tokens::RADIUS)
        .border_1()
        .border_color(palette::border())
        .min_h(px(36.))
}

/// A zone's tiny caption above its well.
fn caption(text: gpui::SharedString) -> Div {
    div()
        .text_xs()
        .text_color(palette::text_faint())
        .child(text)
}

/// Insert `value` into `row` at `at`. Uniqueness is per row: a
/// non-repeatable value already on the row leaves first, pulling the
/// drop point along when it was before it, so a drop replaces the row's
/// copy instead of doubling it while other rows keep theirs.
fn insert_row_unique<V: PartialEq + Copy>(row: &mut Vec<V>, value: V, at: usize, unique: bool) {
    let mut at = at.min(row.len());
    if unique {
        let mut ix = 0;
        row.retain(|v| {
            let keep = *v != value;
            if !keep && ix < at {
                at -= 1;
            }
            ix += 1;
            keep
        });
    }
    row.insert(at.min(row.len()), value);
}

/// `rows` with the chip at `from` moved to `to`, both (row, index)
/// places into the rows as they stand before the move.
fn moved_at<V: PartialEq + Copy>(
    registry: &Arrangement<V>,
    rows: &[Vec<V>],
    from: (usize, usize),
    to: (usize, usize),
) -> Vec<Vec<V>> {
    let mut rows = rows.to_vec();
    let Some(value) = rows.get(from.0).and_then(|row| row.get(from.1)).copied() else {
        return rows;
    };
    rows[from.0].remove(from.1);
    let (to_row, mut to_ix) = to;
    // Only a same-row forward move has to account for the removed slot.
    if to_row == from.0 && from.1 < to_ix {
        to_ix -= 1;
    }
    let unique = !registry.repeats(value);
    if let Some(row) = rows.get_mut(to_row) {
        insert_row_unique(row, value, to_ix, unique);
    }
    rows
}

/// `rows` with `value` inserted at `to`, the tray-to-well drop.
fn inserted<V: PartialEq + Copy>(
    registry: &Arrangement<V>,
    rows: &[Vec<V>],
    value: V,
    to: (usize, usize),
) -> Vec<Vec<V>> {
    let mut rows = rows.to_vec();
    let unique = !registry.repeats(value);
    if let Some(row) = rows.get_mut(to.0) {
        insert_row_unique(row, value, to.1, unique);
    }
    rows
}

/// `rows` without the chip at `at`.
fn removed_at<V: Copy>(rows: &[Vec<V>], at: (usize, usize)) -> Vec<Vec<V>> {
    let mut rows = rows.to_vec();
    if let Some(row) = rows.get_mut(at.0)
        && at.1 < row.len()
    {
        row.remove(at.1);
    }
    rows
}

/// `rows` without row `at`, the empty well's x. The last row stays, so
/// the editor always shows at least one well.
fn removed_row<V: Copy>(rows: &[Vec<V>], at: usize) -> Vec<Vec<V>> {
    let mut rows = rows.to_vec();
    if rows.len() > 1 && at < rows.len() {
        rows.remove(at);
    }
    rows
}

/// `items` without `value`, every copy of it: the menu toggles' hide.
fn without<V: PartialEq + Copy>(items: &[V], value: V) -> Vec<V> {
    items.iter().copied().filter(|v| *v != value).collect()
}

/// `items` with `value` slotted at its stock position: after every shown
/// item that precedes it in the catalog. On a list still in catalog
/// order that restores exactly where the item used to be; on a
/// rearranged list it stays deterministic.
fn insert_stock<V: PartialEq + Copy>(registry: &Arrangement<V>, items: &[V], value: V) -> Vec<V> {
    let target = registry.rank(value);
    let at = items.iter().filter(|v| registry.rank(**v) < target).count();
    let mut items = items.to_vec();
    items.insert(at.min(items.len()), value);
    items
}

/// Show or hide `value` on the list: the panels' quick menu toggles go
/// through this, hiding a shown item and slotting a hidden one back at its
/// stock position.
pub fn toggled<V: PartialEq + Copy + 'static>(
    registry: impl Into<Arrangement<V>>,
    items: &[V],
    value: V,
) -> Vec<V> {
    if items.contains(&value) {
        without(items, value)
    } else {
        insert_stock(&registry.into(), items, value)
    }
}

/// Put a stashed arrangement back, if it still describes the row. `stash`
/// is what the list looked like before a hide took `hidden` out of it, and
/// restoring it is only right while nothing else has moved since: the
/// stash minus the hidden values has to still match the live list exactly.
/// None means it doesn't.
fn restored<V: PartialEq + Copy>(stash: &[V], items: &[V], hidden: &[V]) -> Option<Vec<V>> {
    let kept: Vec<V> = stash
        .iter()
        .copied()
        .filter(|v| !hidden.contains(v))
        .collect();
    (kept == items).then(|| stash.to_vec())
}

/// [`toggled`] with the arrangement kept, the one the panels' quick menus
/// call. `values` moves as one group: shown if any of them is on the row.
/// Hiding stashes the row first, so showing can put it back whole instead
/// of slotting each value at its catalog rank, which on a hand-arranged
/// row is the wrong place and is the whole reason this exists.
///
/// One stash slot per toggle, so hiding a second group forgets the first.
/// [`restored`] catches that, along with any edit made while the group was
/// hidden, and the stock insert takes over.
pub fn toggled_stashed<V: PartialEq + Copy + 'static>(
    registry: impl Into<Arrangement<V>>,
    items: &[V],
    stash: &mut Option<Vec<V>>,
    values: &[V],
) -> Vec<V> {
    if values.iter().any(|value| items.contains(value)) {
        *stash = Some(items.to_vec());
        return items
            .iter()
            .copied()
            .filter(|value| !values.contains(value))
            .collect();
    }
    if let Some(kept) = stash
        .take()
        .and_then(|stash| restored(&stash, items, values))
    {
        return kept;
    }
    let registry = registry.into();
    let mut out = items.to_vec();
    for value in values {
        if !out.contains(value) {
            out = insert_stock(&registry, &out, *value);
        }
    }
    out
}

/// Drop repeated values from a dump's list, keeping first positions, so a
/// hand-edited layout can't render an item twice. Items the catalog marks
/// repeatable pass through as often as they appear.
pub fn dedup<V: PartialEq + Copy + 'static>(
    registry: impl Into<Arrangement<V>>,
    items: Vec<V>,
) -> Vec<V> {
    let registry = registry.into();

    let mut out: Vec<V> = Vec::with_capacity(items.len());
    for item in items {
        if registry.repeats(item) || !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// The flat editor most strips use: [`arrange_rows_editor`] capped at one
/// well, so `apply` gets the single row back as the plain list it stores.
pub fn arrange_editor<P: 'static, V: PartialEq + Copy + 'static>(
    id: &'static str,
    registry: impl Into<Arrangement<V>>,
    items: &[V],
    apply: impl Fn(&mut P, Vec<V>, &mut Context<P>) + Clone + 'static,
    cx: &mut Context<P>,
) -> Stateful<Div> {
    arrange_rows_editor(
        id,
        registry,
        &[items.to_vec()],
        Some(1),
        move |this, rows, cx| apply(this, rows.into_iter().next().unwrap_or_default(), cx),
        cx,
    )
}

/// The editor itself: one well per row over the hidden tray, and while
/// `max_rows` allows another, a button below the wells that opens one.
/// A drag moves a chip along its well or into any other, an empty well
/// keeps an x that drops the row, and `apply` stores the edited rows and
/// notifies. `id` names this editor instance so a drag never lands in
/// another window's copy, and it scopes the whole subtree's element ids:
/// the chips key on their labels, so two editors on one page would
/// otherwise share a chip's drag state.
pub fn arrange_rows_editor<P: 'static, V: PartialEq + Copy + 'static>(
    id: &'static str,
    registry: impl Into<Arrangement<V>>,
    rows: &[Vec<V>],
    max_rows: Option<usize>,
    apply: impl Fn(&mut P, Vec<Vec<V>>, &mut Context<P>) + Clone + 'static,
    cx: &mut Context<P>,
) -> Stateful<Div> {
    // At least one well, so an emptied config still has a drop target.
    let rows: Vec<Vec<V>> = if rows.is_empty() {
        vec![Vec::new()]
    } else {
        rows.to_vec()
    };

    // Resolved once for the whole subtree. The handlers each keep a
    // refcounted copy of the registry itself, since a drop rearranges
    // the rows long after this render is gone.
    let registry = registry.into();
    let entries = registry.entries();

    // The wells: every shown chip drags, drops before the chip it lands
    // on, and hides on its x. The tail past a well's last chip catches
    // drops meant for the end of that row.
    let mut wells = div().flex().flex_col().gap(tokens::SPACE_XS);
    for (row_ix, row) in rows.iter().enumerate() {
        let mut bar = well();
        for (ix, value) in row.iter().copied().enumerate() {
            let Some(entry) = entries.iter().find(|entry| entry.value == value) else {
                continue;
            };
            let drag = ArrangeDrag {
                editor: id,
                value,
                from: Some((row_ix, ix)),
                label: entry.label.clone(),
                icon: entry.icon,
            };
            let drop_registry = registry.clone();
            let drop_rows = rows.clone();
            let drop_apply = apply.clone();
            let hide_rows = rows.clone();
            let hide_apply = apply.clone();
            bar = bar.child(
                // Keyed by position as well as label: two spacers on a
                // well are two chips, and sharing an id would share their
                // drag state. The place folds to one integer for the id.
                chip(entry.label.clone(), entry.icon, false)
                    .id((entry.id.clone(), (row_ix << 16) | ix))
                    .cursor_pointer()
                    .on_drag(drag, |drag, _pos, _window, cx| {
                        cx.new(|_| ChipPreview {
                            label: drag.label.clone(),
                            icon: drag.icon,
                        })
                    })
                    .drag_over::<ArrangeDrag<V>>(move |style, drag, _, _| {
                        if drag.editor == id {
                            style.bg(palette::alpha(palette::accent(), 0x33))
                        } else {
                            style
                        }
                    })
                    .on_drop(cx.listener(move |this, drag: &ArrangeDrag<V>, _, cx| {
                        if drag.editor != id {
                            return;
                        }
                        let rows = match drag.from {
                            Some(from) => moved_at(&drop_registry, &drop_rows, from, (row_ix, ix)),
                            None => inserted(&drop_registry, &drop_rows, drag.value, (row_ix, ix)),
                        };
                        drop_apply(this, rows, cx);
                    }))
                    .child(chip_action(icons::CLOSE).on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            hide_apply(this, removed_at(&hide_rows, (row_ix, ix)), cx);
                        }),
                    )),
            );
        }
        let tail_registry = registry.clone();
        let tail_rows = rows.clone();
        let tail_apply = apply.clone();
        let tail_to = (row_ix, row.len());
        bar = bar.child(
            div()
                .flex_1()
                .min_w(px(24.))
                .h(px(26.))
                .rounded(tokens::RADIUS)
                .drag_over::<ArrangeDrag<V>>(move |style, drag, _, _| {
                    if drag.editor == id {
                        style.bg(palette::alpha(palette::accent(), 0x33))
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(move |this, drag: &ArrangeDrag<V>, _, cx| {
                    if drag.editor != id {
                        return;
                    }
                    let rows = match drag.from {
                        Some(from) => moved_at(&tail_registry, &tail_rows, from, tail_to),
                        None => inserted(&tail_registry, &tail_rows, drag.value, tail_to),
                    };
                    tail_apply(this, rows, cx);
                })),
        );
        if row.is_empty() && rows.len() > 1 {
            let drop_rows = rows.clone();
            let drop_apply = apply.clone();
            bar = bar.child(chip_action(icons::CLOSE).on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    drop_apply(this, removed_row(&drop_rows, row_ix), cx);
                }),
            ));
        }
        wells = wells.child(bar);
    }
    if max_rows.is_none_or(|max| rows.len() < max) {
        let add_rows = rows.clone();
        let add_apply = apply.clone();
        let drop_registry = registry.clone();
        let drop_rows = rows.clone();
        let drop_apply = apply.clone();
        wells = wells.child(
            div()
                .id("add-row")
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(tokens::SPACE_XS)
                .p(tokens::SPACE_XS)
                .min_h(px(36.))
                .rounded(tokens::RADIUS)
                .border_1()
                .border_dashed()
                .border_color(palette::border())
                .cursor_pointer()
                .hover(|s| s.border_color(palette::border_light()))
                .child(
                    svg()
                        .path(icons::PLUS)
                        .size(px(12.))
                        .text_color(palette::text_faint()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette::text_faint())
                        .child(rox_i18n::t!("arrange-add-row")),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut rows = add_rows.clone();
                    rows.push(Vec::new());
                    add_apply(this, rows, cx);
                }))
                // A chip dropped on the button starts its row with it.
                .drag_over::<ArrangeDrag<V>>(move |style, drag, _, _| {
                    if drag.editor == id {
                        style.bg(palette::alpha(palette::accent(), 0x33))
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(move |this, drag: &ArrangeDrag<V>, _, cx| {
                    if drag.editor != id {
                        return;
                    }
                    let mut rows = drop_rows.clone();
                    rows.push(Vec::new());
                    let to = (rows.len() - 1, 0);
                    let rows = match drag.from {
                        Some(from) => moved_at(&drop_registry, &rows, from, to),
                        None => inserted(&drop_registry, &rows, drag.value, to),
                    };
                    drop_apply(this, rows, cx);
                })),
        );
    }

    // The tray: the chips some open row still lacks, dimmed, and the
    // repeatable ones always. Uniqueness is per row, so a piece shown on
    // one line stays offered until every line holds it. A drop from a
    // well hides the dragged item; a chip's plus (or a drag up into a
    // well) shows it at its stock position on the first row without it.
    let tray_rows = rows.clone();
    let tray_apply = apply.clone();
    let mut tray = well()
        .drag_over::<ArrangeDrag<V>>(move |style, drag, _, _| {
            if drag.editor == id {
                style.bg(palette::alpha(palette::accent(), 0x1a))
            } else {
                style
            }
        })
        .on_drop(cx.listener(move |this, drag: &ArrangeDrag<V>, _, cx| {
            if drag.editor != id {
                return;
            }
            // A tray chip dropped back on the tray has nothing to hide.
            let Some(from) = drag.from else {
                return;
            };
            tray_apply(this, removed_at(&tray_rows, from), cx);
        }));
    for entry in entries
        .iter()
        .filter(|entry| entry.repeats || rows.iter().any(|row| !row.contains(&entry.value)))
    {
        let drag = ArrangeDrag {
            editor: id,
            value: entry.value,
            from: None,
            label: entry.label.clone(),
            icon: entry.icon,
        };
        let show_registry = registry.clone();
        let show_rows = rows.clone();
        let show_apply = apply.clone();
        let value = entry.value;
        tray = tray.child(
            chip(entry.label.clone(), entry.icon, true)
                .id(entry.id.clone())
                .cursor_pointer()
                .on_drag(drag, |drag, _pos, _window, cx| {
                    cx.new(|_| ChipPreview {
                        label: drag.label.clone(),
                        icon: drag.icon,
                    })
                })
                .child(chip_action(icons::PLUS).on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        let mut rows = show_rows.clone();
                        let target = rows
                            .iter()
                            .position(|row| !row.contains(&value))
                            .unwrap_or(0);
                        rows[target] = insert_stock(&show_registry, &rows[target], value);
                        show_apply(this, rows, cx);
                    }),
                )),
        );
    }

    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(tokens::SPACE_XS)
        .child(caption(rox_i18n::t!("arrange-shown")))
        .child(wells)
        .child(caption(rox_i18n::t!("arrange-hidden")))
        .child(tray)
}

#[cfg(test)]
mod tests {
    use gpui::SharedString;

    use super::{
        ArrangeEntry, ArrangeSpec, Arrangement, dedup, insert_stock, inserted, moved_at,
        removed_at, removed_row, toggled, toggled_stashed, without,
    };

    /// The place ops that read the catalog for repeatability, pinned to
    /// the test registry.
    fn moved(rows: &[Vec<u8>], from: (usize, usize), to: (usize, usize)) -> Vec<Vec<u8>> {
        moved_at(&Arrangement::Stock(REGISTRY), rows, from, to)
    }
    fn insert(rows: &[Vec<u8>], value: u8, to: (usize, usize)) -> Vec<Vec<u8>> {
        inserted(&Arrangement::Stock(REGISTRY), rows, value, to)
    }
    fn stock(items: &[u8], value: u8) -> Vec<u8> {
        insert_stock(&Arrangement::Stock(REGISTRY), items, value)
    }

    /// Value 3 stands in for a spacer: the one repeatable entry.
    const REGISTRY: &[ArrangeSpec<u8>] = &[
        ArrangeSpec {
            key: "a",
            icon: None,
            value: 0,
            repeats: false,
        },
        ArrangeSpec {
            key: "b",
            icon: None,
            value: 1,
            repeats: false,
        },
        ArrangeSpec {
            key: "c",
            icon: None,
            value: 2,
            repeats: false,
        },
        ArrangeSpec {
            key: "d",
            icon: None,
            value: 3,
            repeats: true,
        },
    ];

    /// Moving forward accounts for the removed slot, so a drop lands where
    /// the pointer was, and moving to the tail appends. The place ops
    /// leave every other copy of a repeated value where it stood.
    #[test]
    fn place_ops_reorder_insert_and_remove() {
        assert_eq!(moved(&[vec![0, 1, 2]], (0, 0), (0, 2)), [vec![1, 0, 2]]);
        assert_eq!(moved(&[vec![0, 1, 2]], (0, 2), (0, 0)), [vec![2, 0, 1]]);
        assert_eq!(moved(&[vec![0, 1, 2]], (0, 0), (0, 3)), [vec![1, 2, 0]]);
        // Two copies keep their identities: moving the second leaves the
        // first, and removing by place takes just the one.
        assert_eq!(moved(&[vec![3, 0, 3]], (0, 2), (0, 0)), [vec![3, 3, 0]]);
        assert_eq!(insert(&[vec![0, 3]], 3, (0, 1)), [vec![0, 3, 3]]);
        assert_eq!(removed_at(&[vec![3, 0, 3]], (0, 2)), [vec![3, 0]]);
    }

    /// A chip moved across rows lands where the pointer was; only a
    /// same-row forward move adjusts for the removed slot. A row drops
    /// whole, except the last one.
    #[test]
    fn row_ops_move_across_and_drop_rows() {
        assert_eq!(
            moved(&[vec![0, 1], vec![2]], (0, 1), (1, 0)),
            [vec![0], vec![1, 2]]
        );
        assert_eq!(
            moved(&[vec![0], vec![1, 2]], (1, 0), (0, 1)),
            [vec![0, 1], vec![2]]
        );
        assert_eq!(removed_row(&[vec![0], vec![]], 1), [vec![0]]);
        assert_eq!(removed_row::<u8>(&[vec![0]], 0), [vec![0]]);
    }

    /// Uniqueness is per row: a non-repeatable value landing on a row
    /// that already holds it replaces that row's copy, wherever it was,
    /// while another row's copy stays; a repeatable value stacks.
    #[test]
    fn row_landings_keep_a_row_unique() {
        assert_eq!(
            moved(&[vec![0, 1], vec![0]], (1, 0), (0, 2)),
            [vec![1, 0], vec![]]
        );
        assert_eq!(
            insert(&[vec![0, 1], vec![0]], 0, (0, 2)),
            [vec![1, 0], vec![0]]
        );
        assert_eq!(insert(&[vec![3, 0]], 3, (0, 2)), [vec![3, 0, 3]]);
    }

    /// A re-shown item slots back after every shown item that precedes it
    /// in the catalog, restoring stock order on a stock list.
    #[test]
    fn stock_insert_restores_catalog_order() {
        assert_eq!(stock(&[0, 1, 3], 2), vec![0, 1, 2, 3]);
        assert_eq!(stock(&[1, 2], 0), vec![0, 1, 2]);
    }

    /// Duplicates in a hand-edited dump collapse to first positions,
    /// except the repeatable entry, which keeps every copy.
    #[test]
    fn dedup_keeps_first_positions_and_repeats() {
        assert_eq!(dedup(REGISTRY, vec![2, 0, 2, 1, 0]), vec![2, 0, 1]);
        assert_eq!(dedup(REGISTRY, vec![3, 0, 3, 3]), vec![3, 0, 3, 3]);
        assert_eq!(without(&[0, 1, 2], 1), vec![0, 2]);
    }

    /// The round trip a quick menu makes: a hand-arranged row comes back
    /// as itself, where the stock insert would have put the item at its
    /// catalog rank instead. The stash is spent on the way back, so a
    /// second show falls through to that stock insert.
    #[test]
    fn stashed_toggle_returns_a_rearranged_row() {
        let row = vec![2, 0, 1];
        let mut stash = None;
        let hidden = toggled_stashed(REGISTRY, &row, &mut stash, &[0]);
        assert_eq!(hidden, vec![2, 1]);
        assert_eq!(toggled_stashed(REGISTRY, &hidden, &mut stash, &[0]), row);
        assert_eq!(stock(&hidden, 0), vec![0, 2, 1]);
        assert!(stash.is_none(), "the stash is spent once it's used");
    }

    /// Both clocks of the seek row move as one group, and come back as one.
    #[test]
    fn stashed_toggle_moves_a_group() {
        let row = vec![2, 0, 3, 1];
        let mut stash = None;
        let hidden = toggled_stashed(REGISTRY, &row, &mut stash, &[0, 1]);
        assert_eq!(hidden, vec![2, 3]);
        assert_eq!(toggled_stashed(REGISTRY, &hidden, &mut stash, &[0, 1]), row);
    }

    /// An edit made while the item was hidden invalidates the stash: the
    /// row it describes isn't the row on screen any more, so putting it
    /// back would undo the edit. The stock insert takes over instead.
    #[test]
    fn an_edit_under_the_stash_falls_back_to_stock() {
        let mut stash = None;
        let hidden = toggled_stashed(REGISTRY, &[2, 0, 1], &mut stash, &[0]);
        assert_eq!(hidden, vec![2, 1]);
        let edited = vec![1, 2];
        assert_eq!(
            toggled_stashed(REGISTRY, &edited, &mut stash, &[0]),
            vec![0, 1, 2],
            "stock rank, not the stale stash"
        );
    }

    /// No stash at all is the first toggle after a launch, and it behaves
    /// exactly like the plain toggle it replaced.
    #[test]
    fn a_cold_stash_matches_the_plain_toggle() {
        let mut stash = None;
        assert_eq!(
            toggled_stashed(REGISTRY, &[2, 1], &mut stash, &[0]),
            toggled(REGISTRY, &[2, 1], 0)
        );
    }

    /// A catalog built while the app runs answers the same questions the
    /// static one does. This is the custom controls panel's case, where
    /// the items are user-made buttons keyed by their persisted id.
    #[test]
    fn a_runtime_registry_behaves_like_a_static_one() {
        let built: Vec<ArrangeEntry<u8>> = [(0u8, false), (1, false), (2, false), (3, true)]
            .iter()
            .map(|(value, repeats)| ArrangeEntry {
                id: SharedString::from(format!("b{value}")),
                label: SharedString::from(format!("Button {value}")),
                icon: None,
                value: *value,
                repeats: *repeats,
            })
            .collect();
        let registry = Arrangement::from(built);

        // The same three answers the static registry gives, off a
        // registry the compiler never saw.
        assert_eq!(dedup(registry.clone(), vec![2, 0, 2, 1, 0]), vec![2, 0, 1]);
        assert_eq!(dedup(registry.clone(), vec![3, 0, 3, 3]), vec![3, 0, 3, 3]);
        assert_eq!(
            inserted(&registry, &[vec![0, 1]], 2, (0, 1)),
            [vec![0, 2, 1]]
        );
        assert_eq!(insert_stock(&registry, &[0, 1, 3], 2), vec![0, 1, 2, 3]);
    }
}
