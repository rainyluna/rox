//! The Custom Controls panel: a strip of buttons the user builds. Each one
//! fires a global rox command on click, and its glyph, colour, and tooltip
//! follow a piece of live player state through a case table. There are no
//! expressions and no scripting anywhere in it: the state catalog
//! ([`rox_panel_api::buttons`]) declares every case a state can be in, the
//! editor generates one row per case off that, and the user only ever picks
//! from lists.
//!
//! It lives in the binary rather than in `rox-panels` because it calls
//! [`keymap::dispatch`], and `keymap` is the binary's. Nothing depends on
//! the `rox` crate, so a panel over in `rox-panels` cannot see the command
//! table at all, and handing that crate a dependency on the binary would
//! invert the layering the whole panel split exists to hold. This is the
//! line `panels/mod.rs` already draws: what stays here is what calls into
//! here for real.
//!
//! What it deliberately isn't: a way into the transport strips. Those carry
//! `Copy` item enums over a `&'static` catalog and cannot grow at runtime,
//! which is the entire reason this is a panel.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, Rgba,
    SharedString, Stateful, Subscription, WeakEntity, Window, div, prelude::*, px, svg,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenu;
use rox_dock::{Panel, PanelEvent, TabPanel};
use serde::{Deserialize, Serialize};

use rox_design::assets::icons;
use rox_design::{palette, tokens};
use rox_panel_api::buttons::{self, StateSpec};
use rox_panel_api::panel::{self, AppState, PanelChrome, PanelSettings};
use rox_panel_api::panel_settings;
use rox_panel_kit::ui::{self as settings_ui, SECTION_GAP, section};
use rox_panel_kit::{
    Align, PickRow, Tip, icon_picker, justify, picker, search_picker, setting_block, setting_row,
};

use crate::keymap::{self, Group};

/// The glyph size a button draws at rest, matching the stock transport
/// controls so a custom button sits in the same strip without reading as a
/// different widget.
const GLYPH: f32 = 16.0;

/// The panel's per-view config: what a saved layout restores and what the
/// settings window edits.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlsConfig {
    /// The rename, theme override, and placement locks shared by every
    /// panel.
    #[serde(flatten)]
    pub chrome: PanelChrome,
    /// Every button the user has defined, in the order the Controls page
    /// lists them. Defining one is not placing it.
    pub buttons: Vec<CustomButton>,
    /// The strip in display order: the Layout page's well, holding buttons
    /// by id and the stock furniture between them. A button whose id isn't
    /// here sits in that page's tray, defined and not drawn, which is where
    /// a new one starts. No migration reads an older shape into it, because
    /// no layout rox has ever shipped carries this panel.
    pub items: Vec<WellItem>,
    /// Where the strip sits when the panel is wider than its buttons, the
    /// name and the vocabulary `TransportConfig` already uses for it.
    pub align: Align,
}

impl Default for ControlsConfig {
    /// Centred rather than `Align`'s own left default: a handful of
    /// buttons hugging one edge of a strip reads as unfinished.
    fn default() -> Self {
        ControlsConfig {
            chrome: PanelChrome::default(),
            buttons: Vec::new(),
            items: Vec::new(),
            align: Align::Center,
        }
    }
}

/// One slot of the well: a button by id, or the furniture every strip
/// offers between its items. Untagged so a saved well reads as `[3,
/// "spacer", 7]`: a bare number is a button, a word is furniture.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WellItem {
    Button(u64),
    Furniture(Furniture),
}

/// The stock pieces between buttons, the same two the head strips carry.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Furniture {
    Spacer,
    Divider,
}

/// One slot of the strip as it draws: a definition the well named, or a
/// piece of furniture.
enum Placed<'a> {
    Button(&'a CustomButton),
    Furniture(Furniture),
}

/// One button: what it does, what it watches, and how it looks in each
/// case of that.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CustomButton {
    /// A stable handle, unique within the panel and persisted, so the
    /// editor's per-button widget state survives a reorder or a removal
    /// shifting the list under it. 0 is unassigned; the panel assigns on
    /// load and on add.
    pub id: u64,
    /// A `keymap::COMMANDS` id. Empty means unconfigured.
    pub action: String,
    /// A `buttons::STATES` id. Empty means stateless, and `cases` then
    /// holds exactly one entry used for every draw.
    pub state: String,
    pub cases: Vec<ButtonCase>,
}

/// One row of a button's case table: the look it wears while the state it
/// follows is in that case.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ButtonCase {
    /// The `StateCase` id this draws for. Empty for the stateless entry.
    pub when: String,
    /// An `icons::CATALOG` path.
    pub icon: String,
    /// A `palette::ROLES` name. Empty falls back to `text`.
    pub color: String,
    /// User text, shown as the tooltip. Not translated: a user-authored
    /// button in a user-authored workspace is in their own language.
    pub tip: String,
}

/// Give every button a unique id, keeping the ones a loaded config already
/// has: zeroes (a freshly added button, or a layout from before the field
/// existed) and hand-edited duplicates get fresh ones. Copied from
/// particles' `assign_emitter_ids`, which solves the same problem for the
/// same reason.
fn assign_button_ids(buttons: &mut [CustomButton]) {
    let mut next = buttons.iter().map(|def| def.id).max().unwrap_or(0) + 1;
    for i in 0..buttons.len() {
        let taken = buttons[..i].iter().any(|def| def.id == buttons[i].id);
        if buttons[i].id == 0 || taken {
            buttons[i].id = next;
            next += 1;
        }
    }
}

