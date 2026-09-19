//! radio-browser.info, the community station directory, and the answer to
//! "where do I even get a stream URL". It's keyless and public, with two
//! asks from its maintainers that this module honours: send a real
//! User-Agent (the shared agent does), and don't point every call at the
//! `all.` round-robin name. So the first search fetches the mirror list
//! off that name, picks one mirror for the rest of the session, and only
//! falls back to the round-robin name when the list can't be had.
//!
//! What comes back is plain data for the stations panel to show. Nothing
//! is written here and nothing is played; a hit becomes a station row only
//! when someone adds it, through the same `stations::put` a typed URL goes
//! through. Two kinds of hit are dropped before they're shown. Broken
//! ones, since the directory checks every stream and says which failed.
//! HLS ones, since a `.m3u8` is a playlist of segments and the engine's
//! transport reads one byte stream.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{number, text};
use crate::providers::{agent, net_reason};

/// The round-robin name the directory publishes. Asked for the mirror list
/// once; searches go to the mirror it named.
const ANY_MIRROR: &str = "all.api.radio-browser.info";

/// One station the directory knows about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// The stream itself, past any `.pls` the station's site hands out;
    /// the directory resolves those on its side.
    pub url: String,
    pub homepage: String,
    pub favicon: String,
    /// The directory's tags, comma joined the way it sends them.
    pub tags: String,
    /// ISO country code, upper case, empty when the directory has none.
    pub country: String,
    /// The codec as the directory last heard it ("MP3", "AAC"), empty when
    /// it doesn't know.
    pub codec: String,
    /// Zero when the directory doesn't know.
    pub bitrate_kbps: u16,
    pub votes: u64,
}

/// Stations whose name matches `text`, most voted first, `limit` at most.
/// Blocking, so it runs on the background executor like every other call
/// in this crate.
pub fn search(text: &str, limit: usize) -> Result<Vec<Found>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }

    let body = agent()
        .get(&format!("https://{}/json/stations/search", mirror()))
        .query("name", text)
        .query("limit", &limit.to_string())
        .query("hidebroken", "true")
        .query("order", "votes")
        .query("reverse", "true")
        .call()
        .map_err(|e| net_reason(&e))?
        .into_string()
        .map_err(|e| e.to_string())?;

    parse(&body)
}

/// The mirror this session talks to. Picked once: the directory asks that
/// clients spread themselves across mirrors rather than all leaning on the
/// round-robin name, and the pick has to be per client, not per call, or
/// a search pages against three different caches.
fn mirror() -> &'static str {
    static MIRROR: OnceLock<String> = OnceLock::new();

    MIRROR.get_or_init(|| {
        let listed = agent()
            .get(&format!("https://{ANY_MIRROR}/json/servers"))
            .call()
            .ok()
            .and_then(|response| response.into_string().ok())
            .map(|body| mirrors(&body))
            .unwrap_or_default();

        pick(&listed).unwrap_or_else(|| ANY_MIRROR.to_string())
    })
}

/// The mirror names off a `/json/servers` reply, deduplicated. The list
/// carries one entry per address, so a dual-stack mirror shows up twice.
fn mirrors(body: &str) -> Vec<String> {
    let Ok(Value::Array(servers)) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };

    let mut names: Vec<String> = servers
        .iter()
        .map(|server| text(server, "name"))
        .filter(|name| !name.is_empty())
        .collect();

    names.sort();
    names.dedup();
    names
}

/// One name off the list, spread by the clock rather than a random
/// number generator this crate doesn't otherwise need.
fn pick(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);

    names.get(nanos % names.len()).cloned()
}

/// The search reply as stations worth showing. The filters here are the
/// module header's: nothing broken, nothing HLS, nothing without a stream
/// the transport can open.
fn parse(body: &str) -> Result<Vec<Found>, String> {
    let Value::Array(stations) = serde_json::from_str::<Value>(body).map_err(|e| e.to_string())?
    else {
        return Err("unrecognized reply".to_string());
    };

    Ok(stations.iter().filter_map(found).collect())
}

