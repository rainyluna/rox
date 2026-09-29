//! Wire v0: newline-delimited JSON-RPC 2.0 over the plugin's stdin and
//! stdout. The frame shape follows `rox-ipc`'s protocol, copied rather than
//! shared so the plugin wire and the control socket version apart.
//!
//! Everything a plugin sends is untrusted. Each result has its own struct
//! with `deny_unknown_fields`, strings are capped at [`MAX_STRING`] and lists
//! at [`MAX_ENTRIES`], and a frame that doesn't parse is the plugin's error,
//! never a panic. Conversion to library types happens in `rox-services`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The same cap as the control socket's frames.
pub const MAX_FRAME: usize = 1 << 20;

pub const MAX_STRING: usize = 4096;

/// Per page, for `entries` and `tracks` alike.
pub const MAX_ENTRIES: usize = 500;

/// So a base64 answer stays under [`MAX_FRAME`].
pub const MAX_READ: u32 = 512 * 1024;

#[derive(Serialize)]
pub struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request<'_> {
    pub fn line(id: u64, method: &str, params: Value) -> Vec<u8> {
        let request = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };

        // A `Value` tree with string keys always serializes.
        let mut line = serde_json::to_vec(&request).unwrap_or_default();
        line.push(b'\n');

        line
    }
}

/// A plugin's answer. `result: null` is a real answer (`source.close`,
/// `shutdown`, a cover it doesn't have), so a present null stays apart from a
/// missing field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: Value,
    #[serde(default, deserialize_with = "present")]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// JSON-RPC allows it; the host has no use for it.
    #[serde(default)]
    pub data: Option<Value>,
}

/// One inbound line, sorted into what the host acts on.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    Answer {
        id: u64,
        result: Result<Value, String>,
    },
    /// Not a frame the host can pair with anything. Logged, then dropped.
    Junk(String),
}

pub fn parse_line(line: &[u8]) -> Inbound {
    let response: Response = match serde_json::from_slice(line) {
        Ok(response) => response,
        Err(e) => return Inbound::Junk(format!("unparseable frame: {e}")),
    };

    if response.jsonrpc.as_deref().is_some_and(|v| v != "2.0") {
        return Inbound::Junk("jsonrpc is not \"2.0\"".into());
    }

    let Some(id) = response.id.as_u64() else {
        return Inbound::Junk(format!(
            "an answer to id {} the host never sent",
            response.id
        ));
    };

    let result = match (response.result, response.error) {
        (Some(value), None) => Ok(value),
        (None, Some(error)) => Err(clip(&error.message).to_string()),
        (Some(_), Some(_)) => Err("the plugin answered with a result and an error".into()),
        (None, None) => Err("the plugin answered with neither a result nor an error".into()),
    };

    Inbound::Answer { id, result }
}

/// Error text from a plugin reaches the log and the UI, so it's cut short.
fn clip(text: &str) -> &str {
    if text.len() <= MAX_STRING {
        return text;
    }

    let mut end = MAX_STRING;
    while !text.is_char_boundary(end) {
        end -= 1;
    }

    &text[..end]
}

/// A result's own checks past what serde enforces.
pub trait Checked: Sized {
    fn check(&self) -> Result<(), String>;
}

/// The typed form of a result, checked.
pub fn decode<T: for<'de> Deserialize<'de> + Checked>(value: Value) -> Result<T, String> {
    let typed: T = serde_json::from_value(value).map_err(|e| format!("malformed result: {e}"))?;
    typed.check()?;

    Ok(typed)
}

fn string(field: &str, value: &str) -> Result<(), String> {
    match value.len() > MAX_STRING {
        true => Err(format!("{field} is longer than {MAX_STRING} bytes")),
        false => Ok(()),
    }
}

fn list<T>(field: &str, items: &[T]) -> Result<(), String> {
    match items.len() > MAX_ENTRIES {
        true => Err(format!("{field} holds more than {MAX_ENTRIES} items")),
        false => Ok(()),
    }
}

fn cursor(value: &Option<String>) -> Result<(), String> {
    value.as_deref().map_or(Ok(()), |c| string("cursor", c))
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub name: String,
    pub version: String,
    pub api: u32,
}