/// The well pruned to what the panel actually has: an id no button
/// carries drops out, and a second mention of one already placed drops
/// with it, first occurrence winning. A hand-edited layout is the reason
/// this runs on load; an id left behind would hold a place on the strip
/// that nothing can ever draw.
fn normalize_items(items: &[WellItem], buttons: &[CustomButton]) -> Vec<WellItem> {
    let mut out: Vec<WellItem> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            // Furniture repeats freely; a button needs a definition behind
            // it and one place on the strip.
            WellItem::Furniture(_) => out.push(*item),

            WellItem::Button(id) => {
                let defined = buttons.iter().any(|def| def.id == *id);
                if defined && !out.contains(item) {
                    out.push(*item);
                }
            }
        }
    }

    out
}

/// What the strip draws, in well order rather than list order. An id with
/// nothing behind it is skipped, which after [`normalize_items`] means it
/// can't happen and is cheap to keep honest anyway.
fn placed<'a>(items: &[WellItem], buttons: &'a [CustomButton]) -> Vec<Placed<'a>> {
    items
        .iter()
        .filter_map(|item| match item {
            WellItem::Furniture(kind) => Some(Placed::Furniture(*kind)),
            WellItem::Button(id) => buttons.iter().find(|def| def.id == *id).map(Placed::Button),
        })
        .collect()
}

/// Drop the button at `index`, off the definitions and off the well with
/// it. The well is what the strip draws from, so a delete that left the id
/// there would leave a hole that only a reload cleans up.
fn remove_at(buttons: &mut Vec<CustomButton>, items: &mut Vec<WellItem>, index: usize) {
    if index >= buttons.len() {
        return;
    }

    let gone = WellItem::Button(buttons.remove(index).id);
    items.retain(|item| *item != gone);
}

/// The Layout page's registry: one chip per defined button, in the order
/// the Controls page lists them, named by the command it fires and wearing
/// the glyph of its first case, then the two pieces of furniture. Built
/// per render, since a button's command and icon change under the editor
/// while the page is open.
fn well_registry(buttons: &[CustomButton]) -> Vec<panel::ArrangeEntry<WellItem>> {
    let mut entries: Vec<_> = buttons
        .iter()
        .map(|def| {
            let label = if def.action.is_empty() {
                rox_i18n::t!("button-editor-unnamed")
            } else {
                command_label(&def.action)
            };

            // Keyed by the button's own id, not its label: two buttons
            // firing one command are two chips, and a renamed command
            // mid-drag would otherwise move the chip out from under the
            // pointer.
            panel::ArrangeEntry {
                id: SharedString::from(format!("b{}", def.id)),
                label,
                icon: Some(icon_for(
                    def.cases
                        .first()
                        .map(|case| case.icon.as_str())
                        .unwrap_or(""),
                )),
                value: WellItem::Button(def.id),
                repeats: false,
            }
        })
        .collect();

    // The furniture after the buttons, so the tray reads as the user's
    // own things first. Both repeat: a strip wants as many as it wants.
    entries.push(panel::ArrangeEntry {
        id: SharedString::new_static("spacer"),
        label: rox_i18n::t!("head-piece-spacer"),
        icon: Some(icons::MOVE_HORIZONTAL),
        value: WellItem::Furniture(Furniture::Spacer),
        repeats: true,
    });
    entries.push(panel::ArrangeEntry {
        id: SharedString::new_static("divider"),
        label: rox_i18n::t!("head-piece-divider"),
        icon: Some(icons::MINUS),
        value: WellItem::Furniture(Furniture::Divider),
        repeats: true,
    });

    entries
}

/// The catalog entry a stored icon path names. `icon_control`'s glyph is a
/// `&'static str` and the config's is a `String`, so the stored path is
/// matched against the catalog and the catalog's own static is what draws.
/// A hand-edited config naming an icon that isn't there degrades to the
/// unconfigured placeholder instead of failing to build an element.
fn icon_for(path: &str) -> &'static str {
    icons::CATALOG
        .iter()
        .find(|entry| **entry == path)
        .copied()
        .unwrap_or(icons::SQUARE_DASHED)
}

/// The colour a case's role name resolves to against the live palette.
/// Empty or unmatched falls back to plain text, so a role dropped from the
/// palette leaves a readable button rather than an invisible one.
fn color_for(role: &str) -> Rgba {
    let Some(entry) = palette::ROLES.iter().find(|entry| entry.name == role) else {
        return palette::text();
    };

    (entry.get)(&palette::resolved())
}

/// The state a button follows, or None when it's stateless or names a
/// state this build doesn't have.
fn state_spec(id: &str) -> Option<&'static StateSpec> {
    buttons::STATES.iter().find(|spec| spec.id == id)
}

/// A command's name for the picker and the tooltip fallback, or the raw id
/// when a saved layout names a command that has since been retired.
fn command_label(id: &str) -> SharedString {
    keymap::COMMANDS
        .iter()
        .find(|command| command.id == id)
        .map(|command| SharedString::from(command.label))
        .unwrap_or_else(|| SharedString::from(id.to_owned()))
}

/// What one button looks like this frame, once the case table has picked
/// its row.
struct ButtonLook {
    icon: &'static str,
    color: Rgba,
    tip: SharedString,
}

impl ButtonLook {
    /// The unconfigured button: a dashed square in muted ink, whose click
    /// opens the settings page where it gets a command.
    fn placeholder() -> Self {
        ButtonLook {
            icon: icons::SQUARE_DASHED,
            color: palette::text_muted(),
            tip: rox_i18n::t!("panel-controls-unconfigured"),
        }
    }
}

