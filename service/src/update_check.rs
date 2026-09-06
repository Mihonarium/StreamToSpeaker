//! Once-a-day "is there a newer release?" check against GitHub.
//!
//! The boring, well-trodden shape for a desktop app:
//!
//! - **Source of truth:** the GitHub Releases API,
//!   `GET /repos/<owner>/<repo>/releases/latest`. It returns the newest
//!   *published, non-prerelease* release, so drafts and the `driver-v*`
//!   prerelease entries are excluded server-side (and rejected again here,
//!   defensively).
//! - **Version compared:** [`crate::release_version`] — the git tag CI
//!   baked into this exe via `STS_RELEASE_VERSION` — never
//!   `CARGO_PKG_VERSION`. The crate version and the release tag are
//!   independent (crate 0.6.0 vs tag v0.1.4 when this was written), so
//!   comparing the crate version would report "up to date" forever. A
//!   build without a baked tag is a dev build and never checks.
//! - **Transport:** WinHTTP, the OS HTTPS stack — system certificate
//!   store, system proxy settings and TLS policy apply, and we ship no
//!   TLS library of our own. There is deliberately no non-Windows fetch:
//!   the product is Windows-only, and everything *except* the fetch is
//!   unit-tested cross-platform.
//! - **Cadence:** one check ~20 s after launch (never on the startup
//!   path), then at most once per 24 h, persisted across launches. One
//!   unauthenticated request a day is nothing against GitHub's 60/h
//!   per-IP limit, so there is no ETag / conditional-request machinery.
//! - **Privacy:** a plain GET carrying only a `User-Agent` with the app
//!   version; GitHub sees the IP address as for any web request. Off
//!   switch in Advanced (`check_for_updates`).
//! - **No auto-download, no auto-install.** The app only *tells* the user
//!   and links to the release page; the installer (which also installs a
//!   driver) is run by the user. Executing something we fetched ourselves
//!   would need signature verification we don't have.

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// GitHub `owner/repo`.
pub const REPO: &str = "Mihonarium/StreamToSpeaker";
/// API host for [`API_PATH`].
pub const API_HOST: &str = "api.github.com";
/// The "latest release" endpoint (published, non-prerelease only).
pub const API_PATH: &str = "/repos/Mihonarium/StreamToSpeaker/releases/latest";
/// Human landing page, used when a release carries no `html_url`.
pub const RELEASES_PAGE: &str = "https://github.com/Mihonarium/StreamToSpeaker/releases/latest";
/// The stable-named installer asset CI attaches to every release.
pub const INSTALLER_ASSET: &str = "StreamToSpeakerSetup.exe";
/// Minimum spacing between automatic checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Wait after launch before the first automatic check.
pub const STARTUP_DELAY: Duration = Duration::from_secs(20);
/// Refuse to buffer more than this from the API (the real body is ~10 KB).
#[cfg(windows)]
const MAX_BODY_BYTES: usize = 1 << 20;

/// What the API told us about the newest release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Git tag, e.g. `v0.1.4`.
    pub tag: String,
    /// The tag parsed as a version.
    pub version: semver::Version,
    /// Release page for humans (notes, checksums, all assets).
    pub url: String,
    /// Direct link to [`INSTALLER_ASSET`] if the release has one.
    pub installer_url: Option<String>,
}

/// `v0.1.4` / `V0.1.4` / `0.1.4` → `0.1.4`. Rejects anything that isn't a
/// version once the `v` is gone (e.g. `driver-v1.1.0.204`).
pub fn parse_tag_version(tag: &str) -> Result<semver::Version> {
    let stripped = tag.trim().trim_start_matches(['v', 'V']);
    semver::Version::parse(stripped).with_context(|| format!("release tag {tag:?} is not a version"))
}

/// The version this exe *is*, for comparison — `None` for dev builds.
pub fn current_version() -> Option<semver::Version> {
    crate::release_version().and_then(|v| semver::Version::parse(v).ok())
}

/// Strict semver ordering, so `0.1.5-rc.1` is *not* newer than `0.1.5`.
pub fn is_newer(latest: &semver::Version, current: &semver::Version) -> bool {
    latest > current
}

/// The newer release the banner should offer, if any — pure so it can be
/// tested: `latest_tag` is the cached newest tag, `skipped_tag` the one
/// the user dismissed for good, `hidden_until` the "Later" snooze.
pub fn banner_candidate(
    current: &semver::Version,
    latest_tag: &str,
    skipped_tag: Option<&str>,
    hidden_until: Option<u64>,
    now_unix: u64,
) -> Option<semver::Version> {
    let latest = parse_tag_version(latest_tag).ok()?;
    if !is_newer(&latest, current) {
        return None;
    }
    if skipped_tag == Some(latest_tag) {
        return None;
    }
    if matches!(hidden_until, Some(t) if now_unix < t) {
        return None;
    }
    Some(latest)
}

