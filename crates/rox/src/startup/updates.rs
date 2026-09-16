//! The update check: ask GitHub for the newest published release and weigh
//! its tag against the running build. The check itself only reports what it
//! found (a newer release, its page, its artifacts) and caches the result in
//! settings; a launch runs it at most once a day, and only when the
//! settings toggle leaves it on. The About page's button checks now
//! regardless. [`updater`](crate::startup::updater) acts on the answer,
//! called from the About page or, opted in, straight from the launch check
//! here.
//!
//! ## Release candidates
//!
//! A release candidate is a prerelease on GitHub tagged with a semver
//! prerelease suffix (`v1.25.0-rc.1`), which the release workflow cuts
//! whenever the workspace version carries one. Versions order the way the
//! spec says: `1.25.0-rc.1` sits above every `1.24.x` and below `1.25.0`
//! itself. Candidates stay out of the check unless the user opts in from
//! settings, with one exception: a build that is itself a candidate always
//! sees them, so `rc.1` learns about `rc.2` and then about the stable
//! release that closes the cycle.

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::Deserialize;

use rox_core::settings::{Settings, UpdateCache};
use rox_net::providers::agent;

use crate::startup::updater;

/// The build's own version, the left side of every comparison.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The newest releases, prereleases included. GitHub's "latest" endpoint
/// would answer the stable case on its own, but it hides prereleases, so
/// the check reads the list and picks by version itself. Newest first by
/// creation, so the stable release and any candidate above it are both
/// within the first page.
const RELEASES: &str = "https://api.github.com/repos/zealsprince/rox/releases?per_page=10";

/// How long a cached check is good for before a launch runs another: a day.
const CHECK_INTERVAL: u64 = 24 * 60 * 60;

/// The version the menubar's "Update Available" chip announces, a live
/// static like the palette's flags: the menubar reads it per frame, where a
/// settings-file load has no place. Some only when the cached check found a
/// newer release that hasn't been dismissed; installs that can't replace
/// themselves see it too, since knowing a release exists doesn't need the
/// updater. Seeded at launch and refreshed when a check lands or the chip
/// is dismissed.
static AVAILABLE: RwLock<Option<String>> = RwLock::new(None);

/// What the menubar chip shows, if anything.
pub fn available() -> Option<String> {
    AVAILABLE.read().unwrap().clone()
}

/// Recompute the chip's static from the settings on hand: the cached
/// release against the running build and the dismissal. Runs when the
/// cache or the dismissal moves, never per frame.
pub fn refresh_available(settings: &Settings) {
    let version = settings
        .session
        .update_cache
        .as_ref()
        .filter(|cache| {
            let release = Release {
                version: cache.latest.clone(),
                url: cache.url.clone(),
                assets: Vec::new(),
            };
            release.offered(settings)
                && settings.session.update_dismissed.as_deref() != Some(cache.latest.as_str())
        })
        .map(|cache| cache.latest.clone());
    *AVAILABLE.write().unwrap() = version;
}

/// Put the chip away for this release: remember the version so it stays
/// dismissed across restarts, and clear the live static. A newer release
/// brings the chip back on its own.
pub fn dismiss(version: String) {
    Settings::update(move |s| s.session.update_dismissed = Some(version));
    *AVAILABLE.write().unwrap() = None;
}

/// A published release as the check reads it: the version its tag names,
/// the page a user opens to get it, and the files attached to it for the
/// updater to resolve against.
#[derive(Clone)]
pub struct Release {
    /// The tag's version, the leading v stripped: "1.2.0".
    pub version: String,
    /// The release page on GitHub, where the artifacts are published.
    pub url: String,
    /// The release's files. Empty on a release rebuilt from the settings
    /// cache, which stores none; the updater refetches when it needs them.
    pub assets: Vec<Asset>,
}

/// One file attached to a release.
#[derive(Clone)]
pub struct Asset {
    pub name: String,
    /// The direct download URL.
    pub url: String,
    pub bytes: u64,
}

impl Release {
    /// Whether this release is newer than the running build. A tag that
    /// somehow doesn't parse reads as not newer, so a bad cache never
    /// prompts an update.
    pub fn is_new(&self) -> bool {
        is_newer(&self.version, CURRENT).unwrap_or(false)
    }

    /// Whether the version carries a prerelease suffix: a release
    /// candidate, as the workflow tags them.
    pub fn is_prerelease(&self) -> bool {
        is_prerelease(&self.version)
    }

    /// Whether this release is one to announce: newer than the running
    /// build, and not a candidate unless the settings want those. The
    /// cache can hold a candidate from a check made with the toggle on, so
    /// the chip and the About page ask this rather than [`Self::is_new`]
    /// and the toggle takes effect without waiting for the next check.
    pub fn offered(&self, settings: &Settings) -> bool {
        self.is_new() && (!self.is_prerelease() || wants_prereleases(settings))
    }
}

