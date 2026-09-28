//! `plugin.json`, manifest v0 (ADR 30). An unknown top-level key refuses
//! the plugin, and so does one inside `entry`, since that's how the plugin
//! runs: a manifest a newer host would read differently never half-loads
//! here. Inside `meta` and `capabilities` unknown keys are ignored, so a
//! field added there later stays additive for plugins built against it.
//!
//! The entry resolves to a [`Command`] and nothing else. The paths it names
//! stay inside the plugin folder; an interpreter comes off PATH through a
//! fixed alias table.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// The `api` values this host speaks.
pub const SUPPORTED_API: RangeInclusive<u32> = 0..=0;

pub const FILE: &str = "plugin.json";

/// A manifest is a few hundred bytes; this only stops a huge file being read.
const MAX_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api: u32,
    pub entry: Entry,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Shown to the user as found or missing on PATH; rox enforces nothing.
    #[serde(default)]
    pub programs: Vec<String>,
    #[serde(default)]
    pub config_schema: serde_json::Value,
}

/// Exactly one kind: serde refuses a map naming both or neither.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Entry {
    Script(Script),
    /// `<os>-<arch>` to a path in the folder.
    Native(BTreeMap<String, String>),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub path: String,
    pub interpreter: String,
}

/// `WorkspaceMeta`'s card minus its dates.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub author: String,
    pub description: String,
    pub website: String,
    pub version: String,
    pub license: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Capabilities {
    pub source: Option<SourceCap>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SourceCap {
    pub label: String,
    /// Off unless the plugin asks: plugin rows don't scrobble by default.
    #[serde(default)]
    pub scrobble: bool,
}

/// `^[a-z0-9][a-z0-9-]{1,63}$`, spelled out rather than pulling in a regex
/// engine for one pattern.
pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();

    (2..=64).contains(&bytes.len())
        && plain(&bytes[0])
        && bytes[1..].iter().all(|b| plain(b) || *b == b'-')
}

/// Reads and checks `<dir>/plugin.json`. The error is the reason the Plugins
/// page shows.
pub fn load(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join(FILE);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{FILE}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{FILE} is not a plain file"));
    }
    if meta.len() > MAX_BYTES {
        return Err(format!("{FILE} is larger than {MAX_BYTES} bytes"));
    }

    let text = std::fs::read_to_string(&path).map_err(|e| format!("{FILE}: {e}"))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let manifest: Manifest = serde_json::from_str(text).map_err(|e| format!("{FILE}: {e}"))?;

    if !valid_id(&manifest.id) {
        return Err(format!(
            "{FILE}: id {:?} is not a valid plugin id",
            manifest.id
        ));
    }
    if !SUPPORTED_API.contains(&manifest.api) {
        return Err(format!(
            "{FILE}: api {} is outside what this rox supports ({}..={})",
            manifest.api,
            SUPPORTED_API.start(),
            SUPPORTED_API.end()
        ));
    }

    Ok(manifest)
}

/// The key a native entry is looked up under, `<os>-<arch>`. Rust's own
/// names for the three are the contract's.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The command that starts this plugin: its own binary, or the interpreter
/// with the script as its first argument. Nothing about cwd, pipes or
/// environment; the process module sets those.
pub fn entry_for(manifest: &Manifest, dir: &Path) -> Result<Command, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    entry_with(manifest, dir, &platform(), &path)
}

fn entry_with(
    manifest: &Manifest,
    dir: &Path,
    platform: &str,
    path_var: &OsStr,
) -> Result<Command, String> {
    match &manifest.entry {
        Entry::Script(script) => {
            let file = inside(dir, &script.path)?;
            let (program, args) = interpreter(&script.interpreter, path_var)
                .ok_or_else(|| format!("interpreter {} not found", script.interpreter))?;

            let mut command = Command::new(program);
            command.args(args).arg(file);

            Ok(command)
        }

        Entry::Native(builds) => {
            let rel = builds
                .get(platform)
                .ok_or_else(|| format!("no build for {platform}"))?;

            Ok(Command::new(inside(dir, rel)?))
        }
    }
}