/// Parse the `releases/latest` JSON body.
pub fn parse_latest_release(json: &str) -> Result<ReleaseInfo> {
    let v: serde_json::Value = serde_json::from_str(json).context("release JSON")?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        bail!("latest release is a draft or prerelease");
    }
    let tag = v["tag_name"].as_str().context("release JSON has no tag_name")?;
    let version = parse_tag_version(tag)?;
    let url = v["html_url"]
        .as_str()
        .filter(|u| u.starts_with("https://github.com/"))
        .unwrap_or(RELEASES_PAGE)
        .to_string();
    let installer_url = v["assets"].as_array().and_then(|assets| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(INSTALLER_ASSET))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_string)
    });
    Ok(ReleaseInfo { tag: tag.to_string(), version, url, installer_url })
}

/// `stream-to-speaker/<version> (+repo url)` — GitHub requires a UA, and
/// a descriptive one is the polite convention.
pub fn user_agent() -> String {
    format!("stream-to-speaker/{} (+https://github.com/{})", crate::display_version(), REPO)
}

/// One synchronous round-trip to GitHub. Blocks for up to the WinHTTP
/// timeouts (~10-15 s); call it off the UI thread.
pub fn fetch_latest_release() -> Result<ReleaseInfo> {
    let (status, body) = http_get(API_HOST, API_PATH, &user_agent())?;
    match status {
        200 => parse_latest_release(&body),
        404 => bail!("no release has been published yet"),
        403 | 429 => bail!("GitHub rate limit reached{}", api_message(&body)),
        s => bail!("GitHub API returned HTTP {s}{}", api_message(&body)),
    }
}

/// GitHub error bodies carry a human `message`; surface it if present.
fn api_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(|m| format!(": {m}")))
        .unwrap_or_default()
}

/// HTTPS GET → (status, body). Windows-only by design (see module docs).
#[cfg(windows)]
fn http_get(host: &str, path: &str, user_agent: &str) -> Result<(u16, String)> {
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn last_error(what: &str) -> anyhow::Error {
        anyhow::anyhow!("{what}: {}", windows::core::Error::from_win32())
    }
    /// Closes the WinHTTP handle on drop, whichever way we leave.
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle we opened, closed exactly once here.
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    // SAFETY: plain WinHTTP FFI. Every handle is owned by a `Handle` guard,
    // every buffer outlives the call that reads it, and the wide strings
    // are NUL-terminated by `wide`.
    unsafe {
        let ua = wide(user_agent);
        let session = Handle(WinHttpOpen(
            PCWSTR(ua.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        // resolve, connect, send, receive — milliseconds.
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 15_000).context("WinHttpSetTimeouts")?;

        let host_w = wide(host);
        let conn = Handle(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), 443, 0));
        if conn.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }

        let verb = wide("GET");
        let object = wide(path);
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(object.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }

        let headers = wide("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n");
        // Length excludes the terminator (the binding passes the slice length).
        WinHttpAddRequestHeaders(req.0, &headers[..headers.len() - 1], WINHTTP_ADDREQ_FLAG_ADD)
            .context("WinHttpAddRequestHeaders")?;
        WinHttpSendRequest(req.0, None, None, 0, 0, 0).context("WinHttpSendRequest")?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).context("WinHttpReceiveResponse")?;

        let mut status: u32 = 0;
        let mut len: u32 = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        )
        .context("WinHttpQueryHeaders(status)")?;

        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut avail: u32 = 0;
            WinHttpQueryDataAvailable(req.0, &mut avail).context("WinHttpQueryDataAvailable")?;
            if avail == 0 {
                break;
            }
            if body.len() + avail as usize > MAX_BODY_BYTES {
                bail!("response larger than {} bytes", MAX_BODY_BYTES);
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            WinHttpReadData(req.0, chunk.as_mut_ptr() as *mut c_void, avail, &mut read)
                .context("WinHttpReadData")?;
            if read == 0 {
                break;
            }
            chunk.truncate(read as usize);
            body.extend_from_slice(&chunk);
        }
        Ok((status as u16, String::from_utf8_lossy(&body).into_owned()))
    }
}