/// The look a button draws with, in the contract's resolution order: an
/// unconfigured button is the placeholder, the live case picks the row,
/// and a table with nothing matching falls back to its first row and then
/// to the placeholder again. `case` is None for a stateless button, which
/// is what puts it on the first row every draw.
fn look(def: &CustomButton, case: Option<&str>) -> ButtonLook {
    if def.action.is_empty() {
        return ButtonLook::placeholder();
    }

    let row = case
        .and_then(|id| def.cases.iter().find(|row| row.when == id))
        .or_else(|| def.cases.first());

    let Some(row) = row else {
        return ButtonLook::placeholder();
    };

    // An untouched tooltip reads as the command's own name. A glyph with
    // no words on hover says nothing to anyone who didn't place it, which
    // is the case `Tip` exists to refuse.
    let tip = if row.tip.is_empty() {
        command_label(&def.action)
    } else {
        SharedString::from(row.tip.clone())
    };

    ButtonLook {
        icon: icon_for(&row.icon),
        color: color_for(&row.color),
        tip,
    }
}

/// The case table a state's pick produces: one row per case the state
/// declares, seeded with that case's stock icon and colour, and keeping
/// whatever the user already edited for a case the new state still has.
/// Silently discarding someone's icon work is worse than a slightly stale
/// table, so nothing here asks before it writes.
///
/// A stateless pick collapses to the single row every draw uses.
fn seed_cases(spec: Option<&StateSpec>, held: &[ButtonCase]) -> Vec<ButtonCase> {
    let Some(spec) = spec else {
        let kept = held.iter().find(|row| row.when.is_empty()).cloned();
        return vec![kept.unwrap_or_else(|| ButtonCase {
            // Named rather than left empty: the colour picker labels a role
            // it can't find with its head option, which would read as a
            // colour the button never had.
            color: "text".to_string(),
            ..ButtonCase::default()
        })];
    };

    spec.cases
        .iter()
        .map(|case| {
            held.iter()
                .find(|row| row.when == case.id)
                .cloned()
                .unwrap_or(ButtonCase {
                    when: case.id.to_string(),
                    icon: case.icon.to_string(),
                    color: case.color.to_string(),
                    tip: String::new(),
                })
        })
        .collect()
}

/// Prefill the click from the state's obvious command, into an empty field
/// only: an action the user already chose is never overwritten. A state
/// with no single obvious command (`player.continuation`, which no keymap
/// entry toggles) leaves the field alone.
fn prefill_action(action: &mut String, spec: Option<&StateSpec>) {
    if !action.is_empty() {
        return;
    }

    if let Some(spec) = spec.filter(|spec| !spec.action.is_empty()) {
        *action = spec.action.to_string();
    }
}

/// Every command a button may fire, as picker rows, grouped the way the
/// Keymap page groups them. Built once: `COMMANDS` bakes its labels
/// through its own `LazyLock` at first use, so these rows hold exactly as
/// long as those strings do, and a settings render has no business
/// rebuilding fifty-nine rows a frame.
fn command_rows() -> Arc<Vec<PickRow>> {
    static ROWS: OnceLock<Arc<Vec<PickRow>>> = OnceLock::new();

    ROWS.get_or_init(|| {
        let mut rows = Vec::new();
        for group in Group::ALL {
            for command in keymap::global_commands().filter(|c| c.group == *group) {
                rows.push(command_row(command));
            }
        }
        Arc::new(rows)
    })
    .clone()
}

/// One command as a row. The id goes in the search terms beside the label
/// words, so someone who knows a command by its settings key finds it
/// without knowing what the menu calls it.
fn command_row(command: &'static keymap::Command) -> PickRow {
    let label = command.label.to_lowercase();

    let mut terms: Vec<SharedString> = vec![command.id.to_lowercase().into()];
    terms.extend(
        label
            .split_whitespace()
            .map(|word| SharedString::from(word.to_owned())),
    );
    terms.push(label.into());

    PickRow {
        label: SharedString::from(command.label),
        value: Some(SharedString::from(command.id)),
        terms,
        icon: Some(SharedString::from(command.group.icon())),
    }
}

/// The handler a button's click runs. An unconfigured button opens the
/// panel's own settings instead of doing nothing, since a placed-but-blank
/// button that swallows a press reads as broken.
fn press(
    action: String,
) -> impl Fn(&mut ControlsPanel, &mut Window, &mut Context<ControlsPanel>) + 'static {
    move |_, window, cx| {
        if action.is_empty() {
            // Deferred for the reason `opens_settings!` defers: `open`
            // reads the panel back through its handle, and this runs while
            // that same entity is still leased for the update.
            let panel = cx.entity();
            cx.defer(move |cx| panel_settings::open(panel, cx));
            return;
        }

        keymap::dispatch(&action, window, cx);
    }
}

/// One button as it draws. The settings page's preview calls this too, so
/// what the editor shows and what the strip shows can't drift apart.
///
/// The glyph chrome is `panel::icon_control`'s, rebuilt here rather than
/// called: that builder drops the window on its way into the click
/// handler, and [`keymap::dispatch`] needs one to dispatch at. Interaction
/// state comes from `.id()` and not `.track_focus()`, which is what lets a
/// press here land without pulling the cursor out of a search box.
fn button_element(
    id: SharedString,
    look: &ButtonLook,
    on_click: impl Fn(&mut ControlsPanel, &mut Window, &mut Context<ControlsPanel>) + 'static,
    cx: &mut Context<ControlsPanel>,
) -> Stateful<Div> {
    let color = look.color;
    let icon = look.icon;

    let body = div()
        .p(tokens::ICON_PAD)
        .rounded(tokens::RADIUS)
        .hover(|d| d.bg(palette::bg_control()))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| on_click(this, window, cx)),
        )
        .child(svg().path(icon).size(px(GLYPH)).text_color(color));

    Tip::keyed(id, look.tip.clone()).apply(body)
}

