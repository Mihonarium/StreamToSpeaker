//! At-rest protection for secrets inside `config.json`.
//!
//! Secret strings (AirPlay passwords, pairing seeds) are written as
//! `dpapi:v1:<base64>` — the UTF-8 value sealed with Windows DPAPI in the
//! current user's scope, so another account on the machine, or a copy of
//! the file on another machine, can't read them. In memory the config
//! keeps plain values; sealing happens on the JSON tree at save time and
//! unsealing at load time, so no other code sees the protected form.
//!
//! Rules:
//! - **Migration**: a plain value at a secret path is accepted as is and
//!   sealed by the next save ([`UnsealReport::plaintext`] tells the loader
//!   to save straight away).
//! - **Never lose a credential**: a value that can't be unsealed (profile
//!   moved to another machine, DPAPI master key unavailable) is logged and
//!   its whole unit — the map entry at the path's last `*`, or the
//!   top-level key when there's no `*` — is taken out of the tree before
//!   the config is parsed, then written back byte-for-byte on every save
//!   unless something new has been stored in that slot meanwhile.
//! - **Non-Windows** builds (tests, Linux checks) have no protector: values
//!   stay plain on save, and protected values can't be opened (they are
//!   kept, as above).
//!
//! Secret locations are listed in [`SECRET_PATHS`]; a config section that
//! gains key material adds its path there.

use base64::Engine as _;
use serde_json::Value;

/// Marker + format version of a sealed value.
pub const PROTECTED_PREFIX: &str = "dpapi:v1:";

/// Largest sealed value we try to open (base64 characters). Real ones are
/// a few hundred bytes; anything far bigger is not ours.
const MAX_SEALED_LEN: usize = 16 * 1024;

/// JSON paths holding secrets. Segments are object keys; `*` matches every
/// key of an object.
pub const SECRET_PATHS: &[&[&str]] = &[
    &["airplay_controller_seed_hex"],
    &["airplay_passwords", "*"],
    &["airplay_pairings", "*", "controller_seed_hex"],
];

/// The platform's at-rest protector.
pub trait SecretCodec {
    /// Whether [`protect`](Self::protect) seals anything on this platform.
    fn can_protect(&self) -> bool;
    /// Seal `plain`. `None` = no protection available (value stays plain).
    fn protect(&self, plain: &[u8]) -> Option<Result<Vec<u8>, String>>;
    /// Open a sealed blob.
    fn unprotect(&self, sealed: &[u8]) -> Result<Vec<u8>, String>;
}

/// DPAPI (current-user scope) on Windows; no protection elsewhere.
pub struct PlatformCodec;

impl SecretCodec for PlatformCodec {
    fn can_protect(&self) -> bool {
        cfg!(windows)
    }

    fn protect(&self, plain: &[u8]) -> Option<Result<Vec<u8>, String>> {
        #[cfg(windows)]
        {
            Some(dpapi::protect(plain))
        }
        #[cfg(not(windows))]
        {
            let _ = plain;
            None
        }
    }

    fn unprotect(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        #[cfg(windows)]
        {
            dpapi::unprotect(sealed)
        }
        #[cfg(not(windows))]
        {
            let _ = sealed;
            Err("protected values can only be opened on Windows".into())
        }
    }
}

/// A unit of the config that held an unreadable sealed value, kept aside
/// verbatim so the next save writes it back.
#[derive(Clone, Debug, PartialEq)]
pub struct LockedNode {
    /// Concrete key path of the removed node.
    pub path: Vec<String>,
    pub value: Value,
}

/// What [`unseal`] found.
#[derive(Debug, Default)]
pub struct UnsealReport {
    /// Units removed because a value in them couldn't be opened.
    pub locked: Vec<LockedNode>,
    /// Secret values found unsealed (pre-protection files).
    pub plaintext: usize,
}

/// Every concrete path matching `pattern` in `root` whose value is a
/// string, together with the length of the path's "unit" prefix.
fn string_paths(root: &Value, pattern: &[&str]) -> Vec<(Vec<String>, usize)> {
    let unit_len = pattern
        .iter()
        .rposition(|s| *s == "*")
        .map(|i| i + 1)
        .unwrap_or(1);
    let mut out = Vec::new();
    let mut stack: Vec<(Vec<String>, &Value)> = vec![(Vec::new(), root)];
    while let Some((path, node)) = stack.pop() {
        if path.len() == pattern.len() {
            if node.is_string() {
                out.push((path, unit_len));
            }
            continue;
        }
        let Some(obj) = node.as_object() else { continue };
        let seg = pattern[path.len()];
        if seg == "*" {
            for (k, v) in obj {
                let mut p = path.clone();
                p.push(k.clone());
                stack.push((p, v));
            }
        } else if let Some(v) = obj.get(seg) {
            let mut p = path.clone();
            p.push(seg.to_string());
            stack.push((p, v));
        }
    }
    out
}

