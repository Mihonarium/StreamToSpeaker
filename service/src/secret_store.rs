//! At-rest protection for secrets inside `config.json`.
//!
//! Secrets (AirPlay passwords, pairing credentials, the controller seed)
//! never sit in their normal fields on disk. Each one is moved into a
//! top-level `protected` object, keyed by its JSON pointer (for example
//! `/airplay_passwords/airplay:aa`), as `dpapi:v1:<base64>`: the value's
//! JSON sealed with Windows DPAPI in the current user's scope, so another
//! account on the machine, or a copy of the file on another machine, can't
//! read it. In memory the config keeps plain values; sealing happens on
//! the JSON tree at save time and unsealing at load time, so no other code
//! sees the protected form.
//!
//! Rules:
//! - **Older versions** see the secret fields as absent (no pairing, no
//!   controller identity, no password) and keep `protected` untouched as
//!   an unknown key. They fall back to a fresh pairing instead of
//!   tripping over sealed bytes, and nothing stored is destroyed.
//! - **A plain value wins**: a secret found in its normal field was
//!   written by an older version (or before protection existed) and is
//!   newer than any sealed copy of it. It is kept, and sealed by the next
//!   save ([`UnsealReport::needs_save`] tells the loader to save at once).
//! - **Never lose a credential**: a sealed value that can't be opened
//!   (profile moved to another machine, DPAPI master key unavailable), or
//!   one at a path this version doesn't know, is logged and written back
//!   byte-for-byte on every save, unless a new value is stored in its slot
//!   or the slot is deliberately cleared ([`forget`]).
//! - **Non-Windows** builds (tests, Linux checks) have no protector: values
//!   stay in their normal fields, and sealed ones are kept as above.
//!
//! Secret locations are listed in [`SECRET_PATHS`]; a config section that
//! gains key material adds its path there.

use base64::Engine as _;
use serde_json::{Map, Value};

/// Marker + format version of a sealed value.
pub const PROTECTED_PREFIX: &str = "dpapi:v1:";

/// Top-level key holding every sealed value.
pub const PROTECTED_KEY: &str = "protected";

/// Largest sealed value we try to open (base64 characters). Real ones are
/// a few hundred bytes; anything far bigger is not ours.
const MAX_SEALED_LEN: usize = 16 * 1024;

/// JSON paths holding secrets. Segments are object keys; `*` matches every
/// key of an object. The sealed unit is the node at the path's last `*`
/// (a whole map entry, so a struct never loses a required field), or the
/// top-level key when there's no `*`.
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

/// A sealed entry kept verbatim because it couldn't be opened, or names a
/// path this version doesn't handle.
#[derive(Clone, Debug, PartialEq)]
pub struct LockedNode {
    /// JSON pointer it is stored under in `protected`; `None` for a
    /// `protected` value that isn't an object at all, kept whole.
    pub pointer: Option<String>,
    pub sealed: Value,
}

/// What [`unseal`] found.
#[derive(Debug, Default)]
pub struct UnsealReport {
    /// Sealed entries kept aside (see [`LockedNode`]).
    pub locked: Vec<LockedNode>,
    /// The file should be rewritten: it had secrets in their plain fields,
    /// or sealed copies those plain values supersede.
    pub needs_save: bool,
}

/// RFC 6901 pointer for a key path.
pub fn pointer(path: &[String]) -> String {
    path.iter()
        .map(|k| format!("/{}", k.replace('~', "~0").replace('/', "~1")))
        .collect()
}

fn parse_pointer(p: &str) -> Option<Vec<String>> {
    let rest = p.strip_prefix('/')?;
    Some(rest.split('/').map(|k| k.replace("~1", "/").replace("~0", "~")).collect())
}