pub struct ControlsPanel {
    state: AppState,
    config: ControlsConfig,
    focus: FocusHandle,
    tab_panel: Option<WeakEntity<TabPanel>>,
    /// The tooltip fields, keyed by button id and case so a reorder or a
    /// removal leaves the rest of them where they were.
    tips: HashMap<(u64, String), (Entity<InputState>, Subscription)>,
    /// The blocks unfolded on the settings page. Per view and never saved:
    /// a reopened window starts folded, so a long list reads as a list
    /// before it reads as a form.
    open: HashSet<u64>,
    _player_changed: Subscription,
}

impl ControlsPanel {
    pub fn new(state: AppState, mut config: ControlsConfig, cx: &mut Context<Self>) -> Self {
        // Before the editor keys anything on them: a zero or a duplicate
        // id here hands two buttons one tooltip field for the rest of the
        // session.
        assign_button_ids(&mut config.buttons);

        // And after them, since the well is a list of those ids: a layout
        // whose well names a button that got deleted by hand comes back
        // drawing the rest of the strip rather than a gap.
        config.items = normalize_items(&config.items, &config.buttons);

        let _player_changed = cx.observe(&state.player, |_, _, cx| cx.notify());

        ControlsPanel {
            state,
            config,
            focus: cx.focus_handle(),
            tab_panel: None,
            tips: HashMap::new(),
            open: HashSet::new(),
            _player_changed,
        }
    }

    /// The live case id per placed button, read once for the frame. A
    /// state reader is a plain `fn(&Player)`, so this is a slice scan and
    /// a call per button.
    fn live_cases(&self, placed: &[Placed<'_>], cx: &App) -> Vec<Option<&'static str>> {
        let player = self.state.player.read(cx);
        placed
            .iter()
            .map(|slot| match slot {
                Placed::Button(def) => buttons::read_state(&def.state, player),
                Placed::Furniture(_) => None,
            })
            .collect()
    }

    fn body(&mut self, cx: &mut Context<Self>) -> Div {
        // The well, not the definition list: a button parked in the
        // Layout page's tray is configured and deliberately not drawn.
        let placed = placed(&self.config.items, &self.config.buttons);
        let cases = self.live_cases(&placed, cx);

        let strip = div()
            .size_full()
            .bg(palette::bg_root())
            .flex()
            .items_center()
            .map(|d| justify(d, self.config.align))
            .gap(tokens::SPACE_XS)
            .px(tokens::SPACE_SM);

        // An empty well draws the placeholder once, so a panel just
        // dropped into the dock is never a blank rectangle with no way in.
        if placed.is_empty() {
            let blank = ButtonLook::placeholder();
            return strip.child(button_element(
                "custom-button-empty".into(),
                &blank,
                press(String::new()),
                cx,
            ));
        }

        let mut children = Vec::with_capacity(placed.len());
        for (index, slot) in placed.iter().enumerate() {
            let child = match slot {
                Placed::Button(def) => {
                    let resolved = look(def, cases[index]);
                    let id = SharedString::from(format!("custom-button-{}", def.id));
                    button_element(id, &resolved, press(def.action.clone()), cx).into_any_element()
                }

                Placed::Furniture(Furniture::Spacer) => div().flex_1().into_any_element(),

                // The seek strip's rule: a hairline that takes the slack.
                Placed::Furniture(Furniture::Divider) => div()
                    .flex_1()
                    .h(px(1.))
                    .bg(palette::border())
                    .into_any_element(),
            };
            children.push(child);
        }

        strip.children(children)
    }
}

// The settings page.
impl ControlsPanel {
    /// The Content page: every button the panel has, and what each one
    /// does. Where they sit is the Layout page's business.
    fn buttons_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.sync_tips(window, cx);

        let add = settings_ui::small_button(
            rox_i18n::t!("button-editor-add"),
            icons::PLUS,
            false,
            cx.listener(|this, _, _, cx| this.add_button(cx)),
        );

        let mut list = div().flex().flex_col().gap(tokens::SPACE_MD);
        if self.config.buttons.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(palette::text_muted())
                    .child(rox_i18n::t!("button-editor-empty")),
            );
        }
        for index in 0..self.config.buttons.len() {
            list = list.child(self.button_block(index, cx));
        }