fn get_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |node, key| node.as_object_mut()?.get_mut(key))
}

fn remove(root: &mut Value, path: &[String]) -> Option<Value> {
    let (last, parent) = path.split_last()?;
    get_mut(root, parent)?.as_object_mut()?.remove(last)
}

fn open_sealed(s: &str, codec: &dyn SecretCodec) -> Result<String, String> {
    let b64 = &s[PROTECTED_PREFIX.len()..];
    if b64.len() > MAX_SEALED_LEN {
        return Err(format!("sealed value too large ({} chars)", b64.len()));
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("bad base64: {e}"))?;
    let plain = codec.unprotect(&blob)?;
    String::from_utf8(plain).map_err(|_| "not UTF-8 after unsealing".to_string())
}

/// Open every sealed secret in `root` in place. Units holding a value that
/// can't be opened are removed and returned in the report.
pub fn unseal(root: &mut Value, codec: &dyn SecretCodec) -> UnsealReport {
    let mut report = UnsealReport::default();
    for pattern in SECRET_PATHS {
        for (path, unit_len) in string_paths(root, pattern) {
            let unit = &path[..unit_len];
            if report.locked.iter().any(|l| l.path == unit) {
                continue;
            }
            let Some(Value::String(s)) = get_mut(root, &path) else { continue };
            if !s.starts_with(PROTECTED_PREFIX) {
                report.plaintext += 1;
                continue;
            }
            match open_sealed(s, codec) {
                Ok(plain) => *s = plain,
                Err(e) => {
                    log::warn!(
                        "user_config: can't open protected value at {} ({e}); keeping it \
                         stored but unused",
                        unit.join(".")
                    );
                    if let Some(value) = remove(root, unit) {
                        report.locked.push(LockedNode { path: unit.to_vec(), value });
                    }
                }
            }
        }
    }
    report
}

/// Seal every plain secret in `root` in place, then put back `locked`
/// units whose slot is still empty. A value the codec fails to seal is
/// left plain (logged) rather than dropped.
pub fn seal(root: &mut Value, locked: &[LockedNode], codec: &dyn SecretCodec) {
    for pattern in SECRET_PATHS {
        for (path, _) in string_paths(root, pattern) {
            let Some(Value::String(s)) = get_mut(root, &path) else { continue };
            if s.starts_with(PROTECTED_PREFIX) {
                continue;
            }
            match codec.protect(s.as_bytes()) {
                None => {}
                Some(Ok(blob)) => {
                    *s = format!(
                        "{PROTECTED_PREFIX}{}",
                        base64::engine::general_purpose::STANDARD.encode(blob)
                    );
                }
                Some(Err(e)) => log::warn!(
                    "user_config: can't protect value at {} ({e}); saving it unprotected",
                    path.join(".")
                ),
            }
        }
    }
    for node in locked {
        let Some((last, parents)) = node.path.split_last() else { continue };
        let Some(obj) = object_at(root, parents) else { continue };
        // Something stored since (a re-pair, a new password) wins.
        if matches!(obj.get(last), None | Some(Value::Null)) {
            obj.insert(last.clone(), node.value.clone());
        }
    }
}

