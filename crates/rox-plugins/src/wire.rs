//! Wire v0: newline-delimited JSON-RPC 2.0 over the plugin's stdin and
//! stdout. The frame shape follows `rox-ipc`'s protocol, copied rather than
//! shared so the plugin wire and the control socket version apart.
//!
//! Everything a plugin sends is untrusted. Each result has its own struct
//! with `deny_unknown_fields`, strings are capped at [`MAX_STRING`] and lists
//! at [`MAX_ENTRIES`], and a frame that doesn't parse is the plugin's error,
//! never a panic. Conversion to library types happens in `rox-services`.

use std::collections::BTreeMap;

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

/// A click hands a notice's link to the OS, which opens any scheme it has a
/// handler for, `file:` included. Only a web address gets that far.
fn web_url(field: &str, url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let clean = !url.chars().any(|c| c.is_whitespace() || c.is_control());

    match rest {
        Some(rest) if clean && !rest.is_empty() && !rest.starts_with('/') => Ok(()),
        _ => Err(format!("{field} isn't an http or https URL")),
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
    /// Other ways to list this place, the `views` feature. Read from a
    /// place's first page.
    #[serde(default)]
    pub views: Vec<View>,
    /// Which of `views` this page is.
    #[serde(default)]
    pub view: Option<String>,
    /// Extra columns the entries carry values for, the `fields` feature.
    #[serde(default)]
    pub fields: Vec<Field>,
}

/// A column the service knows and a row's tags don't, like a popularity.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
}

/// How a field's values read and sort.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    /// A whole number, shown short (12.3k).
    Count,
    /// 0 to 100.
    Percent,
    /// `YYYY-MM-DD`, or any prefix of it.
    Date,
    Text,
}

/// A row's values by field id: numbers for counts and percents, strings for
/// dates and text.
pub type Values = BTreeMap<String, Value>;

fn values(field: &str, values: &Values) -> Result<(), String> {
    for (id, value) in values {
        string(field, id)?;
        match value {
            Value::Number(_) => {}
            Value::String(text) => string(field, text)?,
            _ => return Err(format!("{field} {id} is neither a number nor a string")),
        }
    }

    Ok(())
}

/// One way to list a place: a filter or an order the service applies.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub id: String,
    pub label: String,
}

/// The optional parts of API 1 this host reads, sent in `hello`. A host
/// from before one refuses a result that uses it, so a plugin checks here.
pub const FEATURES: &[&str] = &[
    "notice",
    "notice-link",
    "node-kind",
    "node-art",
    "sections",
    "views",
    "fields",
    "tiles",
    "home",
    "open-duration",
    "go-to",
];

/// More than a row of chips holds.
pub const MAX_VIEWS: usize = 12;

/// Columns beside a row's title, which has to keep most of the width.
pub const MAX_FIELDS: usize = 4;

/// A line the plugin wants shown over a page, like a setting it needs
/// before it can list anything.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub text: String,
    #[serde(default)]
    pub kind: NoticeKind,
    #[serde(default)]
    pub link: Option<NoticeLink>,
}

/// A web page the notice points at, opened in the browser on a click. The
/// `notice-link` feature.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NoticeLink {
    pub url: String,
    /// The button's label. Empty reads as the host's own.
    #[serde(default)]
    pub label: String,
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

            if let Some(link) = &notice.link {
                string("notice link", &link.url)?;
                string("notice link label", &link.label)?;
                web_url("a notice link", &link.url)?;
            }
        }

        if self.views.len() > MAX_VIEWS {
            return Err(format!("a page offers more than {MAX_VIEWS} views"));
        }

        for view in &self.views {
            if view.id.is_empty() || view.label.trim().is_empty() {
                return Err("a view has an empty id or label".into());
            }
            string("view id", &view.id)?;
            string("view label", &view.label)?;
        }

        if let Some(view) = &self.view
            && !self.views.iter().any(|v| &v.id == view)
        {
            return Err(format!("the page is view {view:?}, which it doesn't offer"));
        }

        if self.fields.len() > MAX_FIELDS {
            return Err(format!("a page declares more than {MAX_FIELDS} fields"));
        }

        for (ix, field) in self.fields.iter().enumerate() {
            if field.id.is_empty() || field.label.trim().is_empty() {
                return Err("a field has an empty id or label".into());
            }
            if self.fields[..ix].iter().any(|f| f.id == field.id) {
                return Err(format!("field {:?} is declared twice", field.id));
            }
            string("field id", &field.id)?;
            string("field label", &field.label)?;
        }

        // A value for a field the page never declared has no column to go in.
        let declared = |row: &Values| {
            row.keys()
                .find(|id| !self.fields.iter().any(|f| &f.id == *id))
                .map_or(Ok(()), |id| {
                    Err(format!(
                        "a value for field {id:?}, which the page doesn't declare"
                    ))
                })
        };

        self.entries.iter().try_for_each(|entry| match entry {
            Entry::Node(node) => node.check().and_then(|()| declared(&node.values)),
            Entry::Track(track) => track.check().and_then(|()| declared(&track.values)),
            Entry::Section(section) => section.check(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Entry {
    Node(Node),
    Track(Track),
    /// A heading over the entries after it, the `sections` feature.
    Section(Section),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub title: String,
    /// How the entries under it show, the `tiles` feature.
    #[serde(default)]
    pub layout: Layout,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Rows,
    /// A shelf of cover tiles scrolled sideways, for albums, playlists and
    /// the like. Tracks read better as rows.
    Tiles,
}

impl Checked for Section {
    fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("a section has no title".into());
        }

        string("section title", &self.title)
    }
}

/// What a node is, for its icon: the `node-kind` feature.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Album,
    Playlist,
    Artist,
    Folder,
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
    #[serde(default)]
    pub kind: Option<NodeKind>,
    /// A key for `source.cover`, the `node-art` feature. Empty for none.
    #[serde(default)]
    pub art: String,
    #[serde(default)]
    pub values: Values,
    /// The service's home among the roots, the `home` feature. rox can list
    /// its page under the roots instead of as a node of its own.
    #[serde(default)]
    pub home: bool,
}