/// A plugin's own path, refused if it could name anything outside its folder.
fn inside(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    let plain = rel_path
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if rel.is_empty() || !plain {
        return Err(format!("entry path {rel:?} leaves the plugin folder"));
    }

    let full = dir.join(rel_path);
    if !full.is_file() {
        return Err(format!("entry {rel} is missing"));
    }

    Ok(full)
}

/// What to try, in order, for an interpreter name. Any other name is looked
/// up as written.
fn aliases(name: &str) -> Vec<(&str, &'static [&'static str])> {
    match name {
        "python3" => {
            let mut tries: Vec<(&str, &'static [&'static str])> =
                vec![("python3", &[]), ("python", &[])];
            if cfg!(windows) {
                tries.push(("py", &["-3"]));
            }

            tries
        }

        "node" => vec![("node", &[])],

        other => vec![(other, &[])],
    }
}

fn interpreter(name: &str, path_var: &OsStr) -> Option<(PathBuf, Vec<&'static str>)> {
    aliases(name)
        .into_iter()
        .find_map(|(program, args)| on_path(program, path_var).map(|found| (found, args.to_vec())))
}

/// The first `PATH` entry holding `program`, trying Windows' executable
/// extensions after the bare name.
fn on_path(program: &str, path_var: &OsStr) -> Option<PathBuf> {
    let exts: Vec<OsString> = match cfg!(windows) {
        true => std::env::var_os("PATHEXT")
            .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".into())
            .to_string_lossy()
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(OsString::from)
            .collect(),
        false => Vec::new(),
    };

    for dir in std::env::split_paths(path_var) {
        let bare = dir.join(program);
        if bare.is_file() {
            return Some(bare);
        }

        for ext in &exts {
            let mut name = OsString::from(program);
            name.push(ext);

            let with = dir.join(name);
            if with.is_file() {
                return Some(with);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("rox-plugins-manifest-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            Scratch(dir)
        }

        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();

            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn script_manifest(extra: &str) -> String {
        format!(
            r#"{{
                "id": "example-tones",
                "name": "Tones",
                "version": "0.1.0",
                "api": 0,
                "entry": {{ "script": {{ "path": "tones.py", "interpreter": "python3" }} }},
                "meta": {{ "author": "", "description": "", "website": "", "license": "" }},
                "capabilities": {{ "source": {{ "label": "Tones", "scrobble": false }} }},
                "programs": [],
                "config_schema": {{ "type": "object", "properties": {{}} }}
                {extra}
            }}"#
        )
    }

    fn with_entry(entry: &str) -> String {
        format!(r#"{{ "id": "tones", "name": "T", "version": "1", "api": 0, "entry": {entry} }}"#)
    }

    #[test]
    fn the_contract_example_parses() {
        let manifest = parse(&script_manifest("")).expect("the example is valid");

        assert_eq!(manifest.id, "example-tones");
        assert_eq!(
            manifest.entry,
            Entry::Script(Script {
                path: "tones.py".into(),
                interpreter: "python3".into()
            })
        );
        assert_eq!(
            manifest.capabilities.source,
            Some(SourceCap {
                label: "Tones".into(),
                scrobble: false
            })
        );
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let err = parse(&script_manifest(r#", "tools": []"#)).unwrap_err();
        assert!(err.contains("unknown field"), "{err}");

        let entry = script_manifest("").replace(
            r#""interpreter": "python3""#,
            r#""interpreter": "python3", "args": ["-u"]"#,
        );
        assert!(
            parse(&entry).is_err(),
            "the entry is strict: it's how the plugin runs"
        );
    }

    #[test]
    fn unknown_keys_inside_capabilities_and_meta_are_ignored() {
        let text = script_manifest("")
            .replace(r#""scrobble": false"#, r#""scrobble": false, "later": 1"#)
            .replace(r#""license": """#, r#""license": "", "funding": "x""#)
            .replace(r#""source": {"#, r#""panels": [], "source": {"#);

        let manifest = parse(&text).expect("additions below the top level are additive");
        assert!(manifest.capabilities.source.is_some());
    }

    #[test]
    fn a_bad_id_is_refused() {
        for id in [
            "",
            "a",
            "Tones",
            "-tones",
            "to nes",
            "tönes",
            &"a".repeat(65),
        ] {
            let text = script_manifest("").replace("example-tones", id);
            assert!(parse(&text).is_err(), "{id:?} should be refused");
        }

        assert!(valid_id("ab"));
        assert!(valid_id("0-9"));
        assert!(valid_id(&"a".repeat(64)));
    }

    #[test]
    fn an_api_out_of_range_is_refused() {
        let text = script_manifest("").replace(r#""api": 0"#, r#""api": 1"#);
        let err = parse(&text).unwrap_err();
        assert!(err.contains("api 1"), "{err}");

        let negative = script_manifest("").replace(r#""api": 0"#, r#""api": -1"#);
        assert!(parse(&negative).is_err());
    }

    #[test]
    fn an_entry_names_exactly_one_kind() {
        let both = with_entry(
            r#"{ "script": { "path": "a.py", "interpreter": "python3" }, "native": { "linux-x86_64": "a" } }"#,
        );
        assert!(parse(&both).is_err(), "both kinds");

        assert!(parse(&with_entry("{}")).is_err(), "neither kind");

        let native = parse(&with_entry(r#"{ "native": { "linux-x86_64": "bin/a" } }"#)).unwrap();
        assert!(matches!(native.entry, Entry::Native(_)));
    }

    #[test]
    fn a_native_entry_with_no_build_for_this_platform_is_refused() {
        let scratch = Scratch::new("native");
        scratch.file("bin/a");
        let manifest = parse(&with_entry(r#"{ "native": { "linux-x86_64": "bin/a" } }"#)).unwrap();

        let err = entry_with(&manifest, &scratch.0, "windows-x86_64", OsStr::new("")).unwrap_err();
        assert_eq!(err, "no build for windows-x86_64");

        let command = entry_with(&manifest, &scratch.0, "linux-x86_64", OsStr::new("")).unwrap();
        assert_eq!(command.get_program(), scratch.0.join("bin/a").as_os_str());
    }

    #[test]
    fn a_path_escaping_the_folder_is_refused() {
        let scratch = Scratch::new("escape");
        for rel in ["../x", "a/../../x", "/etc/passwd", ""] {
            let entry = format!(r#"{{ "native": {{ "linux-x86_64": {rel:?} }} }}"#);
            let manifest = parse(&with_entry(&entry)).unwrap();

            let err =
                entry_with(&manifest, &scratch.0, "linux-x86_64", OsStr::new("")).unwrap_err();
            assert!(err.contains("leaves the plugin folder"), "{rel}: {err}");
        }
    }

    #[test]
    fn a_script_runs_under_the_first_alias_on_path() {
        let scratch = Scratch::new("alias");
        scratch.file("tones.py");
        let bin = scratch.0.join("bin");
        let python = scratch.file("bin/python");
        let manifest = parse(&script_manifest("")).unwrap();

        // No python3 on this PATH, so the table falls through to python.
        let command = entry_with(&manifest, &scratch.0, "linux-x86_64", bin.as_os_str()).unwrap();
        assert_eq!(command.get_program(), python.as_os_str());

        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![scratch.0.join("tones.py").as_os_str()]);

        let err = entry_with(&manifest, &scratch.0, "linux-x86_64", OsStr::new("")).unwrap_err();
        assert_eq!(err, "interpreter python3 not found");
    }

    #[test]
    fn a_missing_script_is_refused() {
        let scratch = Scratch::new("missing");
        let manifest = parse(&script_manifest("")).unwrap();

        let err = entry_with(&manifest, &scratch.0, "linux-x86_64", OsStr::new("")).unwrap_err();
        assert_eq!(err, "entry tones.py is missing");
    }
}
