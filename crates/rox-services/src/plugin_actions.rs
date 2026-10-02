//! Plugin actions (ADR 30's actions amendment): calling `source.action`, and
//! polling the jobs it starts until they end.
//!
//! A job is the plugin's work; rox only asks how far it got. Every call is
//! one rox makes, so a plugin still never sends anything unasked.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, Task};
use rox_plugins::wire::{self, ActionAnswer, JobState};
use serde_json::{Value, json};

use crate::plugins::{await_first_apply, host_for, running};

pub use rox_plugins::manifest::ActionDecl;
pub use rox_plugins::wire::Outcome;

const POLL: Duration = Duration::from_secs(1);

/// How long a plugin has after Stop to report the job ended, before rox
/// stops asking.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The actions a running plugin declares. Empty when it isn't running.
pub fn actions(source: &str) -> Vec<ActionDecl> {
    running(source)
        .and_then(|running| {
            let manifest = running.host.manifest();
            manifest
                .capabilities
                .source
                .as_ref()
                .map(|cap| cap.actions.clone())
        })
        .unwrap_or_default()
}

pub enum Started {
    Done(Outcome),
    Job(Arc<Job>),
}

/// Asks the plugin to run `action` on `items`, its tracks' keys or nodes'
/// ids, empty for an action on no item.
pub fn run(
    source: &str,
    action: &ActionDecl,
    items: Vec<String>,
    params: Value,
    cx: &App,
) -> Task<Result<Started, String>> {
    let source = source.to_string();
    let action = action.clone();

    cx.background_executor().spawn(async move {
        await_first_apply(&source);
        let host = host_for(&source)?;

        let params = json!({ "action": action.id, "items": items, "params": params });
        let answer = host.call("source.action", params, host.timeouts().listing)?;
        if answer.is_null() {
            return Ok(Started::Done(Outcome::default()));
        }

        let answer: ActionAnswer = wire::decode(answer)?;
        let Some(id) = answer.job.clone() else {
            return Ok(Started::Done(answer.outcome()));
        };

        let plugin = running(&source)
            .map(|running| running.label.clone())
            .unwrap_or_else(|| source.clone());

        let job = Arc::new(Job {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            source,
            plugin,
            label: action.label,
            id,
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            text: Mutex::new(answer.message),
            stop: AtomicBool::new(false),
        });
        JOBS.lock().unwrap().push(job.clone());

        Ok(Started::Job(job))
    })
}

/// A job a plugin is running, for the Tasks window's row.
pub struct Job {
    /// Unique for the session, for keying the job's row.
    pub serial: u64,
    pub source: String,
    /// The plugin's source label.
    pub plugin: String,
    /// The action's label.
    pub label: String,
    id: String,
    done: AtomicU64,
    total: AtomicU64,
    text: Mutex<String>,
    stop: AtomicBool,
}

impl Job {
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Zero when the plugin can't tell.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn text(&self) -> String {
        self.text.lock().unwrap().clone()
    }

    /// Asks the plugin to stop it, on the next poll.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

static JOBS: LazyLock<Mutex<Vec<Arc<Job>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

static SERIAL: AtomicU64 = AtomicU64::new(1);

/// Every job still running, oldest first.
pub fn jobs() -> Vec<Arc<Job>> {
    JOBS.lock().unwrap().clone()
}

pub fn job(serial: u64) -> Option<Arc<Job>> {
    jobs().into_iter().find(|job| job.serial == serial)
}

pub enum JobEnd {
    Finished(Outcome),
    Failed(String),
    Stopped,
}

/// Polls `job` until the plugin reports it ended, the plugin goes away, or a
/// Stop runs out of grace.
pub fn watch(job: Arc<Job>, cx: &App) -> Task<JobEnd> {
    let executor = cx.background_executor().clone();

    cx.background_executor().spawn(async move {
        let mut stop_sent: Option<Instant> = None;

        let end = loop {
            executor.timer(POLL).await;

            if job.stopping() && stop_sent.is_none() {
                stop_sent = Some(Instant::now());

                // The plugin reports the end through the next polls; a failed
                // cancel only means it ends the long way.
                if let Err(e) = call(&job, "source.cancel") {
                    log::warn!("{}: cancel job {}: {e}", job.source, job.id);
                }
            }

            let state = match call(&job, "source.job").and_then(wire::decode::<JobState>) {
                Ok(state) => state,
                Err(_) if stop_sent.is_some() => break JobEnd::Stopped,
                Err(e) => break JobEnd::Failed(e),
            };

            job.done.store(state.done, Ordering::Relaxed);
            job.total.store(state.total, Ordering::Relaxed);
            *job.text.lock().unwrap() = state.text.clone();

            match (state.error.clone(), state.finished) {
                (Some(_), _) if stop_sent.is_some() => break JobEnd::Stopped,
                (Some(error), _) => break JobEnd::Failed(error),
                (None, true) => break JobEnd::Finished(state.outcome()),
                (None, false) => {}
            }

            if stop_sent.is_some_and(|sent| sent.elapsed() > STOP_GRACE) {
                break JobEnd::Stopped;
            }
        };

        JOBS.lock()
            .unwrap()
            .retain(|running| !Arc::ptr_eq(running, &job));

        end
    })
}

fn call(job: &Job, method: &'static str) -> Result<Value, String> {
    let host = host_for(&job.source)?;
    host.call(method, json!({ "job": job.id }), host.timeouts().listing)
}