fn unit_pattern<'a>(pattern: &'a [&'static str]) -> &'a [&'static str] {
    let len = pattern.iter().rposition(|s| *s == "*").map(|i| i + 1).unwrap_or(1);
    &pattern[..len]
}

/// Whether `path` is the unit of one of [`SECRET_PATHS`].
fn is_secret_unit(path: &[String]) -> bool {
    SECRET_PATHS.iter().any(|p| {
        let u = unit_pattern(p);
        u.len() == path.len() && u.iter().zip(path).all(|(s, k)| *s == "*" || s == k)
    })
}

/// Every concrete unit path in `root` holding a (non-null) secret.
fn present_units(root: &Value) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for pattern in SECRET_PATHS {
        let unit = unit_pattern(pattern);
        let mut stack: Vec<(Vec<String>, &Value)> = vec![(Vec::new(), root)];
        while let Some((path, node)) = stack.pop() {
            if path.len() == unit.len() {
                if !node.is_null() && !out.contains(&path) {
                    out.push(path);
                }
                continue;
            }
            let Some(obj) = node.as_object() else { continue };
            let seg = unit[path.len()];
            for (k, v) in obj {
                if seg == "*" || seg == k {
                    let mut p = path.clone();
                    p.push(k.clone());
                    stack.push((p, v));
                }
            }
        }
    }
    out
}

fn value_at<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(root, |node, key| node.as_object()?.get(key)).filter(|v| !v.is_null())
}

fn take_at(root: &mut Value, path: &[String]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut cur = root;
    for key in parents {
        cur = cur.as_object_mut()?.get_mut(key)?;
    }
    cur.as_object_mut()?.remove(last)
}

