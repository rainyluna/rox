//! The Internet Archive example (`examples/plugins/internet-archive`) against
//! the real host. Only what answers without the network runs here: the
//! manifest, the icon, `hello`, the roots and the links. Skipped with a note
//! when no interpreter resolves, like the tones tests.

use std::path::{Path, PathBuf};

use rox_plugins::{Host, HostConfig, hash, loader, manifest};
use serde_json::{Value, json};

const ID: &str = "internet-archive";

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

fn host(tag: &str) -> Option<Host> {
    let dir = examples().join(ID);
    let manifest = manifest::load(&dir).expect("the example's manifest loads");

    if let Err(e) = manifest::entry_for(&manifest, &dir) {
        eprintln!("skipping the internet archive example tests: {e}");
        return None;
    }

    let data_dir =
        std::env::temp_dir().join(format!("rox-plugins-archive-{tag}-{}", std::process::id()));

    Some(Host::new(HostConfig::new(
        dir,
        manifest,
        json!({}),
        data_dir,
    )))
}

fn call(host: &Host, method: &'static str, params: Value) -> Value {
    host.call(method, params, host.timeouts().listing)
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}

#[test]
fn the_folder_loads_with_its_icon_radio_and_links() {
    let found = loader::scan(&examples());
    let archive = found
        .iter()
        .find(|loaded| loaded.id == ID)
        .expect("the scan finds it");

    assert!(
        archive.runs()
            || archive
                .error
                .as_deref()
                .is_some_and(|e| e.contains("interpreter"))
    );

    let source = archive
        .manifest
        .as_ref()
        .and_then(|m| m.capabilities.source.clone())
        .expect("it declares a source");
    assert!(source.radio && source.links);
    assert_eq!(source.icon, "icon.svg");

    let again = hash::folder_hash(&archive.dir).unwrap();
    assert_eq!(archive.hash, again, "the hash is stable");
}

#[test]
fn the_roots_mark_the_home_and_carry_a_notice() {
    let Some(host) = host("roots") else {
        return;
    };
    let before = hash::folder_hash(&examples().join(ID)).unwrap();

    host.ensure().expect("hello answers");

    let roots = call(
        &host,
        "source.browse",
        json!({"node": null, "cursor": null}),
    );
    let homes: Vec<&str> = roots["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["node"]["home"] == json!(true))
        .map(|entry| entry["node"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(homes, ["home"]);
    assert!(roots["notice"]["link"]["url"].is_string());

    // Running it wrote nothing into its own folder.
    let after = hash::folder_hash(&examples().join(ID)).unwrap();
    assert_eq!(before, after);
}

#[test]
fn links_point_at_the_archive_and_places_have_none() {
    let Some(host) = host("links") else {
        return;
    };
    host.ensure().expect("hello answers");

    let link = |item: &str| call(&host, "source.link", json!({"item": item}));

    assert_eq!(
        link("item:NS050")["url"],
        "https://archive.org/details/NS050"
    );
    assert_eq!(
        link("NS050/01 Track.mp3")["url"],
        "https://archive.org/details/NS050/01%20Track.mp3"
    );
    assert!(link("genre:ambient").is_null());
    assert!(link("home").is_null());
}
