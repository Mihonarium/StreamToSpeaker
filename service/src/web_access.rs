//! Who may use the web page and the JSON API (`--web`, or "Enable web UI").
//!
//! Both need one secret: 128 random bits from the operating system,
//! written as 32 hex digits. The page is served only under `/r/<secret>/`
//! (its script too), so the link the app shows is the whole credential for
//! a browser, and the page's script reads the secret from its own address.
//! Every `/api/*` request must carry `Authorization: Bearer <secret>`.
//! Requests are checked against it in constant time.
//!
//! Not behind it: `/stream.raw` and GENA NOTIFY, which speakers call and
//! can't add a key to. The stream gate and the GENA subscription checks
//! govern those.
//!
//! The secret is made the first time the web UI is turned on (or
//! `--web-link` asks for the link), kept in config.json (sealed like the
//! other stored secrets) and replaced when the user makes a new link: once
//! the new one is saved, the old one stops working at once. It never goes
//! into the log, a diagnostics report or an error message; request paths
//! are logged through [`redact_target`].

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use rand::RngCore;
use subtle::ConstantTimeEq;

use crate::user_config::UserConfig;

/// The page's paths start with this, then the secret: `/r/<secret>/`.
pub const LINK_PREFIX: &str = "/r/";

/// 128 bits.
const SECRET_BYTES: usize = 16;

/// The web UI's secret. `Debug` doesn't show it, and there is no `==`: a
/// request's key is checked with [`WebSecret::matches`].
#[derive(Clone)]
pub struct WebSecret([u8; SECRET_BYTES]);

impl WebSecret {
    /// A new secret from the operating system's random source.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; SECRET_BYTES];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|e| format!("the system's random number source failed ({e})"))?;
        Ok(Self(bytes))
    }

    /// From its text form: exactly 32 hex digits, in either case.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.as_bytes();
        if text.len() != SECRET_BYTES * 2 {
            return None;
        }
        let mut bytes = [0u8; SECRET_BYTES];
        for (b, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
            *b = hex_value(pair[0])? << 4 | hex_value(pair[1])?;
        }
        Some(Self(bytes))
    }

    /// The text form: 32 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Does `candidate`, text from a request, name this secret? The
    /// comparison takes the same time whatever the secret holds.
    pub fn matches(&self, candidate: &str) -> bool {
        match Self::parse(candidate) {
            Some(c) => bool::from(self.0[..].ct_eq(&c.0[..])),
            None => false,
        }
    }
}

impl std::fmt::Debug for WebSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebSecret(..)")
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Whether the web page and the API are on, and the secret they need.
/// Shared by the app, which turns them on and makes new secrets, and the
/// HTTP threads, which check each request against it without touching
/// the app's locks.
pub struct WebAccess {
    enabled: AtomicBool,
    secret: RwLock<Option<WebSecret>>,
}

impl WebAccess {
    /// Off, with no secret yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { enabled: AtomicBool::new(false), secret: RwLock::new(None) })
    }

    /// On, with `secret`.
    pub fn with_secret(secret: WebSecret) -> Arc<Self> {
        let access = Self::new();
        access.set_secret(secret);
        access.set_enabled(true);
        access
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Returns whether the value changed.
    pub fn set_enabled(&self, on: bool) -> bool {
        self.enabled.swap(on, Ordering::AcqRel) != on
    }

    /// Replace the secret: links and keys made with the old one stop
    /// working with the next request.
    pub fn set_secret(&self, secret: WebSecret) {
        *self.secret.write().unwrap() = Some(secret);
    }

    /// The secret's text form, if one is set.
    pub fn secret_hex(&self) -> Option<String> {
        self.secret.read().unwrap().as_ref().map(WebSecret::to_hex)
    }

    /// Is `key` (the path segment after [`LINK_PREFIX`]) the secret?
    pub fn path_key_ok(&self, key: &str) -> bool {
        self.secret.read().unwrap().as_ref().is_some_and(|s| s.matches(key))
    }

    /// Does this `Authorization` header value carry the secret as a bearer
    /// token?
    pub fn bearer_ok(&self, authorization: Option<&str>) -> bool {
        authorization.and_then(bearer_token).is_some_and(|token| self.path_key_ok(token))
    }
}

/// The token of an `Authorization: Bearer <token>` value (the scheme is
/// case-insensitive).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once([' ', '\t'])?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// The web UI's link: `http://<host>:<port>/r/<secret>/`, with an IPv6
/// address in brackets.
pub fn link(host: &str, port: u16, secret_hex: &str) -> String {
    let host = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
        _ => host.to_string(),
    };
    format!("http://{host}:{port}{LINK_PREFIX}{secret_hex}/")
}

