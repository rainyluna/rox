//! The host against a real subprocess: the echo fixture, a Python plugin.
//! Skipped with a note when no interpreter resolves, so the gate still runs
//! on a machine without Python.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rox_plugins::host::STOPPED_AFTER_CRASHES;
use rox_plugins::{Host, HostConfig, Options, Status, Stream, manifest, process};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo")
}

/// None, with a line saying why, when this machine can't run the fixture.
fn host(tag: &str, config: Value) -> Option<Host> {
    let dir = fixture();
    let manifest = manifest::load(&dir).expect("the fixture's manifest loads");

    if let Err(e) = manifest::entry_for(&manifest, &dir) {
        eprintln!("skipping the echo fixture tests: {e}");
        return None;
    }

    let data_dir =
        std::env::temp_dir().join(format!("rox-plugins-echo-{tag}-{}", std::process::id()));
    let mut config = HostConfig::new(dir, manifest, config, data_dir);
    config.backoff = [Duration::ZERO; 4];

    Some(Host::new(config))
}

fn listing(host: &Host) -> Duration {
    host.timeouts().listing
}

fn pattern(offset: u64, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| ((offset + i) % 251) as u8)
        .collect()
}

#[test]
fn it_starts_and_says_hello() {
    let Some(host) = host("hello", json!({})) else {
        return;
    };

    assert_eq!(host.status(), Status::Idle, "nothing runs before first use");
    host.ensure().expect("hello answers");
    assert_eq!(host.status(), Status::Running);

    let roots = host
        .call(
            "source.browse",
            json!({"node": null, "cursor": null}),
            listing(&host),
        )
        .unwrap();
    assert_eq!(roots["entries"][0]["node"]["id"], "all");

    host.stop("test over");
}

#[test]
fn a_slow_answer_times_out_and_the_plugin_keeps_serving() {
    let Some(host) = host("timeout", json!({"sleep": {"source.search": 2}})) else {
        return;
    };

    let slow = host.call(
        "source.search",
        json!({"query": "Tone", "cursor": null}),
        Duration::from_millis(300),
    );
    let err = slow.unwrap_err();
    assert!(err.contains("no answer within"), "{err}");

    let browse = host.call(
        "source.browse",
        json!({"node": "all", "cursor": null}),
        listing(&host),
    );
    assert!(
        browse.is_ok(),
        "a timeout doesn't cost the plugin its other calls"
    );

    host.stop("test over");
}

#[test]
fn answers_pair_by_id_in_whatever_order_they_come() {
    let Some(host) = host("order", json!({"sleep": {"source.search": 1}})) else {
        return;
    };
    host.ensure().unwrap();

    let slow = host
        .send(
            "source.search",
            json!({"query": "Tone 2", "cursor": null}),
            listing(&host),
        )
        .unwrap();
    let fast = host
        .send(
            "source.browse",
            json!({"node": "all", "cursor": null}),
            listing(&host),
        )
        .unwrap();

    let began = Instant::now();
    let browsed = fast.wait().unwrap();
    assert!(
        began.elapsed() < Duration::from_millis(900),
        "the fast one didn't queue behind the slow one"
    );
    assert_eq!(browsed["entries"].as_array().unwrap().len(), 3);

    let searched = slow.wait().unwrap();
    assert_eq!(searched["entries"][0]["track"]["key"], "t2");

    host.stop("test over");
}

#[test]
fn a_crash_fails_the_call_and_the_next_call_restarts_it() {
    let Some(host) = host("crash", json!({"crash_on": "source.cover"})) else {
        return;
    };
    host.ensure().unwrap();
    let first = host.pid().unwrap();

    let err = host
        .call("source.cover", json!({"key": "t1"}), listing(&host))
        .unwrap_err();
    assert_eq!(err, "the plugin exited");

    // The reader files the exit a moment after the pipe closes.
    let deadline = Instant::now() + Duration::from_secs(2);
    while host.status() == Status::Running && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host.status(), Status::Idle);

    let browse = host.call(
        "source.browse",
        json!({"node": null, "cursor": null}),
        listing(&host),
    );
    assert!(browse.is_ok(), "{browse:?}");
    assert_ne!(host.pid().unwrap(), first, "a fresh process");

    host.stop("test over");
}

#[test]
fn five_crashes_in_a_row_stop_the_plugin() {
    let Some(host) = host("stopped", json!({"hello_crash": true})) else {
        return;
    };

    for _ in 0..5 {
        assert!(host.ensure().is_err());
    }

    assert_eq!(host.status(), Status::Stopped(STOPPED_AFTER_CRASHES.into()));
    assert_eq!(host.ensure().unwrap_err(), STOPPED_AFTER_CRASHES);

    host.revive();
    assert_eq!(
        host.status(),
        Status::Idle,
        "switching it back on clears the count"
    );
}

