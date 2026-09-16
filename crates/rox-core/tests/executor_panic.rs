//! A panicking background task must not take the worker thread with it.
//!
//! Our vendored gpui spawns tasks with `async_task::Builder::propagate_panic(true)`
//! (patches/gpui/z5-executor-propagate-panic.patch), so the future is polled
//! inside a `catch_unwind`. Without that flag the panic unwinds out of
//! `Runnable::run`, the worker thread dies for good, and whoever awaited the
//! task gets a second, useless panic reading "Task polled after completion".
//!
//! The dispatcher here is a stripped-down copy of the linux one
//! (`vendor/gpui/src/platform/linux/dispatcher.rs`): one thread looping
//! `for runnable in receiver { runnable.run() }`, never restarted.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use async_task::Runnable;
use gpui::{BackgroundExecutor, PlatformDispatcher, TaskLabel};

struct OneWorkerDispatcher {
    tx: Sender<Runnable>,
    /// Bumped after `run()` returns, so it stops climbing the moment a panic
    /// unwinds through the worker loop.
    finished: Arc<AtomicUsize>,
}

impl OneWorkerDispatcher {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Runnable>();
        let finished = Arc::new(AtomicUsize::new(0));
        let worker_finished = finished.clone();
        std::thread::Builder::new()
            .name("rox-test-worker".into())
            .spawn(move || {
                for runnable in rx {
                    runnable.run();
                    worker_finished.fetch_add(1, Ordering::SeqCst);
                }
            })
            .expect("spawn test worker");
        Self { tx, finished }
    }
}

impl PlatformDispatcher for OneWorkerDispatcher {
    fn is_main_thread(&self) -> bool {
        false
    }

    fn dispatch(&self, runnable: Runnable, _label: Option<TaskLabel>) {
        self.tx.send(runnable).expect("worker thread is gone");
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        self.tx.send(runnable).expect("worker thread is gone");
    }

    fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            let _ = tx.send(runnable);
        });
    }
}

/// Wait for the worker to have returned from `run()` `want` times. The
/// tasks signal from inside `run()`, so the main thread can wake before the
/// worker's own bump lands; sampling the counter right after a receive is
/// a race, and this is the wait that isn't.
fn wait_for_finished(finished: &AtomicUsize, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while finished.load(Ordering::SeqCst) < want {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never returned from run() {want} times"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[test]
fn panicking_task_keeps_the_worker_alive_and_reaches_its_awaiter() {
    let dispatcher = OneWorkerDispatcher::new();
    let finished = dispatcher.finished.clone();
    let executor = BackgroundExecutor::new(Arc::new(dispatcher));

    let task = executor.spawn(async { panic!("rox executor test: boom") });
    let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
        futures::executor::block_on(task);
    }))
    .expect_err("awaiting a panicked task should panic");
    let message = payload_message(payload.as_ref());

    // The awaiter sees the original panic, not async-task's follow-on
    // "Task polled after completion" from a task closed without an output.
    assert!(
        message.contains("rox executor test: boom"),
        "awaiter saw {message:?}"
    );
    assert!(
        !message.contains("Task polled after completion"),
        "awaiter saw the follow-on panic instead of the original: {message:?}"
    );

    // And the worker thread is still there to serve the next task.
    let (done_tx, done_rx) = mpsc::channel();
    executor
        .spawn(async move {
            done_tx.send(()).ok();
        })
        .detach();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker thread died with the panicking task");

    // Both runnables came back out of run() rather than unwinding through it.
    wait_for_finished(&finished, 2);
    assert_eq!(finished.load(Ordering::SeqCst), 2);
}

#[test]
fn detached_panicking_task_is_swallowed_but_the_worker_survives() {
    // Nobody awaits a detached task, so async-task drops the stored payload
    // (`Task::detach` in async-task 4.7.1 binds it to `_out`). The panic hook
    // still prints the panic, but nothing resumes it: the crash becomes a log
    // line. This test pins that behaviour so a change to it is deliberate.
    let dispatcher = OneWorkerDispatcher::new();
    let finished = dispatcher.finished.clone();
    let executor = BackgroundExecutor::new(Arc::new(dispatcher));

    executor
        .spawn(async { panic!("rox executor test: detached boom") })
        .detach();

    let (done_tx, done_rx) = mpsc::channel();
    executor
        .spawn(async move {
            done_tx.send(()).ok();
        })
        .detach();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker thread died with the detached panicking task");

    wait_for_finished(&finished, 2);
    assert_eq!(finished.load(Ordering::SeqCst), 2);
}