/// The web UI's secret as the settings keep it: the one `config` holds;
/// else the one `stored` reads from config.json, which `--web-link` may
/// have stored while a running app held none; else a new one, written into
/// `config` and handed to `save`. Text that isn't a secret (edited by hand,
/// say) counts as none. The app turning its web UI on and `--web-link` both
/// take their key from here, and while the app's settings hold none, its
/// saves keep the one config.json holds (see
/// `UserConfig::try_save_to_with`). So the link `--web-link` prints is the
/// one the app accepts while its web UI is on, except when the app holds a
/// key config.json doesn't (one it couldn't save) or both save at the same
/// moment.
pub fn stored_or_new_secret(
    config: &mut UserConfig,
    stored: impl FnOnce(&UserConfig) -> Option<String>,
    save: impl FnOnce(&UserConfig),
) -> Result<WebSecret, String> {
    if let Some(secret) = config.web_secret.as_deref().and_then(WebSecret::parse) {
        return Ok(secret);
    }
    if let Some(text) = stored(config) {
        if let Some(secret) = WebSecret::parse(&text) {
            config.web_secret = Some(text);
            return Ok(secret);
        }
    }
    if config.web_secret.is_some() {
        log::warn!("web UI: the stored link key isn't 32 hex digits; making a new one");
    }
    let secret = WebSecret::generate()?;
    config.web_secret = Some(secret.to_hex());
    save(config);
    Ok(secret)
}

/// What `--web-link` prints: the link with the key from the settings
/// `load` reads, made and saved with `save` if there is none yet. Headless
/// and service installs have no window to show their link in, and what
/// they print as they start may go nowhere. Fails when a new key didn't
/// reach the settings (a config.json this account can't write, say): the
/// app would refuse that link.
pub fn link_from_settings(
    host: &str,
    port: u16,
    load: impl Fn() -> UserConfig,
    save: impl FnOnce(&UserConfig),
) -> Result<String, String> {
    // Just loaded: config.json holds nothing newer for `stored` to read.
    let key = stored_or_new_secret(&mut load(), |_| None, save)?.to_hex();
    let kept = load().web_secret.as_deref().and_then(WebSecret::parse);
    if !kept.is_some_and(|secret| secret.matches(&key)) {
        return Err("couldn't save the web UI's key in the settings (config.json)".into());
    }
    Ok(link(host, port, &key))
}