        div().flex().flex_col().gap(SECTION_GAP).child(section(
            rox_i18n::t!("button-editor-section-buttons"),
            Some(add.into_any_element()),
            list,
        ))
    }

    /// The Layout page: where the strip sits, then which of the defined
    /// buttons are on it and in what order. The well is the strip and the
    /// tray is everything defined but not placed, so taking a button off
    /// the strip is a drag rather than a delete; deleting still lives on
    /// the trash icon over on the Content page.
    fn layout_page(&mut self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(tokens::SPACE_MD)
            .child(panel::align_row(
                self.config.align,
                |this: &mut Self, align, cx| {
                    this.config.align = align;
                    cx.notify();
                },
                cx,
            ))
            .child(setting_block(
                rox_i18n::t!("button-editor-section-layout"),
                Some(rox_i18n::t!("button-editor-section-layout.description")),
                None,
                panel::arrange_editor(
                    "controls-items",
                    well_registry(&self.config.buttons),
                    &self.config.items,
                    |this: &mut Self, items, cx| {
                        this.config.items = items;
                        cx.notify();
                    },
                    cx,
                ),
            ))
    }

    /// Keep one tooltip field per case alive, and drop the ones whose case
    /// is gone. Keyed by button id and case rather than by position, so
    /// reordering the strip doesn't hand a field's typing to its
    /// neighbour. Particles' `signal_ui::sync` discipline, for the one
    /// piece of widget state this page owns itself.
    fn sync_tips(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<(u64, String)> = self
            .config
            .buttons
            .iter()
            .flat_map(|def| {
                def.cases
                    .iter()
                    .map(move |case| (def.id, case.when.clone()))
            })
            .collect();

        self.tips.retain(|key, _| wanted.contains(key));

        for key in wanted {
            if self.tips.contains_key(&key) {
                continue;
            }

            let current = self
                .case(key.0, &key.1)
                .map(|case| case.tip.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rox_i18n::t!("button-editor-tip-placeholder"))
                    .default_value(current)
            });
            let (id, when) = key.clone();
            let events = cx.subscribe(
                &input,
                move |this: &mut Self, input, event: &InputEvent, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }

                    let text = input.read(cx).value().to_string();
                    if let Some(case) = this.case_mut(id, &when) {
                        case.tip = text;
                        cx.notify();
                    }
                },
            );
            self.tips.insert(key, (input, events));
        }
    }

    fn case(&self, id: u64, when: &str) -> Option<&ButtonCase> {
        self.button(id)?.cases.iter().find(|case| case.when == when)
    }

    fn case_mut(&mut self, id: u64, when: &str) -> Option<&mut ButtonCase> {
        self.button_mut(id)?
            .cases
            .iter_mut()
            .find(|case| case.when == when)
    }

    fn button(&self, id: u64) -> Option<&CustomButton> {
        self.config.buttons.iter().find(|def| def.id == id)
    }

    fn button_mut(&mut self, id: u64) -> Option<&mut CustomButton> {
        self.config.buttons.iter_mut().find(|def| def.id == id)
    }

    fn add_button(&mut self, cx: &mut Context<Self>) {
        self.config.buttons.push(CustomButton::default());
        assign_button_ids(&mut self.config.buttons);

        // The one just added is the one about to be filled in.
        if let Some(added) = self.config.buttons.last() {
            self.open.insert(added.id);
        }
        cx.notify();
    }

    fn toggle_open(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.open.remove(&id) {
            self.open.insert(id);
        }
        cx.notify();
    }

    fn remove_button(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.config.buttons.len() {
            remove_at(&mut self.config.buttons, &mut self.config.items, index);
            cx.notify();
        }
    }

    /// Move a button one place along the strip. The editor's widget state
    /// keys on the button's id, so nothing follows the swap.
    fn move_button(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        let Some(to) = index.checked_add_signed(delta) else {
            return;
        };

        if to < self.config.buttons.len() {
            self.config.buttons.swap(index, to);
            cx.notify();
        }
    }

    /// One button's block: the header carrying its name and its list
    /// controls, then the command, and once there is one, everything the
    /// look is made of.
    ///
    /// The block carries an element id of its own because everything below
    /// it is built from `&'static str` ids that every other button repeats.
    /// gpui paths ids through their ided ancestors, so this is what keeps
    /// two buttons' state pickers from sharing one popup.
    fn button_block(&self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let def = &self.config.buttons[index];
        let id = def.id;
        let last = self.config.buttons.len() - 1;
        let spec = state_spec(&def.state);

        let name = if def.action.is_empty() {
            rox_i18n::t!("button-editor-unnamed")
        } else {
            command_label(&def.action)
        };

        let open = self.open.contains(&id);
        let header = settings_ui::block_header(
            div()
                .id(SharedString::from(format!("button-fold-{id}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(tokens::SPACE_XS)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.toggle_open(id, cx)),
                )
                .child(
                    svg()
                        .path(if open {
                            icons::CHEVRON_DOWN
                        } else {
                            icons::CHEVRON_RIGHT
                        })
                        .size(px(12.))
                        .text_color(palette::text_muted()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette::text_muted())
                        .child(name),
                ),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(tokens::SPACE_XS)
                .child(settings_ui::icon_button(
                    icons::ARROW_UP,
                    index == 0,
                    cx.listener(move |this, _, _, cx| this.move_button(index, -1, cx)),
                ))
                .child(settings_ui::icon_button(
                    icons::ARROW_DOWN,
                    index == last,
                    cx.listener(move |this, _, _, cx| this.move_button(index, 1, cx)),
                ))
                .child(settings_ui::icon_button(
                    icons::TRASH,
                    false,
                    cx.listener(move |this, _, _, cx| this.remove_button(index, cx)),
                )),
        );

        let mut block = div()
            .id(SharedString::from(format!("button-{id}")))
            .flex()
            .flex_col()
            .gap(tokens::SPACE_SM)
            .child(header);

        // Folded is the header alone, so the list stays a list.
        if !open {
            return block;
        }

        block = block.child(setting_row(
            rox_i18n::t!("button-editor-action"),
            Some(rox_i18n::t!("button-editor-action.description")),
            self.command_field(id, def, cx),
        ));

        // An unconfigured button stays one line. There is nothing to say
        // about a look until there is something for it to look like.
        if def.action.is_empty() {
            return block;
        }

        block = block
            .child(setting_row(
                rox_i18n::t!("button-editor-state"),
                Some(rox_i18n::t!("button-editor-state.description")),
                self.state_field(id, def, cx),
            ))
            .child(self.preview_row(def, spec, cx));

        for case in &def.cases {
            block = block.child(self.case_block(id, case, spec, cx));
        }

        block
    }

    /// The command picker: every global command, grouped the way the
    /// Keymap page groups them, behind a search box because fifty-nine
    /// entries in a plain menu is a scroll hunt.
    fn command_field(&self, id: u64, def: &CustomButton, cx: &mut Context<Self>) -> AnyElement {
        let current: Option<SharedString> = (!def.action.is_empty())
            .then(|| SharedString::from(def.action.clone()))
            .filter(|action| keymap::COMMANDS.iter().any(|command| *action == command.id));
        let label = match &current {
            Some(action) => command_label(action),
            None => rox_i18n::t!("button-editor-action-none"),
        };

        search_picker(
            "button-action",
            command_rows(),
            label,
            current,
            rox_i18n::t!("button-editor-action-search"),
            rox_i18n::t!("button-editor-action-empty"),
            move |this: &mut Self, value, cx| {
                // Every row carries a command id; the shared field's
                // clear-to-default head row is never built for this list.
                let Some(value) = value else {
                    return;
                };

                if let Some(def) = this.button_mut(id) {
                    def.action = value;

                    // A button picked action-first still wants a case
                    // table to draw from.
                    if def.cases.is_empty() {
                        def.cases = seed_cases(None, &[]);
                    }
                }
                cx.notify();
            },
            cx,
        )
        .into_any_element()
    }

    /// The state picker: the eight declared states plus the stateless
    /// option. Nine entries doesn't need a search box.
    fn state_field(&self, id: u64, def: &CustomButton, cx: &mut Context<Self>) -> AnyElement {
        let mut options = vec![(String::new(), rox_i18n::t!("button-editor-state-none"))];
        options.extend(
            buttons::STATES
                .iter()
                .map(|spec| (spec.id.to_string(), rox_i18n::t!(spec.label_key))),
        );

        picker(
            "button-state",
            def.state.clone(),
            options,
            false,
            move |this: &mut Self, state: String, cx| {
                let spec = state_spec(&state);
                if let Some(def) = this.button_mut(id) {
                    def.state = state;
                    def.cases = seed_cases(spec, &def.cases);
                    prefill_action(&mut def.action, spec);
                }
                cx.notify();
            },
            cx,
        )
        .into_any_element()
    }

    /// The button drawn once per case, above the table that defines them.
    /// This is what makes the page a design tool rather than a form: the
    /// whole state machine is on screen without driving the app into each
    /// state in turn. A preview press fires the command like the real one,
    /// since the obvious thing to do to a button is press it.
    fn preview_row(
        &self,
        def: &CustomButton,
        spec: Option<&'static StateSpec>,
        cx: &mut Context<Self>,
    ) -> Div {
        // No wrap: the control slot is measured at one line, so a wrapped
        // second row would paint over the case block below. Three cases at
        // most, and they fit on one line.
        let mut row = div().flex().flex_row().items_start().gap(tokens::SPACE_MD);

        for (index, case) in def.cases.iter().enumerate() {
            let resolved = look(def, Some(&case.when));
            let id = SharedString::from(format!("preview-{}-{index}", def.id));
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.))
                    .child(button_element(id, &resolved, press(def.action.clone()), cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette::text_muted())
                            .child(case_label(spec, &case.when)),
                    ),
            );
        }

        setting_row(rox_i18n::t!("button-editor-preview"), None, row)
    }

    /// One case's rows: the glyph, the colour role, and the tooltip, under
    /// the case's own name. The nested-block shape is `RouteEditor::row`'s,
    /// which is where the tree already puts several controls that belong to
    /// one thing.
    fn case_block(
        &self,
        id: u64,
        case: &ButtonCase,
        spec: Option<&'static StateSpec>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let when = case.when.clone();
        let roles: Vec<(String, SharedString)> = palette::ROLES
            .iter()
            .map(|role| (role.name.to_string(), SharedString::from(role.label)))
            .collect();

        let icon = {
            let when = when.clone();
            icon_picker(
                "case-icon",
                &case.icon,
                move |this: &mut Self, path: SharedString, cx| {
                    if let Some(case) = this.case_mut(id, &when) {
                        case.icon = path.to_string();
                        cx.notify();
                    }
                },
                cx,
            )
        };

        let color = {
            let when = when.clone();
            // An empty role draws as `text`, so the picker says so; left
            // empty it would label the row with its head option instead.
            let current = if case.color.is_empty() {
                "text".to_string()
            } else {
                case.color.clone()
            };
            picker(
                "case-color",
                current,
                roles,
                false,
                move |this: &mut Self, role: String, cx| {
                    if let Some(case) = this.case_mut(id, &when) {
                        case.color = role;
                        cx.notify();
                    }
                },
                cx,
            )
        };

        let mut body = div()
            .flex()
            .flex_col()
            .gap(tokens::SPACE_SM)
            .child(settings_ui::block_header(
                div()
                    .text_xs()
                    .text_color(palette::text_muted())
                    .child(case_label(spec, &when)),
                div(),
            ))
            .child(setting_row(rox_i18n::t!("button-editor-icon"), None, icon))
            .child(setting_row(
                rox_i18n::t!("button-editor-color"),
                None,
                color,
            ));

        // The tooltip field only exists once `sync_tips` has built it,
        // which it has by the time a block renders.
        if let Some((input, _)) = self.tips.get(&(id, when.clone())) {
            body = body.child(setting_row(
                rox_i18n::t!("button-editor-tip"),
                None,
                div().w(px(180.)).child(Input::new(input).small()),
            ));
        }

        div()
            .id(SharedString::from(format!("case-{id}-{when}")))
            .child(settings_ui::nested(body))
    }
}