impl Checked for Node {
    fn check(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("a node has an empty id".into());
        }

        string("node id", &self.id)?;
        string("title", &self.title)?;
        string("subtitle", &self.subtitle)?;
        string("node art", &self.art)?;
        values("node value", &self.values)
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
    /// The `fields` feature's values. Sync ignores them: a kept row holds
    /// only its tags.
    pub values: Values,
    /// The `go-to` feature. Sync ignores it too. Boxed, since most tracks
    /// leave it out.
    pub go_to: Option<Box<GoTo>>,
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
        .try_for_each(|(field, value)| string(field, value))?;

        values("track value", &self.values)?;
        self.go_to.as_ref().map_or(Ok(()), |go_to| go_to.check())
    }
}

/// The nodes a track's album and artists open, for Go to in the source
/// browser.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GoTo {
    pub album: Option<Node>,
    pub artists: Vec<Node>,
}

impl Checked for GoTo {
    fn check(&self) -> Result<(), String> {
        list("go_to artists", &self.artists)?;

        self.album
            .iter()
            .chain(&self.artists)
            .try_for_each(Node::check)
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

/// A batch of a radio seeded from a track or a node, the `radio` capability.
/// A null `cursor` means the station ran out; the host seeds a new one.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RadioPage {
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub cursor: Option<String>,
}

impl Checked for RadioPage {
    fn check(&self) -> Result<(), String> {
        list("tracks", &self.tracks)?;
        cursor(&self.cursor)?;

        self.tracks.iter().try_for_each(Track::check)
    }
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
    /// The stream's length, for a container that doesn't state one. Only
    /// sent once `hello` listed `open-duration`.
    #[serde(default)]
    pub duration_ms: Option<u64>,
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

/// A track's or a node's web page, `source.link`'s answer. rox only opens it
/// in the browser or copies it, on the user's click, and never fetches it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub url: String,
}

impl Checked for Link {
    fn check(&self) -> Result<(), String> {
        string("link", &self.url)?;
        web_url("a link", &self.url)
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

/// What an action left for the user when it ended. rox shows `message` in
/// a toast, and the link and the path become its buttons and nothing more:
/// the link opens in the browser and the path shows in the file manager,
/// each only on a click. rox never fetches the link or opens the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub message: String,
    pub link: Option<String>,
    pub reveal: Option<String>,
}

impl Outcome {
    fn check(&self) -> Result<(), String> {
        string("message", &self.message)?;

        if let Some(link) = &self.link {
            string("link", link)?;
            web_url("an outcome's link", link)?;
        }

        if let Some(path) = &self.reveal {
            string("reveal", path)?;

            let clean = !path.chars().any(|c| c.is_control());
            if !clean || !std::path::Path::new(path).is_absolute() {
                return Err("reveal isn't an absolute path".into());
            }
        }

        Ok(())
    }
}

/// `source.action`'s answer: finished at once, or a job rox polls.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionAnswer {
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub reveal: Option<String>,
}

impl ActionAnswer {
    pub fn outcome(&self) -> Outcome {
        Outcome {
            message: self.message.clone(),
            link: self.link.clone(),
            reveal: self.reveal.clone(),
        }
    }
}

impl Checked for ActionAnswer {
    fn check(&self) -> Result<(), String> {
        if let Some(job) = &self.job {
            string("job", job)?;
            if job.is_empty() {
                return Err("job is empty".into());
            }
        }

        self.outcome().check()
    }
}

/// `source.job`'s answer, polled about once a second. An `error` ends the
/// job as failed; `finished` ends it with the outcome fields.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JobState {
    #[serde(default)]
    pub done: u64,
    /// Zero when the plugin can't tell how much there is.
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub finished: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub reveal: Option<String>,
}