impl Checked for Hello {
    fn check(&self) -> Result<(), String> {
        string("name", &self.name)?;
        string("version", &self.version)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub notice: Option<Notice>,
}

/// The optional parts of API 1 this host reads, sent in `hello`. A host
/// from before one refuses a result that uses it, so a plugin checks here.
pub const FEATURES: &[&str] = &["notice"];

/// A line the plugin wants shown over a page, like a setting it needs
/// before it can list anything.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub text: String,
    #[serde(default)]
    pub kind: NoticeKind,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoticeKind {
    /// Something worth knowing about the page.
    #[default]
    Info,
    /// The plugin needs a setting from the user, so rox offers its settings.
    Setup,
}

impl Checked for Page {
    fn check(&self) -> Result<(), String> {
        list("entries", &self.entries)?;
        cursor(&self.cursor)?;
        if let Some(notice) = &self.notice {
            if notice.text.trim().is_empty() {
                return Err("a notice has no text".into());
            }
            string("notice", &notice.text)?;
        }

        self.entries.iter().try_for_each(|entry| match entry {
            Entry::Node(node) => node.check(),
            Entry::Track(track) => track.check(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Entry {
    Node(Node),
    Track(Track),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub collection: bool,
}

impl Checked for Node {
    fn check(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("a node has an empty id".into());
        }

        string("node id", &self.id)?;
        string("title", &self.title)?;
        string("subtitle", &self.subtitle)
    }
}

/// Every field but `key` defaults, empty or zero: the contract says a plugin
/// always sends them, and a missing one is cheaper to forgive than to refuse.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Track {
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub genre: String,
    pub year: u16,
    pub disc_no: u16,
    pub track_no: u16,
    pub duration_ms: u32,
    pub codec: String,
    pub bitrate_kbps: u16,
    pub live: bool,
}

impl Checked for Track {
    fn check(&self) -> Result<(), String> {
        if self.key.is_empty() {
            return Err("a track has an empty key".into());
        }

        [
            ("key", &self.key),
            ("title", &self.title),
            ("artist", &self.artist),
            ("album_artist", &self.album_artist),
            ("album", &self.album),
            ("genre", &self.genre),
            ("codec", &self.codec),
        ]
        .into_iter()
        .try_for_each(|(field, value)| string(field, value))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SyncPage {
    #[serde(default)]
    pub unchanged: bool,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

impl Checked for SyncPage {
    fn check(&self) -> Result<(), String> {
        list("tracks", &self.tracks)?;
        cursor(&self.cursor)?;
        self.token
            .as_deref()
            .map_or(Ok(()), |t| string("token", t))?;

        self.tracks.iter().try_for_each(Track::check)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub stream: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(default)]
    pub seekable: bool,
    #[serde(default)]
    pub live: bool,
    /// Absent reads as whole.
    #[serde(default)]
    pub buffer: Option<Buffer>,
}

/// How much of a stream the host may fetch ahead. A plugin can only lower it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Buffer {
    Whole,
    Ahead,
}

impl Checked for Open {
    fn check(&self) -> Result<(), String> {
        if self.stream.is_empty() {
            return Err("an open answered an empty stream id".into());
        }

        string("stream", &self.stream)?;
        string("hint", &self.hint)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub data: String,
}

impl Checked for Read {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Read {
    /// The bytes, refused past what was asked for.
    pub fn bytes(&self, asked: u32) -> Result<Vec<u8>, String> {
        let bytes = BASE64
            .decode(&self.data)
            .map_err(|e| format!("read data is not base64: {e}"))?;

        match bytes.len() > asked as usize {
            true => Err(format!(
                "a read answered {} bytes for {asked} asked",
                bytes.len()
            )),
            false => Ok(bytes),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cover {
    pub mime: String,
    pub data: String,
}

impl Checked for Cover {
    fn check(&self) -> Result<(), String> {
        string("mime", &self.mime)
    }
}

impl Cover {
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        BASE64
            .decode(&self.data)
            .map_err(|e| format!("cover data is not base64: {e}"))
    }
}

/// `null` answers: close, shutdown.
pub fn is_null(value: &Value) -> Result<(), String> {
    match value.is_null() {
        true => Ok(()),
        false => Err(format!("expected null, got {value}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_is_one_line_with_its_id() {
        let line = Request::line(
            7,
            "source.read",
            json!({"stream": "s1", "offset": 0, "len": 4}),
        );

        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);

        let back: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(back["jsonrpc"], "2.0");
        assert_eq!(back["id"], 7);
        assert_eq!(back["method"], "source.read");
    }

    #[test]
    fn a_request_with_no_params_leaves_the_field_out() {
        let back: Value =
            serde_json::from_slice(&Request::line(1, "shutdown", Value::Null)).unwrap();
        assert!(back.get("params").is_none());
    }

    #[test]
    fn a_null_result_is_an_answer() {
        let inbound = parse_line(br#"{"jsonrpc":"2.0","id":3,"result":null}"#);
        assert_eq!(
            inbound,
            Inbound::Answer {
                id: 3,
                result: Ok(Value::Null)
            }
        );
    }

    #[test]
    fn neither_result_nor_error_is_the_plugins_error() {
        let inbound = parse_line(br#"{"jsonrpc":"2.0","id":3}"#);
        assert!(matches!(
            inbound,
            Inbound::Answer {
                id: 3,
                result: Err(_)
            }
        ));
    }

    #[test]
    fn an_error_answer_carries_its_message() {
        let inbound = parse_line(
            br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"no such video","data":{"x":1}}}"#,
        );
        assert_eq!(
            inbound,
            Inbound::Answer {
                id: 4,
                result: Err("no such video".into())
            }
        );
    }

    #[test]
    fn the_jsonrpc_field_is_optional_but_checked() {
        assert!(matches!(
            parse_line(br#"{"id":1,"result":1}"#),
            Inbound::Answer { id: 1, .. }
        ));
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"1.0","id":1,"result":1}"#),
            Inbound::Junk(_)
        ));
    }

    #[test]
    fn junk_never_panics() {
        for line in [
            &b"not json"[..],
            br#"{"id":1,"result":1,"extra":true}"#,
            br#"{"id":"one","result":1}"#,
            br#"{"id":null,"error":{"code":1,"message":"x"}}"#,
            br#"[]"#,
            b"",
        ] {
            assert!(matches!(parse_line(line), Inbound::Junk(_)), "{line:?}");
        }
    }

    #[test]
    fn a_page_holds_nodes_and_tracks() {
        let page: Page = decode(json!({
            "entries": [
                {"node": {"id": "likes", "title": "Likes", "subtitle": "", "collection": true}},
                {"track": {"key": "k1", "title": "One", "duration_ms": 1000}}
            ],
            "cursor": null
        }))
        .unwrap();

        assert_eq!(page.entries.len(), 2);
        assert!(
            matches!(&page.entries[1], Entry::Track(t) if t.key == "k1" && t.artist.is_empty())
        );
    }

    #[test]
    fn a_page_carries_a_notice() {
        let page: Page = decode(json!({
            "entries": [],
            "notice": {"text": "Set a cookies file", "kind": "setup"}
        }))
        .unwrap();
        let notice = page.notice.expect("the notice parses");
        assert_eq!(notice.kind, NoticeKind::Setup);

        let plain: Page = decode(json!({"entries": [], "notice": {"text": "hi"}})).unwrap();
        assert_eq!(plain.notice.map(|n| n.kind), Some(NoticeKind::Info));

        for bad in [
            json!({"entries": [], "notice": {"text": " "}}),
            json!({"entries": [], "notice": {"text": "x", "kind": "urgent"}}),
            json!({"entries": [], "notice": {"text": "x", "link": "y"}}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn caps_and_empty_ids_are_refused() {
        let long = "x".repeat(MAX_STRING + 1);

        let err = decode::<Page>(json!({"entries": [{"track": {"key": long}}]})).unwrap_err();
        assert!(err.contains("key is longer"), "{err}");

        let err =
            decode::<Page>(json!({"entries": [{"node": {"id": "", "title": "t"}}]})).unwrap_err();
        assert!(err.contains("empty id"), "{err}");

        let many: Vec<Value> = (0..=MAX_ENTRIES)
            .map(|i| json!({"key": format!("k{i}")}))
            .collect();
        let err = decode::<SyncPage>(json!({"tracks": many})).unwrap_err();
        assert!(err.contains("more than 500"), "{err}");

        let err =
            decode::<Page>(json!({"entries": [{"track": {"key": "k", "rating": 5}}]})).unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn a_read_refuses_more_than_it_asked_for() {
        let read: Read = decode(json!({"data": BASE64.encode([1u8, 2, 3, 4])})).unwrap();

        assert_eq!(read.bytes(4).unwrap(), vec![1, 2, 3, 4]);
        assert!(read.bytes(3).is_err());

        let bad: Read = decode(json!({"data": "***"})).unwrap();
        assert!(bad.bytes(10).is_err());
    }

    #[test]
    fn an_open_names_its_stream() {
        let open: Open = decode(
            json!({"stream": "s1", "hint": "m4a", "length": 10, "seekable": true, "live": false}),
        )
        .unwrap();
        assert_eq!(open.length, Some(10));

        assert!(decode::<Open>(json!({"stream": ""})).is_err());

        let ahead: Open = decode(json!({"stream": "s1", "buffer": "ahead"})).unwrap();
        assert_eq!(ahead.buffer, Some(Buffer::Ahead));
        assert!(decode::<Open>(json!({"stream": "s1", "buffer": "everything"})).is_err());
    }
}