/// `target` for a log line: the segment after every `/r/` is replaced, so
/// no key — right or wrong — is written down.
pub fn redact_target(target: &str) -> Cow<'_, str> {
    if !target.contains(LINK_PREFIX) {
        return Cow::Borrowed(target);
    }
    let mut out = String::with_capacity(target.len());
    let mut rest = target;
    while let Some(at) = rest.find(LINK_PREFIX) {
        let after = &rest[at + LINK_PREFIX.len()..];
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        out.push_str(&rest[..at + LINK_PREFIX.len()]);
        out.push_str("<key>");
        rest = &after[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn secrets_are_32_hex_digits_and_round_trip() {
        let s = WebSecret::parse(HEX).unwrap();
        assert_eq!(s.to_hex(), HEX);
        assert_eq!(WebSecret::parse(&HEX.to_ascii_uppercase()).unwrap().to_hex(), HEX);
        for bad in ["", "0011", &HEX[1..], &format!("{HEX}0"), &HEX.replace('f', "g"), "zz112233445566778899aabbccddeeff"] {
            assert!(WebSecret::parse(bad).is_none(), "{bad:?}");
        }
        // Multibyte text of the right byte length is not hex either.
        assert!(WebSecret::parse(&"é".repeat(16)).is_none());
        let a = WebSecret::generate().unwrap();
        let b = WebSecret::generate().unwrap();
        assert_eq!(a.to_hex().len(), 32);
        assert!(WebSecret::parse(&a.to_hex()).is_some());
        assert_ne!(a.to_hex(), b.to_hex());
        assert_eq!(format!("{a:?}"), "WebSecret(..)");
    }

    #[test]
    fn matches_only_the_same_secret() {
        let s = WebSecret::parse(HEX).unwrap();
        assert!(s.matches(HEX));
        assert!(s.matches(&HEX.to_ascii_uppercase()));
        assert!(!s.matches("00112233445566778899aabbccddeef0"));
        assert!(!s.matches(&HEX[..31]));
        assert!(!s.matches(""));
    }

    #[test]
    fn access_checks_the_path_key_and_the_bearer_token() {
        let access = WebAccess::new();
        assert!(!access.is_enabled());
        // No secret yet: nothing matches.
        assert!(!access.path_key_ok(HEX));
        assert!(!access.bearer_ok(Some(&format!("Bearer {HEX}"))));
        access.set_secret(WebSecret::parse(HEX).unwrap());
        assert!(access.path_key_ok(HEX));
        for ok in [format!("Bearer {HEX}"), format!("bearer {HEX}"), format!("  BEARER \t{HEX}  ")] {
            assert!(access.bearer_ok(Some(&ok)), "{ok:?}");
        }
        for bad in [
            String::new(),
            HEX.to_string(),
            format!("Bearer{HEX}"),
            format!("Basic {HEX}"),
            format!("Bearer {HEX}x"),
            "Bearer ".to_string(),
        ] {
            assert!(!access.bearer_ok(Some(&bad)), "{bad:?}");
        }
        assert!(!access.bearer_ok(None));
        // A new secret: the old key stops working at once.
        let new = WebSecret::generate().unwrap();
        let new_hex = new.to_hex();
        access.set_secret(new);
        assert!(!access.path_key_ok(HEX));
        assert!(access.path_key_ok(&new_hex));
        assert_eq!(access.secret_hex().as_deref(), Some(new_hex.as_str()));
        assert!(access.set_enabled(true) && !access.set_enabled(true) && access.is_enabled());
    }

    #[test]
    fn links_bracket_ipv6_hosts() {
        assert_eq!(link("192.168.1.10", 5901, HEX), format!("http://192.168.1.10:5901/r/{HEX}/"));
        assert_eq!(link("fe80::1", 5901, HEX), format!("http://[fe80::1]:5901/r/{HEX}/"));
        assert_eq!(link("pc.local", 8080, HEX), format!("http://pc.local:8080/r/{HEX}/"));
        assert!(crate::is_openable_url(&link("fe80::1", 5901, HEX)));
        assert!(crate::is_openable_url(&link("192.168.1.10", 5901, HEX)));
    }

    #[test]
    fn a_missing_secret_is_made_and_saved_and_a_stored_one_kept() {
        let mut config = UserConfig::default();
        let mut saves = Vec::new();
        let made =
            stored_or_new_secret(&mut config, |_| None, |c| saves.push(c.web_secret.clone())).unwrap();
        assert_eq!(saves, vec![Some(made.to_hex())]);
        assert_eq!(config.web_secret.as_deref(), Some(made.to_hex().as_str()));
        // Held: the same secret, config.json not read, and nothing saved.
        let unread = |_: &UserConfig| -> Option<String> { panic!("read config.json for a held key") };
        let again = stored_or_new_secret(&mut config, unread, |_| panic!("saved a held secret")).unwrap();
        assert_eq!(again.to_hex(), made.to_hex());
        // Edited by hand into capitals: still that secret, left as written.
        config.web_secret = Some(HEX.to_ascii_uppercase());
        let upper = stored_or_new_secret(&mut config, unread, |_| panic!("saved a usable secret")).unwrap();
        assert_eq!(upper.to_hex(), HEX);
        assert_eq!(config.web_secret.as_deref(), Some(HEX.to_ascii_uppercase().as_str()));
    }

    #[test]
    fn an_unusable_stored_secret_is_replaced_and_saved() {
        for bad in ["", "short", "zz112233445566778899aabbccddeeff", &format!("{HEX}00")] {
            let mut config = UserConfig::default();
            config.web_secret = Some(bad.to_string());
            let mut saved = None;
            let made = stored_or_new_secret(&mut config, |_| None, |c| saved = c.web_secret.clone()).unwrap();
            assert_eq!(saved.as_deref(), Some(made.to_hex().as_str()), "{bad:?}");
            assert_eq!(config.web_secret.as_deref(), Some(made.to_hex().as_str()), "{bad:?}");
        }
    }

    #[test]
    fn a_key_config_json_holds_is_taken_before_one_is_made() {
        // Settings holding none, or text that isn't a key: config.json's key,
        // kept as written there, and nothing saved (the file holds it).
        for held in [None, Some("short".to_string())] {
            let mut config = UserConfig::default();
            config.web_secret = held.clone();
            let stored = |_: &UserConfig| Some(HEX.to_ascii_uppercase());
            let taken = stored_or_new_secret(&mut config, stored, |_| panic!("saved")).unwrap();
            assert_eq!(taken.to_hex(), HEX, "{held:?}");
            assert_eq!(config.web_secret.as_deref(), Some(HEX.to_ascii_uppercase().as_str()));
        }
        // config.json holding text that isn't a key: a new one, saved.
        let mut config = UserConfig::default();
        let mut saved = None;
        let made =
            stored_or_new_secret(&mut config, |_| Some("zz".into()), |c| saved = c.web_secret.clone()).unwrap();
        assert_eq!(saved.as_deref(), Some(made.to_hex().as_str()));
    }

    fn temp_settings(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sts-web-access-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.json")
    }

    #[test]
    fn the_printed_link_is_the_one_the_settings_keep() {
        use crate::secret_store::tests::FakeCodec;
        let path = temp_settings("link");
        let load = || UserConfig::load_from_with(&path, &FakeCodec(7));
        let save = |c: &UserConfig| c.save_to_with(&path, &FakeCodec(7));
        // The first time: a key is made, sealed into the file, and linked.
        let first = link_from_settings("192.168.1.10", 5901, load, save).unwrap();
        let key = load().web_secret.expect("saved");
        assert_eq!(first, format!("http://192.168.1.10:5901/r/{key}/"));
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(&key), "{disk}");
        // Later: the same link, nothing saved; and the app turning its web UI
        // on with these settings takes the same key.
        let again = link_from_settings("192.168.1.10", 5901, load, |_| panic!("saved again")).unwrap();
        assert_eq!(again, first);
        let app_secret = stored_or_new_secret(&mut load(), |_| None, |_| panic!("the app saved")).unwrap();
        assert_eq!(app_secret.to_hex(), key);
        // A replaced key (Make a new link) is what the next link carries.
        let mut c = load();
        c.web_secret = Some(HEX.into());
        save(&c);
        assert_eq!(
            link_from_settings("fe80::1", 8080, load, |_| panic!()).unwrap(),
            format!("http://[fe80::1]:8080/r/{HEX}/")
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_running_app_takes_the_key_web_link_stored_meanwhile() {
        use crate::secret_store::tests::FakeCodec;
        let path = temp_settings("running");
        let codec = FakeCodec(7);
        let load = || UserConfig::load_from_with(&path, &codec);
        let save = |c: &UserConfig| c.save_to_with(&path, &codec);
        // The app's settings, loaded before there was a key: its web UI is
        // off and has never been on.
        save(&UserConfig::default());
        let mut app = load();
        // `--web-link` meanwhile: it stores a key and prints the link.
        let printed = link_from_settings("192.168.1.10", 5901, load, save).unwrap();
        // The app saves another setting, then its web UI is turned on.
        app.onboarding_dismissed = true;
        save(&app);
        let stored = |_: &UserConfig| UserConfig::stored_web_secret_at(&path, &codec);
        let secret = stored_or_new_secret(&mut app, stored, |_| panic!("made a key of its own")).unwrap();
        assert_eq!(link("192.168.1.10", 5901, &secret.to_hex()), printed);
        // config.json still holds that key, and the app's setting.
        let disk = load();
        assert_eq!(disk.web_secret.as_deref(), Some(secret.to_hex().as_str()));
        assert!(disk.onboarding_dismissed);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_key_the_settings_did_not_keep_gives_no_link() {
        // Settings this run can't write: every load starts from scratch, so
        // the key just made would be a link the app never accepts.
        let mut saved = 0;
        let err = link_from_settings("192.168.1.10", 5901, UserConfig::default, |_| saved += 1)
            .unwrap_err();
        assert_eq!(saved, 1);
        assert!(err.contains("config.json") && !err.contains(LINK_PREFIX), "{err}");
    }

    #[test]
    fn redaction_hides_every_key_and_keeps_the_rest() {
        assert_eq!(redact_target("/api/speakers"), "/api/speakers");
        assert_eq!(redact_target(&format!("/r/{HEX}/")), "/r/<key>/");
        assert_eq!(redact_target(&format!("/r/{HEX}")), "/r/<key>");
        assert_eq!(redact_target(&format!("/r/{HEX}/app.js?x=1")), "/r/<key>/app.js?x=1");
        assert_eq!(redact_target(&format!("/r/{HEX}?a")), "/r/<key>?a");
        assert_eq!(redact_target(&format!("/./r/{HEX}/x/r/{HEX}")), "/./r/<key>/x/r/<key>");
        assert_eq!(redact_target("/r/"), "/r/<key>");
        for t in [format!("/r/{HEX}/"), format!("//r/{HEX}"), format!("/a/r/{HEX}#f")] {
            assert!(!redact_target(&t).contains(HEX), "{t}");
        }
    }
}