/// A case's name in the editor: the catalog's label for it, or the
/// stateless row's own name when the button follows nothing.
fn case_label(spec: Option<&'static StateSpec>, when: &str) -> SharedString {
    let Some(spec) = spec else {
        return rox_i18n::t!("button-editor-case-always");
    };

    spec.cases
        .iter()
        .find(|case| case.id == when)
        .map(|case| rox_i18n::t!(case.label_key))
        .unwrap_or_else(|| rox_i18n::t!("button-editor-case-always"))
}

impl PanelSettings for ControlsPanel {
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

    fn pages(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("Layout", icons::ALIGN_LEFT),
            ("Content", icons::LAYOUT_GRID),
        ]
    }

    fn page(
        &mut self,
        page: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            "Layout" => self.layout_page(cx).into_any_element(),

            _ => self.buttons_page(window, cx).into_any_element(),
        }
    }
}

impl EventEmitter<PanelEvent> for ControlsPanel {}

impl Focusable for ControlsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for ControlsPanel {
    fn panel_name(&self) -> &'static str {
        "custom controls"
    }

    rox_panel_api::opens_settings!();

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        panel::title_text(
            self.config.chrome.title.as_deref(),
            rox_i18n::t!("panel-title-custom-controls"),
        )
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        self.config.chrome.title.clone().map(SharedString::from)
    }

    fn locked(&self, _cx: &App) -> bool {
        self.config.chrome.locked
    }

    /// A strip, like the transport panels: the body paints edge to edge
    /// and pads itself, so the dock's inner padding would only show as a
    /// bare band above the buttons.
    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }

    fn min_size(&self, _cx: &App) -> gpui::Size<gpui::Pixels> {
        rox_panel_api::panel::chrome_min_size(
            &self.config.chrome,
            gpui::size(
                rox_dock::resizable::PANEL_MIN_SIZE,
                rox_dock::resizable::PANEL_MIN_SIZE,
            ),
        )
    }

    fn max_size(&self, cx: &App) -> gpui::Size<gpui::Pixels> {
        rox_panel_api::panel::chrome_max_size(&self.config.chrome, self.min_size(cx))
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
        panel::popout_item(
            menu,
            &cx.entity(),
            self.tab_panel.clone(),
            self.state.clone(),
            window,
        )
    }
}