/// Whether the check should consider release candidates: the settings
/// toggle, or the running build being one itself. A candidate build that
/// ignored candidates would sit on `rc.1` while `rc.2` fixed its bugs.
pub fn wants_prereleases(settings: &Settings) -> bool {
    settings.prerelease_updates || is_prerelease(CURRENT)
}

/// One release as GitHub lists it, the fields the check reads.
#[derive(Deserialize)]
struct Api {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Ask GitHub for the newest release the settings allow: the highest
/// version among the published ones, candidates included only when
/// [`wants_prereleases`] says so. Err is the network or the API failing,
/// or nothing published that parses as a version, so callers never cache
/// a junk tag. Background executor only, it blocks.
pub fn fetch_latest() -> Result<Release, String> {
    // The shared agent already sets the app User-Agent the API requires;
    // the Accept header pins the versioned media type GitHub documents.
    let text = agent()
        .get(RELEASES)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let listed: Vec<Api> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let (version, api) = pick(listed, wants_prereleases(&Settings::load()))
        .ok_or_else(|| "no published release carries a version tag".to_string())?;
    Ok(Release {
        version: version.to_string(),
        url: api.html_url,
        assets: api
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                bytes: a.size,
            })
            .collect(),
    })
}

/// The release to offer out of a listing: drafts are unpublished, a tag
/// that isn't a version (or is one GitHub or the tag itself calls a
/// prerelease, when those aren't wanted) is skipped, and the highest
/// version wins. GitHub's flag and the tag's suffix both count as
/// prerelease, so a release flagged by hand and a candidate the workflow
/// tagged read the same way.
fn pick(listed: Vec<Api>, include_prereleases: bool) -> Option<(Version, Api)> {
    listed
        .into_iter()
        .filter(|api| !api.draft)
        .filter_map(|api| {
            let version = Version::parse(api.tag_name.trim_start_matches('v')).ok()?;
            let prerelease = api.prerelease || !version.pre.is_empty();
            (include_prereleases || !prerelease).then_some((version, api))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
}

/// Run the daily check at launch if it's due, off the UI thread, caching
/// the result in settings. The toggle and the one-day spacing both gate
/// it, so a normal start usually does nothing. A failed fetch leaves the
/// old cache and its timestamp alone, so the next launch just retries.
///
/// With the download toggle opted in, a check that finds a newer release
/// rolls straight into the updater on the same background task, but only
/// where the install can update itself. A distro package or a read-only
/// home stays notify-only whatever the toggle says.
pub fn check_on_launch(cx: &mut gpui::App) {
    let settings = Settings::load();
    // Seed the menubar chip from the cache whether or not a check is due,
    // so a launch inside the one-day window still announces what the last
    // check found.
    refresh_available(&settings);
    if !auto_check_due(&settings) {
        return;
    }
    let auto_download = settings.download_updates;
    let check = cx.background_executor().spawn(async move {
        match fetch_latest() {
            Ok(release) => {
                Settings::update(|s| s.session.update_cache = Some(cache(&release)));
                refresh_available(&Settings::load());
                if auto_download
                    && release.is_new()
                    && updater::can_update()
                    && let Some(job) = updater::begin(&release)
                {
                    job();
                }
            }
            Err(e) => log::warn!("update check: {e}"),
        }
    });
    // Back on the foreground once the check settles: repaint the open
    // windows, since the chip's static is outside gpui's reactivity and
    // nothing else would wake an idle menubar.
    cx.spawn(async move |cx| {
        check.await;
        cx.refresh().ok();
    })
    .detach();
}

/// The cache entry a finished check writes: the release stamped with now.
pub fn cache(release: &Release) -> UpdateCache {
    UpdateCache {
        checked_at: now(),
        latest: release.version.clone(),
        url: release.url.clone(),
    }
}

/// Whether a launch should run the check: the toggle is on and either
/// nothing has been checked or the last check is over a day old.
fn auto_check_due(settings: &Settings) -> bool {
    settings.check_updates
        && settings
            .session
            .update_cache
            .as_ref()
            .is_none_or(|c| now().saturating_sub(c.checked_at) >= CHECK_INTERVAL)
}

/// Now as unix seconds, the cache's clock. Zero if the system clock is set
/// before the epoch, which just makes the next check read as due.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether `latest` is a higher version than `current`, semver ordering
/// with the prerelease rule: `1.25.0-rc.1` is above `1.24.9` and below
/// `1.25.0`. None when either doesn't parse, so a tag like "nightly" reads
/// as unparseable rather than sorting as zero.
fn is_newer(latest: &str, current: &str) -> Option<bool> {
    Some(Version::parse(latest).ok()? > Version::parse(current).ok()?)
}

/// Whether a version carries a prerelease suffix. Unparseable reads as
/// not a prerelease; it won't be offered anyway.
fn is_prerelease(version: &str) -> bool {
    Version::parse(version).is_ok_and(|v| !v.pre.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_versions() {
        assert_eq!(is_newer("1.2.0", "1.1.9"), Some(true));
        assert_eq!(is_newer("1.1.10", "1.1.9"), Some(true));
        assert_eq!(is_newer("1.1.2", "1.1.2"), Some(false));
        assert_eq!(is_newer("1.0.0", "1.1.0"), Some(false));
        assert_eq!(is_newer("nightly", "1.1.2"), None);
    }

    /// The prerelease rule, which is the whole reason candidates can tag
    /// ahead of the release they preview.
    #[test]
    fn candidates_sort_below_their_release_and_above_the_last_one() {
        assert_eq!(is_newer("1.25.0-rc.1", "1.24.9"), Some(true));
        assert_eq!(is_newer("1.25.0-rc.1", "1.25.0"), Some(false));
        assert_eq!(is_newer("1.25.0", "1.25.0-rc.1"), Some(true));
        assert_eq!(is_newer("1.25.0-rc.2", "1.25.0-rc.1"), Some(true));
        assert_eq!(is_newer("1.25.0-rc.10", "1.25.0-rc.9"), Some(true));
        assert!(is_prerelease("1.25.0-rc.1"));
        assert!(!is_prerelease("1.25.0"));
        assert!(!is_prerelease("nightly"));
    }

    fn listed(tag: &str, draft: bool, prerelease: bool) -> Api {
        Api {
            tag_name: tag.to_string(),
            html_url: format!("https://github.com/zealsprince/rox/releases/tag/{tag}"),
            draft,
            prerelease,
            assets: Vec::new(),
        }
    }

    /// What the listing hands back under each toggle: the candidate only
    /// when asked for, the stable release otherwise, never a draft, and
    /// the highest version rather than whatever GitHub lists first.
    #[test]
    fn picks_by_version_and_toggle() {
        let releases = || {
            vec![
                listed("v1.25.0-rc.1", false, true),
                listed("v1.24.0", false, false),
                listed("v1.26.0", true, false),
                listed("v1.23.6", false, false),
                listed("nightly", false, false),
            ]
        };
        let (stable, _) = pick(releases(), false).unwrap();
        assert_eq!(stable.to_string(), "1.24.0");
        let (candidate, api) = pick(releases(), true).unwrap();
        assert_eq!(candidate.to_string(), "1.25.0-rc.1");
        assert!(api.html_url.ends_with("v1.25.0-rc.1"));
        // A release flagged prerelease by hand hides with the candidates
        // even when its tag looks stable.
        let flagged = vec![
            listed("v1.24.1", false, true),
            listed("v1.24.0", false, false),
        ];
        assert_eq!(pick(flagged, false).unwrap().0.to_string(), "1.24.0");
        assert!(pick(vec![listed("nightly", false, false)], true).is_none());
    }

    /// The listing as GitHub sends it, trimmed to the fields the check
    /// reads: a real answer from the API on 2026-09-05, so a renamed field
    /// fails here and not on a user's machine.
    #[test]
    fn parses_the_listing_as_github_sends_it() {
        let text = r#"[
          {
            "tag_name": "v1.24.0",
            "html_url": "https://github.com/zealsprince/rox/releases/tag/v1.24.0",
            "draft": false,
            "prerelease": false,
            "assets": [
              {
                "name": "rox-v1.24.0-linux-x86_64.tar.gz",
                "browser_download_url": "https://github.com/zealsprince/rox/releases/download/v1.24.0/rox-v1.24.0-linux-x86_64.tar.gz",
                "size": 46561633
              },
              {
                "name": "SHA256SUMS.txt",
                "browser_download_url": "https://github.com/zealsprince/rox/releases/download/v1.24.0/SHA256SUMS.txt",
                "size": 481
              }
            ]
          },
          {
            "tag_name": "v1.23.6",
            "html_url": "https://github.com/zealsprince/rox/releases/tag/v1.23.6",
            "draft": false,
            "prerelease": false,
            "assets": []
          }
        ]"#;
        let listed: Vec<Api> = serde_json::from_str(text).unwrap();
        let (version, api) = pick(listed, false).unwrap();
        assert_eq!(version.to_string(), "1.24.0");
        assert_eq!(api.assets.len(), 2);
        assert_eq!(api.assets[0].name, "rox-v1.24.0-linux-x86_64.tar.gz");
        assert_eq!(api.assets[0].size, 46561633);
    }
}