/// The object at `path`, creating empty objects for missing keys. `None`
/// when something on the way isn't an object.
fn object_at<'a>(
    root: &'a mut Value,
    path: &[String],
) -> Option<&'a mut serde_json::Map<String, Value>> {
    let mut cur = root;
    for key in path {
        cur = cur
            .as_object_mut()?
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    cur.as_object_mut()
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Fixed secondary entropy: another program running as the same user
    /// can't open our blobs with a bare CryptUnprotectData call.
    const ENTROPY: &[u8] = b"StreamToSpeaker config secret v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copy out and free a DPAPI-allocated output blob.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        v
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(plain);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: input/entropy point at live slices for the call; `out`
        // is allocated by DPAPI and released by `take`.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!("CryptProtectData: {}", std::io::Error::last_os_error()));
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(sealed);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as in `protect`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!("CryptUnprotectData: {}", std::io::Error::last_os_error()));
        }
        Ok(unsafe { take(out) })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Test protector: XOR with a key byte behind a 2-byte tag. A blob
    /// whose tag doesn't match fails to open, like a DPAPI blob from
    /// another user or machine.
    pub(crate) struct FakeCodec(pub u8);

    impl SecretCodec for FakeCodec {
        fn can_protect(&self) -> bool {
            true
        }
        fn protect(&self, plain: &[u8]) -> Option<Result<Vec<u8>, String>> {
            let mut v = vec![0xD9, self.0];
            v.extend(plain.iter().map(|b| b ^ self.0));
            Some(Ok(v))
        }
        fn unprotect(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
            match sealed {
                [0xD9, k, rest @ ..] if *k == self.0 => {
                    Ok(rest.iter().map(|b| b ^ self.0).collect())
                }
                _ => Err("wrong key".into()),
            }
        }
    }

    fn sample() -> Value {
        json!({
            "last_speaker_id": "airplay:aa",
            "airplay_controller_id": "ctl-1",
            "airplay_controller_seed_hex": "11".repeat(32),
            "airplay_passwords": { "airplay:aa": "hunter2", "airplay:bb": "pw-b" },
            "airplay_pairings": {
                "airplay:cc": {
                    "controller_id": "ctl-1",
                    "controller_seed_hex": "22".repeat(32),
                    "accessory_id": "acc",
                    "accessory_ltpk_hex": "33".repeat(32)
                }
            }
        })
    }

    #[test]
    fn seal_then_unseal_round_trips_and_hides_only_secrets() {
        let codec = FakeCodec(0x5A);
        let mut v = sample();
        seal(&mut v, &[], &codec);
        let text = v.to_string();
        assert!(!text.contains("hunter2") && !text.contains(&"11".repeat(32)));
        assert!(!text.contains(&"22".repeat(32)));
        // Public parts stay readable.
        assert!(text.contains(&"33".repeat(32)) && text.contains("ctl-1"));
        assert!(v["airplay_passwords"]["airplay:bb"]
            .as_str()
            .unwrap()
            .starts_with(PROTECTED_PREFIX));
        let r = unseal(&mut v, &codec);
        assert!(r.locked.is_empty());
        assert_eq!(r.plaintext, 0);
        assert_eq!(v, sample());
    }

    #[test]
    fn plaintext_values_are_counted_for_migration() {
        let mut v = sample();
        let r = unseal(&mut v, &FakeCodec(1));
        assert_eq!(r.plaintext, 4);
        assert_eq!(v, sample(), "plain values pass through untouched");
    }

    #[test]
    fn unopenable_values_are_set_aside_and_written_back() {
        let mut v = sample();
        seal(&mut v, &[], &FakeCodec(7));
        let sealed = v.clone();
        // Another user / machine: nothing opens.
        let r = unseal(&mut v, &FakeCodec(8));
        assert_eq!(r.locked.len(), 4);
        // Whole pairing entry removed so the struct still parses.
        assert!(v["airplay_pairings"].as_object().unwrap().is_empty());
        assert!(v.get("airplay_controller_seed_hex").is_none());
        assert_eq!(v["airplay_controller_id"], "ctl-1");
        // Saving puts every original sealed byte back.
        seal(&mut v, &r.locked, &FakeCodec(8));
        assert_eq!(v, sealed);
    }

    #[test]
    fn a_new_value_in_a_locked_slot_wins() {
        let mut v = sample();
        seal(&mut v, &[], &FakeCodec(7));
        let r = unseal(&mut v, &FakeCodec(8));
        v["airplay_passwords"]["airplay:aa"] = json!("new-pw");
        seal(&mut v, &r.locked, &FakeCodec(8));
        let back = {
            let mut c = v.clone();
            unseal(&mut c, &FakeCodec(8));
            c
        };
        assert_eq!(back["airplay_passwords"]["airplay:aa"], "new-pw");
        // The untouched locked one is still there, still sealed.
        assert!(v["airplay_passwords"]["airplay:bb"]
            .as_str()
            .unwrap()
            .starts_with(PROTECTED_PREFIX));
    }

    #[test]
    fn malformed_sealed_values_never_panic() {
        for bad in ["dpapi:v1:", "dpapi:v1:!!!", "dpapi:v1:AAAA", "dpapi:v1:2Q=="] {
            let mut v = json!({ "airplay_passwords": { "x": bad }, "airplay_controller_seed_hex": bad });
            let r = unseal(&mut v, &FakeCodec(3));
            assert_eq!(r.locked.len(), 2, "{bad}");
        }
        let huge = format!("{PROTECTED_PREFIX}{}", "A".repeat(MAX_SEALED_LEN + 4));
        let mut v = json!({ "airplay_controller_seed_hex": huge });
        assert_eq!(unseal(&mut v, &FakeCodec(3)).locked.len(), 1);
    }

    #[test]
    fn platform_codec_off_windows_leaves_values_plain() {
        if cfg!(windows) {
            return;
        }
        let mut v = sample();
        seal(&mut v, &[], &PlatformCodec);
        assert_eq!(v, sample());
    }
}