impl JobState {
    pub fn outcome(&self) -> Outcome {
        Outcome {
            message: self.message.clone(),
            link: self.link.clone(),
            reveal: self.reveal.clone(),
        }
    }
}

impl Checked for JobState {
    fn check(&self) -> Result<(), String> {
        string("text", &self.text)?;
        if let Some(error) = &self.error {
            string("error", error)?;
        }

        self.outcome().check()
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

        let linked: Page = decode(json!({
            "entries": [],
            "notice": {"text": "Sign in", "link": {"url": "https://example.com/abc", "label": "Sign In"}}
        }))
        .unwrap();
        let link = linked.notice.and_then(|n| n.link).expect("the link parses");
        assert_eq!(
            (link.url.as_str(), link.label.as_str()),
            ("https://example.com/abc", "Sign In")
        );

        for bad in [
            json!({"entries": [], "notice": {"text": " "}}),
            json!({"entries": [], "notice": {"text": "x", "kind": "urgent"}}),
            json!({"entries": [], "notice": {"text": "x", "link": "y"}}),
            json!({"entries": [], "notice": {"text": "x", "link": {"url": "https://a.b", "extra": 1}}}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn nodes_carry_a_kind_and_art_and_sections_head_them() {
        let page: Page = decode(json!({
            "entries": [
                {"section": {"title": "Albums"}},
                {"node": {"id": "album:1", "title": "A", "kind": "album", "art": "album:1"}},
                {"node": {"id": "plain", "title": "B"}}
            ]
        }))
        .unwrap();

        assert!(
            matches!(&page.entries[0], Entry::Section(s) if s.title == "Albums" && s.layout == Layout::Rows)
        );

        let shelf: Page =
            decode(json!({"entries": [{"section": {"title": "New", "layout": "tiles"}}]})).unwrap();
        assert!(matches!(&shelf.entries[0], Entry::Section(s) if s.layout == Layout::Tiles));
        assert!(
            decode::<Page>(json!({"entries": [{"section": {"title": "New", "layout": "cards"}}]}))
                .is_err()
        );
        assert!(
            matches!(&page.entries[1], Entry::Node(n) if n.kind == Some(NodeKind::Album) && n.art == "album:1")
        );
        assert!(
            matches!(&page.entries[2], Entry::Node(n) if n.kind.is_none() && n.art.is_empty() && !n.home)
        );

        let roots: Page =
            decode(json!({"entries": [{"node": {"id": "home", "title": "Home", "home": true}}]}))
                .unwrap();
        assert!(matches!(&roots.entries[0], Entry::Node(n) if n.home));

        for bad in [
            json!({"entries": [{"section": {"title": " "}}]}),
            json!({"entries": [{"node": {"id": "x", "title": "x", "kind": "genre"}}]}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_track_names_the_nodes_its_album_and_artists_open() {
        let page: Page = decode(json!({
            "entries": [{"track": {"key": "t1", "go_to": {
                "album": {"id": "album:1", "title": "A", "kind": "album", "collection": true},
                "artists": [
                    {"id": "artist:1", "title": "B", "kind": "artist"},
                    {"id": "artist:2", "title": "C", "kind": "artist"}
                ]
            }}}]
        }))
        .unwrap();

        let Entry::Track(track) = &page.entries[0] else {
            panic!("not a track");
        };
        let go_to = track.go_to.as_ref().unwrap();
        assert_eq!(go_to.album.as_ref().unwrap().id, "album:1");
        assert_eq!(go_to.artists.len(), 2);

        let bare: Page = decode(json!({"entries": [{"track": {"key": "t2"}}]})).unwrap();
        assert!(matches!(&bare.entries[0], Entry::Track(t) if t.go_to.is_none()));

        for bad in [
            json!({"entries": [{"track": {"key": "t", "go_to": {"album": {"id": "", "title": "A"}}}}]}),
            json!({"entries": [{"track": {"key": "t", "go_to": {"label": "A"}}}]}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_page_offers_views_and_names_its_own() {
        let page: Page = decode(json!({
            "entries": [],
            "views": [{"id": "all", "label": "All"}, {"id": "albums", "label": "Albums"}],
            "view": "albums"
        }))
        .unwrap();
        assert_eq!(page.views.len(), 2);
        assert_eq!(page.view.as_deref(), Some("albums"));

        let many: Vec<Value> = (0..=MAX_VIEWS)
            .map(|i| json!({"id": i.to_string(), "label": "v"}))
            .collect();

        for bad in [
            json!({"entries": [], "views": [{"id": "all", "label": "All"}], "view": "gone"}),
            json!({"entries": [], "views": [{"id": "", "label": "All"}]}),
            json!({"entries": [], "views": many}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn fields_declare_the_columns_rows_carry_values_for() {
        let page: Page = decode(json!({
            "fields": [{"id": "pop", "label": "Popularity", "kind": "percent"}],
            "entries": [
                {"track": {"key": "1", "values": {"pop": 78}}},
                {"node": {"id": "n", "title": "N", "values": {"pop": 40}}},
                {"track": {"key": "2"}}
            ]
        }))
        .unwrap();
        assert_eq!(page.fields[0].kind, FieldKind::Percent);
        assert!(matches!(&page.entries[0], Entry::Track(t) if t.values["pop"] == 78));

        let five: Vec<Value> = (0..=MAX_FIELDS)
            .map(|i| json!({"id": i.to_string(), "label": "f", "kind": "count"}))
            .collect();

        for bad in [
            json!({"entries": [{"track": {"key": "1", "values": {"pop": 1}}}]}),
            json!({"fields": [{"id": "a", "label": "A", "kind": "count"}], "entries": [{"track": {"key": "1", "values": {"a": [1]}}}]}),
            json!({"fields": [{"id": "a", "label": "A", "kind": "rating"}], "entries": []}),
            json!({"fields": [{"id": "a", "label": "A", "kind": "count"}, {"id": "a", "label": "B", "kind": "count"}], "entries": []}),
            json!({"fields": five, "entries": []}),
        ] {
            assert!(decode::<Page>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_radio_page_is_tracks_and_a_cursor() {
        let page: RadioPage =
            decode(json!({"tracks": [{"key": "1"}, {"key": "2"}], "cursor": "2"})).unwrap();
        assert_eq!(page.tracks.len(), 2);
        assert_eq!(page.cursor.as_deref(), Some("2"));

        assert!(decode::<RadioPage>(json!({"tracks": [{"key": ""}]})).is_err());
        assert!(decode::<RadioPage>(json!({"tracks": [], "station": "x"})).is_err());
    }

    #[test]
    fn a_notice_link_is_only_a_web_address() {
        for url in ["https://example.com", "http://127.0.0.1:8080/x?y=z"] {
            assert!(web_url("a link", url).is_ok(), "{url}");
        }

        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ms-settings:privacy",
            "https://",
            "https:///path",
            "HTTPS://example.com",
            "https://example.com/a b",
            "https://example.com/\n",
        ] {
            assert!(web_url("a link", url).is_err(), "{url}");
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

    #[test]
    fn a_link_is_a_web_page_or_nothing() {
        let link: Link = decode(json!({"url": "https://example.com/track/1"})).unwrap();
        assert_eq!(link.url, "https://example.com/track/1");

        for bad in [
            json!({"url": "javascript:alert(1)"}),
            json!({"url": "file:///etc/passwd"}),
            json!({"url": "https://example.com/a b"}),
            json!({"url": "https://example.com", "label": "x"}),
        ] {
            assert!(decode::<Link>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_action_answers_a_job_or_an_outcome() {
        let job: ActionAnswer = decode(json!({"job": "j1"})).unwrap();
        assert_eq!(job.job.as_deref(), Some("j1"));

        let done: ActionAnswer =
            decode(json!({"message": "Saved", "reveal": "/home/me/Videos/a.mp4"})).unwrap();
        assert_eq!(
            done.outcome().reveal.as_deref(),
            Some("/home/me/Videos/a.mp4")
        );

        assert!(decode::<ActionAnswer>(json!({"job": ""})).is_err());
        assert!(decode::<ActionAnswer>(json!({"then": "x"})).is_err());
    }

    #[test]
    fn an_outcome_names_only_a_web_link_and_an_absolute_path() {
        let refused = [
            json!({"link": "file:///etc/passwd"}),
            json!({"link": "javascript:alert(1)"}),
            json!({"reveal": "relative/path"}),
            json!({"reveal": "/tmp/a\nb"}),
        ];

        for answer in refused {
            assert!(decode::<ActionAnswer>(answer.clone()).is_err(), "{answer}");
            assert!(decode::<JobState>(answer.clone()).is_err(), "{answer}");
        }
    }

    #[test]
    fn a_job_state_reads_progress_and_its_end() {
        let running: JobState = decode(json!({"done": 3, "total": 10, "text": "a.wav"})).unwrap();
        assert!(!running.finished && running.error.is_none());

        let failed: JobState = decode(json!({"error": "disk full"})).unwrap();
        assert_eq!(failed.error.as_deref(), Some("disk full"));
    }
}
