//! Three bars over the playing row's cover, moving with the low, mid and high
//! bands of what's audible. A list marks its playing row with them; they
//! aren't a visualizer and have no settings.
//!
//! The bars step in their canvas's paint and ask for the next frame only
//! while audio flows or they're still falling, so a paused list parks. A
//! paused track keeps short stubs, so the row still reads as the playing one.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{AnyElement, Bounds, Pixels, canvas, div, fill, point, prelude::*, px, size};
use rox_viz::AudioFeed;

use crate::design::{palette, tokens};

/// Low, mid and high, in Hz. Their edges only have to look right.
const BANDS: [(f32, f32); 3] = [(40., 250.), (250., 2_000.), (2_000., 8_000.)];

/// Enough resolution to split 250 Hz from 40 at 48 kHz, and a size the
/// spectrum panel asks for too, so the feed often has it already.
const FFT: usize = 1024;

/// The spectrum panel's range and speeds, so the two agree on a track.
const FLOOR_DB: f32 = -66.0;
const MAX_DB: f32 = -12.0;
const ATTACK: f32 = 40.0;
const RELEASE: f32 = 10.0;
const EPSILON: f32 = 0.002;
const SILENT_AFTER: f32 = 0.15;

/// A bar at rest, as a share of the height.
const STUB: f32 = 0.18;

/// Paints closer together than this are one frame: a list can draw the
/// playing track twice.
const STEP_MIN: f32 = 0.004;

#[derive(Default)]
pub(crate) struct PlayingBars {
    levels: [f32; 3],
    targets: [f32; 3],
    last_tick: Option<Instant>,
    last_written: u64,
    last_fresh: Option<Instant>,
    /// Audio still flowing, or a bar still falling.
    moving: bool,
}

impl PlayingBars {
    pub(crate) fn shared() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::default()))
    }

    fn step(&mut self, feed: &AudioFeed) {
        let now = Instant::now();
        let dt = match self.last_tick {
            Some(t) if (now - t).as_secs_f32() < STEP_MIN => return,
            Some(t) => (now - t).as_secs_f32().min(0.1),
            None => 1.0 / 60.0,
        };
        self.last_tick = Some(now);

        let written = feed.written();
        let fresh = written != self.last_written;
        self.last_written = written;

        if fresh {
            self.last_fresh = Some(now);
        }
        let stopped = self
            .last_fresh
            .is_none_or(|t| (now - t).as_secs_f32() > SILENT_AFTER);

        let mags = fresh.then(|| feed.magnitudes(FFT)).flatten();
        let rate = feed.sample_rate().max(1) as f32;

        match &mags {
            Some(mags) => {
                for (target, &(lo, hi)) in self.targets.iter_mut().zip(BANDS.iter()) {
                    let bin = |hz: f32| ((hz * FFT as f32 / rate) as usize).min(mags.len());
                    let (lo, hi) = (bin(lo), bin(hi).max(bin(lo) + 1).min(mags.len()));
                    let peak = mags[lo..hi].iter().fold(0.0f32, |peak, &m| peak.max(m));
                    let db = 20.0 * (peak + 1e-9).log10();
                    *target = ((db - FLOOR_DB) / (MAX_DB - FLOOR_DB)).clamp(0.0, 1.0);
                }
            }

            None if stopped => self.targets = [0.0; 3],
            None => {}
        }

        self.moving = !stopped;
        for (level, &target) in self.levels.iter_mut().zip(self.targets.iter()) {
            let speed = match target > *level {
                true => ATTACK,
                false => RELEASE,
            };
            *level += (target - *level) * (speed * dt).min(1.0);
            self.moving |= *level > EPSILON;
        }
    }

    fn paint(&self, bounds: Bounds<Pixels>, window: &mut gpui::Window) {
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let bar = (w * 0.14).max(2.0);
        let gap = bar * 0.6;
        let run = bar * 3.0 + gap * 2.0;
        let (left, base) = ((w - run) / 2.0, h * 0.78);
        let reach = h * 0.56;

        for (ix, &level) in self.levels.iter().enumerate() {
            let tall = reach * (STUB + (1.0 - STUB) * level);
            let x = left + ix as f32 * (bar + gap);
            let quad = fill(
                Bounds::new(
                    point(bounds.origin.x + px(x), bounds.origin.y + px(base - tall)),
                    size(px(bar), px(tall)),
                ),
                palette::accent(),
            );
            window.paint_quad(quad.corner_radii(px(bar / 2.0)));
        }
    }
}

/// The bars over a cover of `side`, on a scrim that dims the art under them.
pub(crate) fn overlay(
    bars: Rc<RefCell<PlayingBars>>,
    feed: Arc<AudioFeed>,
    side: Pixels,
) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size(side)
        .rounded(tokens::RADIUS)
        .bg(palette::alpha(palette::bg_root_opaque(), 0xa0))
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let mut bars = bars.borrow_mut();
                    bars.step(&feed);
                    bars.paint(bounds, window);

                    // Every frame re-renders the whole list, so a pause parks
                    // it once the bars are down.
                    if bars.moving {
                        window.request_animation_frame();
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}
