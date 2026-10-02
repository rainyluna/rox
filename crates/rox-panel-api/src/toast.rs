//! Toasts: a short line in a window's corner for something the user did
//! that has no place of its own to answer in, like a copy or a lookup that
//! failed after its menu closed.
//!
//! Each window keeps its own stack, drawn bottom right by [`layer`]: the
//! workspace draws it, and so does the frame every child window is wrapped
//! in. The code raising a toast decides whether it's pinned. A pinned one
//! stays until its X is pressed; the rest count down while the pointer is
//! off them. A toast never replaces a panel's own state or the Tasks
//! window's progress; it reports and goes.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyWindowHandle, App, ClipboardItem, Context,
    ElementId, Entity, Global, Pixels, Render, SharedString, Window, div, ease_out_quint,
    overlay_phase, prelude::*, px,
};
use rox_design::assets::icons;
use rox_design::{palette, tokens};
use rox_panel_kit::ui::{icon_button, small_button};

use crate::panel::Tone;

/// Counted only while the pointer is off the toast.
fn lifetime(tone: Tone) -> Duration {
    match tone {
        Tone::Warn => Duration::from_secs(8),
        _ => Duration::from_secs(5),
    }
}

const TICK: Duration = Duration::from_millis(100);

/// So a burst can't climb the window. The oldest go first, pinned or not.
const MAX_SHOWN: usize = 10;

const WIDTH: Pixels = px(380.);

type Run = Rc<dyn Fn(&mut Window, &mut App)>;

struct Action {
    label: SharedString,
    icon: &'static str,
    run: Run,
}

pub struct Toast {
    tone: Tone,
    title: Option<SharedString>,
    message: SharedString,
    key: Option<SharedString>,
    pinned: bool,
    actions: Vec<Action>,
}

impl Toast {
    /// A failure starts pinned, since its reason is the part worth reading
    /// and a few seconds isn't always enough for it.
    pub fn new(tone: Tone, message: impl Into<SharedString>) -> Self {
        Self {
            tone,
            title: None,
            message: message.into(),
            key: None,
            pinned: tone == Tone::Bad,
            actions: Vec::new(),
        }
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// A toast with the same key replaces the one showing instead of
    /// stacking under it, so pressing Copy twice says it once.
    pub fn key(mut self, key: impl Into<SharedString>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Pinned stays until its X is pressed.
    pub fn pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    /// A button under the message. Pressing one runs it and closes the toast.
    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        icon: &'static str,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.actions.push(Action {
            label: label.into(),
            icon,
            run: Rc::new(run),
        });
        self
    }

    /// Show it in `window`.
    pub fn show(self, window: &mut Window, cx: &mut App) {
        let stack = stack_for(window, cx);
        stack.update(cx, |stack, cx| stack.push(self, cx));

        // A window's first toast makes its stack, which no render has drawn yet.
        window.refresh();
    }

    /// Show it in `origin` when that window is still open, or else wherever
    /// the user is. For an answer that lands after the menu or dialog that
    /// asked for it has gone.
    pub fn post(self, origin: AnyWindowHandle, cx: &mut App) {
        let open = cx.windows();
        let target = [Some(origin), cx.active_window()]
            .into_iter()
            .flatten()
            .chain(open.iter().copied())
            .find(|handle| open.contains(handle));

        let Some(target) = target else {
            log::info!("toast dropped, no window is open: {}", self.message);
            return;
        };

        target
            .update(cx, |_, window, cx| self.show(window, cx))
            .ok();
    }
}

/// Write `text` to the clipboard and say so, for a Copy that otherwise
/// gives no sign it did anything.
pub fn copy(text: impl Into<String>, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.into()));
    copied().show(window, cx);
}

/// The toast [`copy`] shows, for a copy that lands after its window has
/// moved on.
pub fn copied() -> Toast {
    Toast::new(Tone::Good, rox_i18n::t!("toast-copied")).key("copied")
}

/// The window's toasts, stacked up from its bottom right corner, newest
/// lowest. Draw it as the view's last child.
pub fn layer(window: &Window, cx: &App) -> Option<AnyElement> {
    let stack = cx.try_global::<Stacks>()?.0.get(&window_id(window))?;
    if stack.read(cx).items.is_empty() {
        return None;
    }

    Some(
        // The overlay phase keeps a panel's region shader from washing them
        // out, the reason the modals paint there too.
        overlay_phase(
            div()
                .absolute()
                .bottom(tokens::SPACE_MD)
                .right(tokens::SPACE_MD)
                .child(stack.clone()),
        )
        .into_any_element(),
    )
}

#[derive(Default)]
struct Stacks(HashMap<u64, Entity<ToastStack>>);

impl Global for Stacks {}

fn window_id(window: &Window) -> u64 {
    window.window_handle().window_id().as_u64()
}

fn stack_for(window: &Window, cx: &mut App) -> Entity<ToastStack> {
    let id = window_id(window);
    if let Some(stack) = cx.try_global::<Stacks>().and_then(|s| s.0.get(&id)) {
        return stack.clone();
    }

    // A closed window's stack goes when the next one is made.
    let open: HashSet<u64> = cx
        .windows()
        .iter()
        .map(|handle| handle.window_id().as_u64())
        .collect();

    let stack = cx.new(|_| ToastStack::default());
    let stacks = cx.default_global::<Stacks>();
    stacks.0.retain(|id, _| open.contains(id));
    stacks.0.insert(id, stack.clone());

    stack
}

#[derive(Default)]
struct ToastStack {
    items: Vec<Item>,
    next_id: u64,
    ticking: bool,
}

struct Item {
    id: u64,
    toast: Toast,
    left: Duration,
    hovered: bool,
    closing: bool,
}