#[test]
fn a_plugin_exits_when_its_stdin_closes() {
    let dir = fixture();
    let manifest = manifest::load(&dir).unwrap();
    let Ok(command) = manifest::entry_for(&manifest, &dir) else {
        eprintln!("skipping: no interpreter for the echo fixture");
        return;
    };

    let (mut process, pipes) = process::spawn("echo", command, &dir).unwrap();
    assert!(process.exited().is_none());

    drop(pipes.stdin);
    assert!(process.wait_for(Duration::from_secs(5)), "it exited on EOF");
}

#[cfg(unix)]
#[test]
fn stop_leaves_nothing_running() {
    let Some(host) = host("stop", json!({})) else {
        return;
    };
    host.ensure().unwrap();
    let pid = host.pid().unwrap() as i32;

    host.stop("switched off");
    assert_eq!(host.status(), Status::Stopped("switched off".into()));

    // SAFETY: signal 0 only asks whether the pid exists.
    let alive = unsafe { libc_kill(pid, 0) } == 0;
    assert!(!alive, "the process is gone");
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn a_stream_reads_whole_and_ends() {
    let Some(host) = host("stream", json!({"size": 300_000})) else {
        return;
    };

    let stream = Stream::open(&host, "t1", Options::default()).unwrap();
    assert_eq!(stream.length, Some(300_000));
    assert!(stream.seekable);

    let mut out = Vec::new();
    loop {
        let bytes = stream.read_at(out.len() as u64, 64 * 1024).unwrap();
        if bytes.is_empty() {
            break;
        }
        out.extend_from_slice(&bytes);
    }

    assert_eq!(out, pattern(0, 300_000));

    let mid = stream.read_at(123_456, 10).unwrap();
    assert_eq!(mid, pattern(123_456, 10), "a seek reads at the new offset");

    drop(stream);
    host.stop("test over");
}

#[test]
fn the_next_chunk_is_already_on_its_way() {
    let Some(host) = host("ahead", json!({"size": 1 << 20})) else {
        return;
    };

    let options = Options {
        chunk: 64 * 1024,
        depth: 1,
    };
    let stream = Stream::open(&host, "t2", options).unwrap();

    // The first read is cold and asks for the second chunk behind it.
    let first = stream.read_at(0, 64 * 1024).unwrap();
    assert_eq!(first, pattern(0, 64 * 1024));
    assert_eq!(stream.stats().cold, 1);

    std::thread::sleep(Duration::from_millis(200));
    let second = stream.read_at(64 * 1024, 1000).unwrap();
    assert_eq!(second, pattern(64 * 1024, 1000));

    let stats = stream.stats();
    assert_eq!(stats.cold, 1, "the read-ahead covered it");
    assert_eq!(stats.memory + stats.waited, 1);

    // The rest of that chunk is memory.
    stream.read_at(64 * 1024 + 1000, 1000).unwrap();
    assert_eq!(stream.stats().memory, stats.memory + 1);

    drop(stream);
    host.stop("test over");
}

#[test]
fn an_unseekable_stream_reads_in_order_one_at_a_time() {
    let Some(host) = host("unseekable", json!({"size": 200_000, "unseekable": true})) else {
        return;
    };

    let options = Options {
        chunk: 16 * 1024,
        depth: 2,
    };
    let stream = Stream::open(&host, "t3", options).unwrap();
    assert!(!stream.seekable);

    let mut out = Vec::new();
    loop {
        let bytes = stream.read_at(out.len() as u64, 5000).unwrap();
        if bytes.is_empty() {
            break;
        }
        out.extend_from_slice(&bytes);
    }

    assert_eq!(out, pattern(0, 200_000));
    assert_eq!(stream.stats().cold, 1, "only the first read went out cold");

    drop(stream);
    host.stop("test over");
}

#[test]
fn a_stream_from_a_dead_process_fails_without_restarting_it() {
    let Some(host) = host("dead-stream", json!({"crash_on": "source.cover"})) else {
        return;
    };

    let stream = Stream::open(&host, "t1", Options::default()).unwrap();
    assert!(
        host.call("source.cover", json!({"key": "t1"}), listing(&host))
            .is_err()
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while host.status() == Status::Running && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let err = stream.read_at(0, 10).unwrap_err();
    assert_eq!(err, "the plugin exited");
    assert_eq!(
        host.status(),
        Status::Idle,
        "the read didn't start a new one"
    );
}