fn found(station: &Value) -> Option<Found> {
    if number(station, "hls") != 0 || number(station, "lastcheckok") == 0 {
        return None;
    }

    // The resolved URL is the stream past a .pls; the plain one is what
    // the station's site links, and it's the fallback when the directory
    // hasn't resolved it yet.
    let mut url = text(station, "url_resolved");
    if url.is_empty() {
        url = text(station, "url");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    let name = text(station, "name");
    if name.is_empty() {
        return None;
    }

    Some(Found {
        name,
        url,
        homepage: text(station, "homepage"),
        favicon: text(station, "favicon"),
        tags: text(station, "tags"),
        country: text(station, "countrycode").to_uppercase(),
        codec: text(station, "codec"),
        bitrate_kbps: number(station, "bitrate").clamp(0, u16::MAX as i64) as u16,
        votes: number(station, "votes").max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three stations as the directory sends them: one good, one HLS, one
    /// whose resolved URL is empty and falls back to the plain one.
    const REPLY: &str = r#"[
        {"name":"Adroit Jazz Underground","url":"https://icecast.walmradio.com:8443/jazz",
         "url_resolved":"https://icecast.walmradio.com:8443/jazz","homepage":"https://walmradio.com/",
         "favicon":"https://icecast.walmradio.com:8443/jazz.jpg","tags":"bebop,cool jazz",
         "countrycode":"us","codec":"MP3","bitrate":320,"hls":0,"lastcheckok":1,"votes":181857},
        {"name":"Segmented","url":"https://example.com/live.m3u8","url_resolved":"https://example.com/live.m3u8",
         "countrycode":"DE","codec":"AAC","bitrate":128,"hls":1,"lastcheckok":1,"votes":5},
        {"name":"Plain","url":"http://example.org/stream","url_resolved":"",
         "countrycode":"","codec":"","bitrate":"96","hls":0,"lastcheckok":1,"votes":"2"}
    ]"#;

    #[test]
    fn a_reply_keeps_the_playable_stations() {
        let found = parse(REPLY).unwrap();

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Adroit Jazz Underground");
        assert_eq!(found[0].url, "https://icecast.walmradio.com:8443/jazz");
        assert_eq!(found[0].country, "US");
        assert_eq!(found[0].codec, "MP3");
        assert_eq!(found[0].bitrate_kbps, 320);
        assert_eq!(found[0].votes, 181857);
        assert_eq!(found[0].tags, "bebop,cool jazz");
    }

    #[test]
    fn an_unresolved_url_falls_back_to_the_plain_one() {
        let found = parse(REPLY).unwrap();

        assert_eq!(found[1].url, "http://example.org/stream");
        // Numbers sent as strings still read.
        assert_eq!(found[1].bitrate_kbps, 96);
        assert_eq!(found[1].votes, 2);
        assert_eq!(found[1].country, "");
    }

    #[test]
    fn broken_and_non_http_stations_are_dropped() {
        let found = parse(
            r#"[{"name":"Down","url":"http://a/x","url_resolved":"http://a/x","hls":0,"lastcheckok":0},
                {"name":"Odd","url":"rtsp://a/x","url_resolved":"rtsp://a/x","hls":0,"lastcheckok":1},
                {"name":"","url":"http://a/x","url_resolved":"http://a/x","hls":0,"lastcheckok":1}]"#,
        )
        .unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn a_reply_that_is_not_a_list_is_an_error() {
        assert!(parse(r#"{"error":"nope"}"#).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn mirrors_dedupe_the_dual_stack_entries() {
        let names = mirrors(
            r#"[{"ip":"91.98.4.78","name":"de1.api.radio-browser.info"},
                {"ip":"2a01:4f8::1","name":"de1.api.radio-browser.info"},
                {"ip":"1.2.3.4","name":"fi1.api.radio-browser.info"}]"#,
        );

        assert_eq!(
            names,
            vec!["de1.api.radio-browser.info", "fi1.api.radio-browser.info"]
        );
        assert!(pick(&names).is_some());
        assert!(pick(&[]).is_none());
    }
}