/// Insert at `path`, creating empty objects for missing parents. False when
/// something on the way isn't an object.
fn insert_at(root: &mut Value, path: &[String], value: Value) -> bool {
    let Some((last, parents)) = path.split_last() else { return false };
    let mut cur = root;
    for key in parents {
        let Some(obj) = cur.as_object_mut() else { return false };
        cur = obj.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    match cur.as_object_mut() {
        Some(obj) => {
            obj.insert(last.clone(), value);
            true
        }
        None => false,
    }
}

fn open_sealed(v: &Value, codec: &dyn SecretCodec) -> Result<Value, String> {
    let s = v.as_str().ok_or("not a string")?;
    let b64 = s.strip_prefix(PROTECTED_PREFIX).ok_or("unknown format")?;
    if b64.len() > MAX_SEALED_LEN {
        return Err(format!("sealed value too large ({} chars)", b64.len()));
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("bad base64: {e}"))?;
    let plain = codec.unprotect(&blob)?;
    serde_json::from_slice(&plain).map_err(|e| format!("not JSON after unsealing: {e}"))
}

/// Move every sealed secret from `protected` back into its normal field.
/// `protected` itself is removed from the tree.
pub fn unseal(root: &mut Value, codec: &dyn SecretCodec) -> UnsealReport {
    let mut report = UnsealReport {
        needs_save: !present_units(root).is_empty(),
        ..Default::default()
    };
    let Some(obj) = root.as_object_mut() else { return report };
    let sealed = match obj.remove(PROTECTED_KEY) {
        None => return report,
        Some(Value::Object(m)) => m,
        Some(other) => {
            log::warn!("user_config: `{PROTECTED_KEY}` is not an object; keeping it as is");
            report.locked.push(LockedNode { pointer: None, sealed: other });
            return report;
        }
    };
    for (ptr, v) in sealed {
        let path = match parse_pointer(&ptr) {
            Some(p) if is_secret_unit(&p) => p,
            _ => {
                report.locked.push(LockedNode { pointer: Some(ptr), sealed: v });
                continue;
            }
        };
        if value_at(root, &path).is_some() {
            // An older version stored this since; its plain value is newer.
            report.needs_save = true;
            continue;
        }
        match open_sealed(&v, codec) {
            Ok(plain) => {
                if !insert_at(root, &path, plain) {
                    log::warn!(
                        "user_config: no place for protected value {ptr}; keeping it stored"
                    );
                    report.locked.push(LockedNode { pointer: Some(ptr), sealed: v });
                }
            }
            Err(e) => {
                log::warn!(
                    "user_config: can't open protected value {ptr} ({e}); keeping it stored \
                     but unused"
                );
                report.locked.push(LockedNode { pointer: Some(ptr), sealed: v });
            }
        }
    }
    report
}

/// Move every secret in `root` into a sealed `protected` entry, and write
/// `locked` entries back unless their slot holds a new value. Without a
/// protector (or when sealing fails, logged) a value stays in its field.
pub fn seal(root: &mut Value, locked: &[LockedNode], codec: &dyn SecretCodec) {
    let mut out = Map::new();
    if codec.can_protect() {
        for path in present_units(root) {
            let Some(plain) = take_at(root, &path) else { continue };
            let sealed = serde_json::to_vec(&plain)
                .map_err(|e| e.to_string())
                .and_then(|bytes| codec.protect(&bytes).unwrap_or(Err("no protector".into())));
            match sealed {
                Ok(blob) => {
                    let text = format!(
                        "{PROTECTED_PREFIX}{}",
                        base64::engine::general_purpose::STANDARD.encode(blob)
                    );
                    out.insert(pointer(&path), Value::String(text));
                }
                Err(e) => {
                    log::warn!(
                        "user_config: can't protect {} ({e}); saving it unprotected",
                        pointer(&path)
                    );
                    insert_at(root, &path, plain);
                }
            }
        }
    }
    let mut passthrough = None;
    for node in locked {
        let Some(ptr) = &node.pointer else {
            passthrough = Some(node.sealed.clone());
            continue;
        };
        let superseded = out.contains_key(ptr)
            || parse_pointer(ptr).is_some_and(|p| value_at(root, &p).is_some());
        if !superseded {
            out.insert(ptr.clone(), node.sealed.clone());
        }
    }
    if let Some(obj) = root.as_object_mut() {
        if !out.is_empty() {
            obj.insert(PROTECTED_KEY.into(), Value::Object(out));
        } else if let Some(v) = passthrough {
            obj.insert(PROTECTED_KEY.into(), v);
        }
    }
}

/// Drop kept sealed entries for `path` (a deliberate removal, so a later
/// save must not bring the old value back).
pub fn forget(locked: &mut Vec<LockedNode>, path: &[String]) {
    let ptr = pointer(path);
    locked.retain(|n| n.pointer.as_deref() != Some(ptr.as_str()));
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
            "airplay_passwords": { "airplay:aa": "hunter2", "a/b~c": "pw-b" },
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
    fn seal_moves_secrets_out_of_their_fields_and_unseal_restores_them() {
        let codec = FakeCodec(0x5A);
        let mut v = sample();
        seal(&mut v, &[], &codec);
        let text = v.to_string();
        assert!(!text.contains("hunter2") && !text.contains(&"11".repeat(32)));
        assert!(!text.contains(&"22".repeat(32)));
        // Normal fields no longer hold anything secret.
        assert!(v.get("airplay_controller_seed_hex").is_none());
        assert!(v["airplay_passwords"].as_object().unwrap().is_empty());
        assert!(v["airplay_pairings"].as_object().unwrap().is_empty());
        assert_eq!(v["airplay_controller_id"], "ctl-1");
        let p = v[PROTECTED_KEY].as_object().unwrap();
        assert_eq!(p.len(), 4);
        assert!(p["/airplay_passwords/a~1b~0c"].as_str().unwrap().starts_with(PROTECTED_PREFIX));
        assert!(p.contains_key("/airplay_pairings/airplay:cc"));
        let r = unseal(&mut v, &codec);
        assert!(r.locked.is_empty());
        assert!(!r.needs_save);
        assert_eq!(v, sample());
    }

    #[test]
    fn plain_fields_ask_for_a_rewrite() {
        let mut v = sample();
        let r = unseal(&mut v, &FakeCodec(1));
        assert!(r.needs_save);
        assert_eq!(v, sample(), "plain values pass through untouched");
    }

    #[test]
    fn a_plain_value_written_by_an_older_version_wins() {
        let codec = FakeCodec(9);
        let mut v = sample();
        seal(&mut v, &[], &codec);
        // An older version sees no pairing for airplay:cc, re-pairs, and
        // writes it in the plain field next to the sealed copy.
        v["airplay_pairings"]["airplay:cc"] = json!({
            "controller_id": "ctl-2", "controller_seed_hex": "44".repeat(32),
            "accessory_id": "acc", "accessory_ltpk_hex": "55".repeat(32)
        });
        let r = unseal(&mut v, &codec);
        assert!(r.needs_save);
        assert_eq!(v["airplay_pairings"]["airplay:cc"]["controller_id"], "ctl-2");
        // Everything it didn't replace comes back from the sealed copies.
        assert_eq!(v["airplay_passwords"]["airplay:aa"], "hunter2");
        assert_eq!(v["airplay_controller_seed_hex"], "11".repeat(32));
    }

    #[test]
    fn unopenable_and_unknown_entries_are_written_back() {
        let mut v = sample();
        seal(&mut v, &[], &FakeCodec(7));
        v[PROTECTED_KEY]["/future_section/key"] = json!("dpapi:v1:AAAA");
        let sealed = v.clone();
        // Another user / machine: nothing opens.
        let r = unseal(&mut v, &FakeCodec(8));
        assert_eq!(r.locked.len(), 5);
        assert!(v["airplay_pairings"].as_object().unwrap().is_empty());
        assert!(v.get("airplay_controller_seed_hex").is_none());
        seal(&mut v, &r.locked, &FakeCodec(8));
        assert_eq!(v, sealed);
        // Same without any protector (non-Windows builds).
        let mut v2 = sealed.clone();
        let r2 = unseal(&mut v2, &PlatformCodecForTests);
        seal(&mut v2, &r2.locked, &PlatformCodecForTests);
        assert_eq!(v2, sealed);
    }

    /// No protector at all, like `PlatformCodec` off Windows.
    struct PlatformCodecForTests;
    impl SecretCodec for PlatformCodecForTests {
        fn can_protect(&self) -> bool {
            false
        }
        fn protect(&self, _: &[u8]) -> Option<Result<Vec<u8>, String>> {
            None
        }
        fn unprotect(&self, _: &[u8]) -> Result<Vec<u8>, String> {
            Err("none".into())
        }
    }

    #[test]
    fn new_values_and_deliberate_removals_beat_kept_entries() {
        let mut v = sample();
        seal(&mut v, &[], &FakeCodec(7));
        let mut v2 = v.clone();
        let mut r = unseal(&mut v2, &FakeCodec(8));
        // New password for aa; password for a/b~c deliberately cleared.
        v2["airplay_passwords"]["airplay:aa"] = json!("new-pw");
        forget(&mut r.locked, &["airplay_passwords".into(), "a/b~c".into()]);
        seal(&mut v2, &r.locked, &FakeCodec(8));
        let p = v2[PROTECTED_KEY].as_object().unwrap();
        assert!(!p.contains_key("/airplay_passwords/a~1b~0c"), "removal stays removed");
        assert_ne!(p["/airplay_passwords/airplay:aa"], v[PROTECTED_KEY]["/airplay_passwords/airplay:aa"]);
        let mut back = v2.clone();
        unseal(&mut back, &FakeCodec(8));
        assert_eq!(back["airplay_passwords"]["airplay:aa"], "new-pw");
        // The untouched kept ones are still there.
        assert!(p.contains_key("/airplay_pairings/airplay:cc"));
    }

    #[test]
    fn malformed_protected_sections_never_panic() {
        for bad in ["dpapi:v1:", "dpapi:v1:!!!", "dpapi:v1:AAAA", "x", "/"] {
            let mut v = json!({ PROTECTED_KEY: {
                "/airplay_passwords/x": bad, "": bad, "/": bad, "nope": 3,
                "/airplay_controller_seed_hex": bad } });
            let r = unseal(&mut v, &FakeCodec(3));
            assert_eq!(r.locked.len(), 5, "{bad}");
        }
        let huge = format!("{PROTECTED_PREFIX}{}", "A".repeat(MAX_SEALED_LEN + 4));
        let mut v = json!({ PROTECTED_KEY: { "/airplay_controller_seed_hex": huge } });
        assert_eq!(unseal(&mut v, &FakeCodec(3)).locked.len(), 1);
        let mut v = json!({ PROTECTED_KEY: [1, 2] });
        let r = unseal(&mut v, &FakeCodec(3));
        seal(&mut v, &r.locked, &FakeCodec(3));
        assert_eq!(v[PROTECTED_KEY], json!([1, 2]));
    }

    #[test]
    fn platform_codec_off_windows_leaves_values_in_place() {
        if cfg!(windows) {
            return;
        }
        let mut v = sample();
        seal(&mut v, &[], &PlatformCodec);
        assert_eq!(v, sample());
    }
}