#[cfg(not(windows))]
fn http_get(_host: &str, _path: &str, _user_agent: &str) -> Result<(u16, String)> {
    bail!("update checks are only implemented on Windows (WinHTTP)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn tag_parsing_strips_v_and_rejects_non_versions() {
        assert_eq!(parse_tag_version("v0.1.4").unwrap(), v("0.1.4"));
        assert_eq!(parse_tag_version("V1.2.3").unwrap(), v("1.2.3"));
        assert_eq!(parse_tag_version("1.2.3-rc.1").unwrap(), v("1.2.3-rc.1"));
        // The driver prerelease tags must never be mistaken for the app.
        assert!(parse_tag_version("driver-v1.1.0.204").is_err());
        assert!(parse_tag_version("").is_err());
    }

    #[test]
    fn newer_is_strict_semver() {
        assert!(is_newer(&v("0.1.5"), &v("0.1.4")));
        assert!(is_newer(&v("1.0.0"), &v("0.9.9")));
        assert!(!is_newer(&v("0.1.4"), &v("0.1.4")));
        assert!(!is_newer(&v("0.1.3"), &v("0.1.4")));
        // A prerelease of the next version is not "newer" than it.
        assert!(!is_newer(&v("0.1.5-rc.1"), &v("0.1.5")));
        assert!(is_newer(&v("0.1.5-rc.1"), &v("0.1.4")));
    }

    #[test]
    fn parses_the_real_api_shape() {
        // Trimmed from a live `releases/latest` response.
        let json = r#"{
          "html_url": "https://github.com/Mihonarium/StreamToSpeaker/releases/tag/v0.1.4",
          "tag_name": "v0.1.4", "name": "v0.1.4", "draft": false, "prerelease": false,
          "assets": [
            {"name": "stream-to-speaker-0.1.4.exe", "browser_download_url": "https://x/a"},
            {"name": "StreamToSpeakerSetup.exe",
             "browser_download_url": "https://github.com/Mihonarium/StreamToSpeaker/releases/download/v0.1.4/StreamToSpeakerSetup.exe"}
          ]
        }"#;
        let r = parse_latest_release(json).unwrap();
        assert_eq!(r.tag, "v0.1.4");
        assert_eq!(r.version, v("0.1.4"));
        assert_eq!(r.url, "https://github.com/Mihonarium/StreamToSpeaker/releases/tag/v0.1.4");
        assert_eq!(
            r.installer_url.as_deref(),
            Some("https://github.com/Mihonarium/StreamToSpeaker/releases/download/v0.1.4/StreamToSpeakerSetup.exe")
        );
    }

    #[test]
    fn rejects_prerelease_draft_and_garbage() {
        assert!(parse_latest_release(r#"{"tag_name":"v9.9.9","prerelease":true}"#).is_err());
        assert!(parse_latest_release(r#"{"tag_name":"v9.9.9","draft":true}"#).is_err());
        assert!(parse_latest_release(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_latest_release("not json").is_err());
        // No assets / no html_url still yields a usable release.
        let r = parse_latest_release(r#"{"tag_name":"v0.2.0"}"#).unwrap();
        assert_eq!(r.url, RELEASES_PAGE);
        assert_eq!(r.installer_url, None);
    }

    #[test]
    fn banner_respects_skip_and_snooze() {
        let cur = v("0.1.4");
        assert_eq!(banner_candidate(&cur, "v0.1.5", None, None, 1000), Some(v("0.1.5")));
        // Not newer → nothing.
        assert_eq!(banner_candidate(&cur, "v0.1.4", None, None, 1000), None);
        assert_eq!(banner_candidate(&cur, "v0.1.3", None, None, 1000), None);
        // Skipped exactly this tag → nothing; a later one shows again.
        assert_eq!(banner_candidate(&cur, "v0.1.5", Some("v0.1.5"), None, 1000), None);
        assert_eq!(banner_candidate(&cur, "v0.1.6", Some("v0.1.5"), None, 1000), Some(v("0.1.6")));
        // Snoozed until 2000: hidden at 1500, back at 2000.
        assert_eq!(banner_candidate(&cur, "v0.1.5", None, Some(2000), 1500), None);
        assert_eq!(banner_candidate(&cur, "v0.1.5", None, Some(2000), 2000), Some(v("0.1.5")));
        // Garbage cached tag → nothing, never a panic.
        assert_eq!(banner_candidate(&cur, "driver-v1.1.0.204", None, None, 1000), None);
    }

    #[test]
    fn dev_builds_have_no_current_version() {
        if crate::release_version().is_none() {
            assert!(current_version().is_none());
        }
    }

    #[test]
    fn api_message_is_optional() {
        assert_eq!(api_message(r#"{"message":"API rate limit exceeded"}"#), ": API rate limit exceeded");
        assert_eq!(api_message("<html>"), "");
    }

    /// Live round-trip through WinHTTP to GitHub. Ignored by default (needs
    /// Windows + network): `cargo test -- --ignored live_github_fetch`.
    #[test]
    #[ignore]
    fn live_github_fetch() {
        let r = fetch_latest_release().expect("fetch latest release");
        assert!(r.tag.starts_with('v'));
        assert!(r.url.starts_with("https://github.com/"));
    }
}