impl Render for ControlsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chrome = self.config.chrome.clone();
        panel::themed(&chrome, || self.body(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well slot naming a button.
    fn b(id: u64) -> WellItem {
        WellItem::Button(id)
    }

    /// A button carrying everything a saved layout has to bring back.
    fn button(id: u64, action: &str, state: &str, cases: &[(&str, &str, &str)]) -> CustomButton {
        CustomButton {
            id,
            action: action.to_string(),
            state: state.to_string(),
            cases: cases
                .iter()
                .map(|(when, icon, color)| ButtonCase {
                    when: when.to_string(),
                    icon: icon.to_string(),
                    color: color.to_string(),
                    tip: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn config_round_trips() {
        let config = ControlsConfig {
            buttons: vec![
                button(
                    3,
                    "toggle_playback",
                    "player.playback",
                    &[
                        ("playing", icons::PAUSE, "text"),
                        ("paused", icons::PLAY, "text"),
                    ],
                ),
                button(7, "cycle_loop", "", &[("", icons::REPEAT, "accent")]),
            ],
            items: vec![b(7), WellItem::Furniture(Furniture::Spacer), b(3)],
            align: Align::Center,
            ..ControlsConfig::default()
        };

        let json = serde_json::to_value(config.clone()).expect("the config serializes");
        let back: ControlsConfig = serde_json::from_value(json).expect("and reads back");

        // Compared by value rather than by `assert_eq!`: the config types
        // carry the derives the contract spells and `Debug` isn't one.
        assert!(
            back.buttons == config.buttons,
            "the buttons came back changed"
        );
        assert_eq!(back.items, config.items, "the well came back changed");
        assert!(back.align == Align::Center);
    }

    /// The well is ids, and nothing stops a hand-edited layout from
    /// naming one twice or naming a button that was deleted.
    #[test]
    fn items_normalize() {
        let buttons = vec![
            button(3, "toggle_playback", "", &[]),
            button(7, "cycle_loop", "", &[]),
        ];

        let spacer = WellItem::Furniture(Furniture::Spacer);
        assert_eq!(
            normalize_items(&[b(9), b(7), spacer, b(7), b(3), spacer, b(0)], &buttons),
            vec![b(7), spacer, b(3), spacer]
        );
        assert_eq!(normalize_items(&[], &buttons), Vec::<WellItem>::new());
        assert_eq!(normalize_items(&[b(3), b(7)], &[]), Vec::<WellItem>::new());
    }

    /// The saved shape is a bare number for a button and a word for the
    /// furniture, so a well written before the furniture existed still
    /// reads.
    #[test]
    fn the_well_reads_numbers_and_words() {
        let old: Vec<WellItem> = serde_json::from_str("[3, 7]").expect("the old shape parses");
        assert_eq!(old, vec![b(3), b(7)]);

        let mixed: Vec<WellItem> =
            serde_json::from_str(r#"[3, "spacer", 7, "divider"]"#).expect("the mixed shape parses");
        assert_eq!(
            mixed,
            vec![
                b(3),
                WellItem::Furniture(Furniture::Spacer),
                b(7),
                WellItem::Furniture(Furniture::Divider),
            ]
        );

        let json = serde_json::to_string(&mixed).expect("and writes back");
        assert_eq!(json, r#"[3,"spacer",7,"divider"]"#);
    }

    /// Deleting a button takes it off the strip with it. Left behind, the
    /// id would hold a place nothing can draw until the next reload.
    #[test]
    fn removing_a_button_drops_it_from_the_well() {
        let mut buttons = vec![
            button(3, "toggle_playback", "", &[]),
            button(7, "cycle_loop", "", &[]),
        ];
        let mut items = vec![b(7), b(3)];

        remove_at(&mut buttons, &mut items, 1);

        assert_eq!(items, vec![b(3)]);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].id, 3);

        // Past the end is a no-op rather than a panic: the editor's rows
        // are built from a list an event could have shortened.
        remove_at(&mut buttons, &mut items, 4);
        assert_eq!(buttons.len(), 1);
    }

    /// The strip reads the well, in the well's order. The Controls page's
    /// own order only decides where a button sits in the editor's list
    /// and in the Layout page's tray.
    #[test]
    fn the_strip_follows_the_well_not_the_list() {
        let buttons = vec![
            button(3, "toggle_playback", "", &[]),
            button(7, "cycle_loop", "", &[]),
            button(9, "play_random", "", &[]),
        ];

        let drawn: Vec<Option<u64>> = placed(
            &[b(7), WellItem::Furniture(Furniture::Divider), b(3)],
            &buttons,
        )
        .iter()
        .map(|slot| match slot {
            Placed::Button(def) => Some(def.id),
            Placed::Furniture(_) => None,
        })
        .collect();
        assert_eq!(
            drawn,
            vec![Some(7), None, Some(3)],
            "the strip sorted itself by definition"
        );
        assert!(
            !drawn.contains(&Some(9)),
            "a button off the well reached the strip"
        );

        // Nothing placed is the placeholder's case, and an id with no
        // button behind it is skipped rather than drawn as a gap.
        assert!(placed(&[], &buttons).is_empty());
        assert!(placed(&[b(42)], &buttons).is_empty());
    }

    /// A collision here hands two buttons one tooltip field and nothing
    /// anywhere says so.
    #[test]
    fn ids_normalize() {
        let mut buttons = vec![
            button(0, "", "", &[]),
            button(4, "", "", &[]),
            button(4, "", "", &[]),
            button(0, "", "", &[]),
            button(1, "", "", &[]),
        ];
        assign_button_ids(&mut buttons);

        let ids: Vec<u64> = buttons.iter().map(|def| def.id).collect();
        assert!(
            ids.iter().all(|id| *id != 0),
            "{ids:?} kept an unassigned id"
        );

        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{ids:?} repeats an id");

        // The distinct ones a loaded config already had stay put.
        assert_eq!(ids[1], 4);
        assert_eq!(ids[4], 1);
    }

    #[test]
    fn an_unknown_icon_falls_back() {
        assert_eq!(icon_for(icons::PLAY), icons::PLAY);
        assert_eq!(icon_for("icons/does-not-exist.svg"), icons::SQUARE_DASHED);
        assert_eq!(icon_for(""), icons::SQUARE_DASHED);
    }

    #[test]
    fn an_unknown_colour_falls_back() {
        assert_eq!(color_for("accent"), palette::accent());
        assert_eq!(color_for("not-a-role"), palette::text());
        assert_eq!(color_for(""), palette::text());
    }

    #[test]
    fn picking_a_state_seeds_every_case() {
        let spec = state_spec("player.repeat").expect("repeat is in the catalog");
        let seeded = seed_cases(Some(spec), &[]);

        assert_eq!(seeded.len(), spec.cases.len());
        for (case, stock) in seeded.iter().zip(spec.cases) {
            assert_eq!(case.when, stock.id);
            assert_eq!(case.icon, stock.icon);
            assert_eq!(case.color, stock.color);
            assert!(case.tip.is_empty(), "the stock look carries no tooltip");
        }
    }

    #[test]
    fn reseeding_keeps_matching_cases() {
        let repeat = state_spec("player.repeat").expect("repeat is in the catalog");
        let mut held = seed_cases(Some(repeat), &[]);
        held[1].icon = icons::INFINITY.to_string();
        held[1].tip = "mine".to_string();
        held.push(ButtonCase {
            when: "gone".to_string(),
            icon: icons::DICE.to_string(),
            color: "accent".to_string(),
            tip: String::new(),
        });

        let again = seed_cases(Some(repeat), &held);

        assert_eq!(again.len(), repeat.cases.len());
        assert_eq!(again[1].icon, icons::INFINITY, "an edited icon survived");
        assert_eq!(again[1].tip, "mine");
        assert!(
            !again.iter().any(|case| case.when == "gone"),
            "a case the state can't be in kept its row"
        );

        // Switching away collapses to the single row every draw uses.
        let stateless = seed_cases(None, &again);
        assert_eq!(stateless.len(), 1);
        assert!(stateless[0].when.is_empty());
    }

    #[test]
    fn picking_a_state_prefills_an_empty_action_only() {
        let repeat = state_spec("player.repeat").expect("repeat is in the catalog");

        let mut empty = String::new();
        prefill_action(&mut empty, Some(repeat));
        assert_eq!(empty, "cycle_loop");

        let mut chosen = "play_random".to_string();
        prefill_action(&mut chosen, Some(repeat));
        assert_eq!(chosen, "play_random", "a chosen action was overwritten");

        // A state whose catalog entry carries no action prefills nothing
        // rather than an empty id; the stateless pick is the one case that
        // is guaranteed to have none.
        let mut stateless = String::new();
        prefill_action(&mut stateless, None);
        assert!(stateless.is_empty());
    }

    /// The catalog's prefills are plain strings `keymap` never sees, so
    /// this is the one place the two tables meet: a state naming a command
    /// that doesn't exist, or one a button can't reach, would seed a
    /// button that does nothing.
    #[test]
    fn every_state_action_is_a_global_command() {
        for spec in buttons::STATES {
            if spec.action.is_empty() {
                continue;
            }

            let Some(command) = keymap::COMMANDS.iter().find(|c| c.id == spec.action) else {
                panic!("{} prefills unknown command {}", spec.id, spec.action);
            };
            assert!(
                command.reach == keymap::Reach::Global,
                "{} prefills the panel-scoped command {}",
                spec.id,
                spec.action
            );
        }
    }
}