impl ToastStack {
    fn push(&mut self, toast: Toast, cx: &mut Context<Self>) {
        if let Some(key) = &toast.key {
            self.items
                .retain(|item| item.toast.key.as_ref() != Some(key));
        }

        self.next_id += 1;
        self.items.push(Item {
            id: self.next_id,
            left: lifetime(toast.tone),
            toast,
            hovered: false,
            closing: false,
        });

        let over = self.items.len().saturating_sub(MAX_SHOWN);
        self.items.drain(..over);

        self.start_ticking(cx);
        cx.notify();
    }

    fn start_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;

                let counting = this.update(cx, |this, cx| this.tick(cx)).unwrap_or(false);
                if !counting {
                    break;
                }
            }
        })
        .detach();
    }

    /// One step off every unpinned clock. False once none is left to count,
    /// which ends the loop; a hovered toast still counts, since it resumes.
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut expired = Vec::new();

        for item in self.items.iter_mut() {
            if item.toast.pinned || item.closing || item.hovered {
                continue;
            }

            item.left = item.left.saturating_sub(TICK);
            if item.left.is_zero() {
                expired.push(item.id);
            }
        }

        for id in expired {
            self.dismiss(id, cx);
        }

        self.ticking = self
            .items
            .iter()
            .any(|item| !item.toast.pinned && !item.closing);
        self.ticking
    }

    /// Plays the exit, then drops it.
    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id == id && !item.closing)
        else {
            return;
        };

        item.closing = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs_f32(tokens::EASE_SECS))
                .await;

            this.update(cx, |this, cx| {
                this.items.retain(|item| item.id != id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_hovered(&mut self, id: u64, hovered: bool) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.hovered = hovered;
        }
    }

    fn card(&self, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        let id = item.id;
        let toast = &item.toast;
        let stack = cx.weak_entity();

        // Through the weak handle rather than a listener: an action may raise
        // a toast of its own, which updates this stack, and a listener would
        // still be holding it.
        let actions: Vec<_> = toast
            .actions
            .iter()
            .enumerate()
            .map(|(n, action)| {
                let (run, stack) = (action.run.clone(), stack.clone());

                small_button(
                    action.label.clone(),
                    action.icon,
                    false,
                    move |_, window, cx| {
                        run(window, cx);
                        stack.update(cx, |stack, cx| stack.dismiss(id, cx)).ok();
                    },
                )
                .keyed(ElementId::NamedInteger(
                    format!("toast-action-{n}").into(),
                    id,
                ))
            })
            .collect();

        let close = icon_button(
            icons::CLOSE,
            false,
            cx.listener(move |this, _, _, cx| this.dismiss(id, cx)),
        )
        .keyed(ElementId::NamedInteger("toast-close".into(), id));

        let card = div()
            .id(ElementId::NamedInteger("toast".into(), id))
            .group("toast")
            .occlude()
            .relative()
            .w(WIDTH)
            .p(tokens::SPACE_MD)
            .flex()
            .flex_col()
            .gap(tokens::SPACE_SM)
            .bg(palette::bg_menu())
            .border_1()
            .border_color(palette::border())
            .rounded(tokens::RADIUS)
            .shadow_md()
            .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                this.set_hovered(id, *hovered);
            }))
            .child(body(toast.tone, toast.title.clone(), toast.message.clone()))
            .when(!actions.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(tokens::SPACE_SM)
                        .pl(px(16.) + tokens::SPACE_SM)
                        .children(actions),
                )
            })
            // Always there on a pinned toast, since that's the only way it
            // goes. A passing one shows it on hover.
            .child(
                div()
                    .absolute()
                    .top(tokens::SPACE_SM)
                    .right(tokens::SPACE_SM)
                    .when(!toast.pinned, |x| {
                        x.invisible().group_hover("toast", |x| x.visible())
                    })
                    .child(close),
            );

        let closing = item.closing;
        let phase = if closing { "toast-out" } else { "toast-in" };

        card.with_animation(
            ElementId::NamedInteger(phase.into(), id),
            Animation::new(Duration::from_secs_f32(tokens::EASE_SECS))
                .with_easing(ease_out_quint()),
            move |card, delta| {
                if closing {
                    card.left(px(48.) * delta).opacity(1. - delta)
                } else {
                    card.top(px(24.) * (1. - delta)).opacity(delta)
                }
            },
        )
        .into_any_element()
    }
}

impl Render for ToastStack {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<_> = self.items.iter().map(|item| self.card(item, cx)).collect();

        div()
            .flex()
            .flex_col()
            .items_end()
            .gap(tokens::SPACE_SM)
            .children(cards)
    }
}

/// The banner's arrangement: the icon on the headline's row, the reason
/// under it lined up with the headline's text.
fn body(tone: Tone, title: Option<SharedString>, message: SharedString) -> AnyElement {
    let (headline, reason) = match title {
        Some(title) => (title, Some(message)),
        None => (message, None),
    };

    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens::SPACE_SM)
        .child(
            gpui::svg()
                .path(tone.icon())
                .size_4()
                .flex_none()
                .text_color(tone.color()),
        )
        // Sized off the row: a bare zero minimum reads as min-content here
        // and the headline goes one glyph per line.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(palette::text_bright())
                .child(headline),
        );

    div()
        .flex()
        .flex_col()
        .gap(tokens::SPACE_XS)
        // Clears the close button in the corner.
        .pr(px(24.))
        .text_sm()
        .child(head)
        .children(reason.map(|reason| {
            div()
                .pl(px(16.) + tokens::SPACE_SM)
                .text_color(palette::text_muted())
                .child(reason)
        }))
        .into_any_element()
}
