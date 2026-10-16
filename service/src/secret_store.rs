//! At-rest protection for secrets inside `config.json`.
//!
//! Secrets (AirPlay passwords, pairing credentials, the controller seed;
//! Sendspin identities and pairings; the web UI's key) never sit in their
//! normal fields on disk. Each one is moved into a
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
//!
//! **Groups.** An identity key and the pairings made with it only work
//! together (a peer holds the pairing for that identity and no other), so
//! they are sealed as one entry, listed in [`SECRET_GROUPS`]. The entry is
//! filed under a name no plain field uses: a version that knows the fields
//! but not the group keeps the entry untouched, even after it mints an
//! identity of its own in the plain fields. Groups follow the rules above,
//! with one refinement: **an identity nothing is paired with gives way**.
//! A group with pairings in the plain fields wins over the sealed one (it
//! was paired again by an older version); plain fields with an identity
//! but no pairings are what an older version mints when it can't see the
//! sealed group, so the sealed group wins over them; and a sealed group
//! that can't be opened is kept until a group with pairings is stored in
//! its place (an identity minted meanwhile is kept under
//! `<name>_pending`, used only while the stored group stays shut). A
//! group replaced by one with pairings is gone, as a replaced AirPlay
//! value is. Fields in a sealed group that this version doesn't list
//! (added by a later one) are sealed again with the group on every save.

use base64::Engine as _;
use serde_json::{Map, Value};

/// Marker + format version of a sealed value.
pub const PROTECTED_PREFIX: &str = "dpapi:v1:";

/// Top-level key holding every sealed value.
pub const PROTECTED_KEY: &str = "protected";

/// JSON paths holding secrets. Segments are object keys; `*` matches every
/// key of an object. The sealed unit is the node at the path's last `*`
/// (a whole map entry, so a struct never loses a required field), or the
/// top-level key when there's no `*`.
pub const SECRET_PATHS: &[&[&str]] = &[
    &["airplay_controller_seed_hex"],
    &["airplay_passwords", "*"],
    &["airplay_pairings", "*", "controller_seed_hex"],
    // The web UI's link key, also the JSON API's bearer token.
    &["web_secret"],
];

/// Fields of one object sealed together as a single entry (see the
/// module docs).
pub struct SecretGroup {
    /// Path of the object holding the fields.
    pub parent: &'static [&'static str],
    /// Name the sealed entry is filed under, below `parent`. It must never
    /// be a field of that object.
    pub name: &'static str,
    /// The group's fields: the identity and everything made with it.
    pub fields: &'static [&'static str],
    /// The fields holding pairings made with the group's identity.
    pub pairings: &'static [&'static str],
}

/// Suffix of the name an identity this account minted is kept under while
/// the stored group can't be opened (see [`seal_with`]).
pub const PENDING_SUFFIX: &str = "_pending";

pub const SECRET_GROUPS: &[SecretGroup] = &[
    // Sendspin source client: our client key, the pairing PSK handed out
    // in pairing tokens, and the long-term PSKs of the servers paired
    // with that key.
    SecretGroup {
        parent: &["sendspin"],
        name: "client_identity",
        fields: &["client_key", "pairing_psk", "server_pairings"],
        pairings: &["server_pairings"],
    },
    // Sendspin server: our server key and the players paired with it.
    SecretGroup {
        parent: &["sendspin"],
        name: "server_identity",
        fields: &["server_key", "player_pairings"],
        pairings: &["player_pairings"],
    },
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

fn group_path(g: &SecretGroup) -> Vec<String> {
    g.parent.iter().chain([&g.name]).map(|s| s.to_string()).collect()
}

fn object_at_mut<'a>(root: &'a mut Value, path: &[&str]) -> Option<&'a mut Map<String, Value>> {
    path.iter()
        .try_fold(root, |node, key| node.as_object_mut()?.get_mut(*key))?
        .as_object_mut()
}

/// [`object_at_mut`], creating missing objects on the way (not replacing
/// anything that isn't one).
fn object_entry<'a>(root: &'a mut Value, path: &[&str]) -> Option<&'a mut Map<String, Value>> {
    path.iter()
        .try_fold(root, |node, key| {
            Some(node.as_object_mut()?.entry(*key).or_insert_with(|| Value::Object(Map::new())))
        })?
        .as_object_mut()
}

/// Null, `[]` and `{}`: what a field holds when nothing is stored in it.
fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// The group's non-blank fields in `obj`.
fn group_fields(g: &SecretGroup, obj: &Map<String, Value>) -> Map<String, Value> {
    g.fields
        .iter()
        .filter_map(|f| obj.get(*f).filter(|v| !is_blank(v)).map(|v| (f.to_string(), v.clone())))
        .collect()
}

fn has_pairings(g: &SecretGroup, fields: &Map<String, Value>) -> bool {
    g.pairings.iter().any(|f| fields.contains_key(*f))
}

fn open_group(v: &Value, codec: &dyn SecretCodec) -> Result<Map<String, Value>, String> {
    match open_sealed(v, codec)? {
        Value::Object(fields) => Ok(fields),
        _ => Err("not an object".into()),
    }
}

fn seal_value(plain: &Value, codec: &dyn SecretCodec) -> Result<Value, String> {
    let bytes = serde_json::to_vec(plain).map_err(|e| e.to_string())?;
    let blob = codec.protect(&bytes).unwrap_or(Err("no protector".into()))?;
    Ok(Value::String(format!(
        "{PROTECTED_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    )))
}

fn open_sealed(v: &Value, codec: &dyn SecretCodec) -> Result<Value, String> {
    let s = v.as_str().ok_or("not a string")?;
    // No size limit: whatever `seal_value` wrote must open again (a group
    // grows with every pairing), and the file is already in memory whole.
    let b64 = s.strip_prefix(PROTECTED_PREFIX).ok_or("unknown format")?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("bad base64: {e}"))?;
    let plain = codec.unprotect(&blob)?;
    serde_json::from_slice(&plain).map_err(|e| format!("not JSON after unsealing: {e}"))
}

/// Move every sealed secret from `protected` back into its normal field.
/// `protected` itself is removed from the tree.
pub fn unseal(root: &mut Value, codec: &dyn SecretCodec) -> UnsealReport {
    unseal_with(root, codec, SECRET_GROUPS)
}

/// [`unseal`] with an explicit group table (an empty one is what a version
/// without groups does).
pub(crate) fn unseal_with(
    root: &mut Value,
    codec: &dyn SecretCodec,
    groups: &[SecretGroup],
) -> UnsealReport {
    let plain_groups: Vec<Map<String, Value>> = groups
        .iter()
        .map(|g| {
            let parent: Vec<String> = g.parent.iter().map(|s| s.to_string()).collect();
            value_at(root, &parent)
                .and_then(Value::as_object)
                .map(|obj| group_fields(g, obj))
                .unwrap_or_default()
        })
        .collect();
    let mut report = UnsealReport {
        needs_save: !present_units(root).is_empty() || plain_groups.iter().any(|f| !f.is_empty()),
        ..Default::default()
    };
    let Some(obj) = root.as_object_mut() else { return report };
    let mut sealed = match obj.remove(PROTECTED_KEY) {
        None => return report,
        Some(Value::Object(m)) => m,
        Some(other) => {
            log::warn!("user_config: `{PROTECTED_KEY}` is not an object; keeping it as is");
            report.locked.push(LockedNode { pointer: None, sealed: other });
            return report;
        }
    };
    for (g, plain) in groups.iter().zip(&plain_groups) {
        let ptr = pointer(&group_path(g));
        let pending_ptr = format!("{ptr}{PENDING_SUFFIX}");
        let stored = sealed.remove(&ptr);
        let pending = sealed.remove(&pending_ptr);
        if has_pairings(g, plain) {
            // Paired again by an older version: its group is newer and
            // replaces the stored one (and a pending identity, which was
            // never paired). The next save writes it in their place.
            continue;
        }
        // The stored group, or, while it can't be opened, the identity this
        // account minted meanwhile (see `seal_with`). Once the stored group
        // opens, that one gives way.
        let mut restore = None;
        for (slot, v) in [(ptr.clone(), stored), (pending_ptr, pending)] {
            let Some(v) = v else { continue };
            if restore.is_some() {
                continue;
            }
            match open_group(&v, codec) {
                Ok(fields) => restore = Some((slot, v, fields)),
                Err(e) => {
                    log::warn!(
                        "user_config: can't open protected value {slot} ({e}); keeping it \
                         stored but unused"
                    );
                    report.locked.push(LockedNode { pointer: Some(slot), sealed: v });
                }
            }
        }
        let Some((slot, v, fields)) = restore else { continue };
        let Some(parent) = object_entry(root, g.parent) else {
            log::warn!("user_config: no place for protected value {slot}; keeping it stored");
            report.locked.push(LockedNode { pointer: Some(slot), sealed: v });
            continue;
        };
        // Whatever the plain fields hold is an identity nothing is paired
        // with (minted by a version that couldn't see this group).
        for f in g.fields {
            parent.remove(*f);
            if let Some(val) = fields.get(*f) {
                parent.insert(f.to_string(), val.clone());
            }
        }
        if slot == ptr && fields.keys().any(|k| !g.fields.contains(&k.as_str())) {
            // Fields a later version added: the stored group is kept as
            // well, so saves can seal them again with the current fields.
            report.locked.push(LockedNode { pointer: Some(slot), sealed: v });
        }
    }
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
    seal_with(root, locked, codec, SECRET_GROUPS)
}

/// [`seal`] with an explicit group table (see [`unseal_with`]).
pub(crate) fn seal_with(
    root: &mut Value,
    locked: &[LockedNode],
    codec: &dyn SecretCodec,
    groups: &[SecretGroup],
) {
    let mut out = Map::new();
    // Kept groups whose fields were sealed again here.
    let mut carried: Vec<String> = Vec::new();
    if codec.can_protect() {
        for g in groups {
            let ptr = pointer(&group_path(g));
            let Some(parent) = object_at_mut(root, g.parent) else { continue };
            let fields = group_fields(g, parent);
            for f in g.fields {
                parent.remove(*f);
            }
            let kept = locked.iter().find(|n| n.pointer.as_ref() == Some(&ptr));
            // A kept group that opens holds fields a later version added
            // (see `unseal_with`); they go along with the current ones.
            let later = kept.and_then(|n| open_group(&n.sealed, codec).ok()).map(|mut m| {
                m.retain(|k, _| !g.fields.contains(&k.as_str()));
                m
            });
            if fields.is_empty() {
                if later.is_some() {
                    // The identity was cleared; what was added to it goes too.
                    carried.push(ptr);
                }
                continue;
            }
            let slot = if !has_pairings(g, &fields) && kept.is_some() && later.is_none() {
                // An identity nothing is paired with yet doesn't replace a
                // stored group this account can't open. It is kept beside
                // it, so it stays the same across launches (a pairing token
                // may be handed out meanwhile) until it is paired.
                format!("{ptr}{PENDING_SUFFIX}")
            } else {
                ptr
            };
            let mut sealed_fields = fields.clone();
            if let Some(later) = &later {
                for (k, v) in later {
                    sealed_fields.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            match seal_value(&Value::Object(sealed_fields), codec) {
                Ok(text) => {
                    if later.is_some() {
                        carried.push(slot.clone());
                    }
                    out.insert(slot, text);
                }
                Err(e) => {
                    log::warn!("user_config: can't protect {slot} ({e}); saving it unprotected");
                    parent.extend(fields);
                }
            }
        }
        for path in present_units(root) {
            let Some(plain) = take_at(root, &path) else { continue };
            match seal_value(&plain, codec) {
                Ok(text) => {
                    out.insert(pointer(&path), text);
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
        if carried.contains(ptr) {
            continue;
        }
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

/// The secret in top-level field `key` of `root`, a config file's tree as
/// stored, by [`unseal`]'s rules: its plain value wins, else its sealed
/// entry if that opens. Nothing else in `root` is opened, and nothing is
/// logged.
pub fn stored_value(root: &Value, key: &str, codec: &dyn SecretCodec) -> Option<Value> {
    if let Some(plain) = root.get(key).filter(|v| !v.is_null()) {
        return Some(plain.clone());
    }
    let sealed = root.get(PROTECTED_KEY)?.get(pointer(&[key.to_string()]))?;
    open_sealed(sealed, codec).ok()
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
        let huge = format!("{PROTECTED_PREFIX}{}", "A".repeat(4 << 20));
        let mut v = json!({ PROTECTED_KEY: { "/airplay_controller_seed_hex": huge } });
        assert_eq!(unseal(&mut v, &FakeCodec(3)).locked.len(), 1);
        let mut v = json!({ PROTECTED_KEY: [1, 2] });
        let r = unseal(&mut v, &FakeCodec(3));
        seal(&mut v, &r.locked, &FakeCodec(3));
        assert_eq!(v[PROTECTED_KEY], json!([1, 2]));
    }

    #[test]
    fn stored_value_reads_one_secret_as_loading_would() {
        let codec = FakeCodec(4);
        let mut v = sample();
        v["web_secret"] = json!("k1");
        seal(&mut v, &[], &codec);
        assert!(v.get("web_secret").is_none());
        assert_eq!(stored_value(&v, "web_secret", &codec), Some(json!("k1")));
        assert_eq!(stored_value(&v, "airplay_controller_seed_hex", &codec), Some(json!("11".repeat(32))));
        // Another account's blob doesn't open.
        assert_eq!(stored_value(&v, "web_secret", &FakeCodec(5)), None);
        // A plain value wins over the sealed one; a null one is no value.
        let mut both = v.clone();
        both["web_secret"] = json!("k2");
        assert_eq!(stored_value(&both, "web_secret", &codec), Some(json!("k2")));
        both["web_secret"] = Value::Null;
        assert_eq!(stored_value(&both, "web_secret", &codec), Some(json!("k1")));
        // Nothing there, and shapes no save writes.
        for root in [json!({}), json!({ PROTECTED_KEY: [1] }), json!({ PROTECTED_KEY: { "/web_secret": 3 } }), json!([1])] {
            assert_eq!(stored_value(&root, "web_secret", &codec), None, "{root}");
        }
    }

    #[test]
    fn platform_codec_off_windows_leaves_values_in_place() {
        if cfg!(windows) {
            return;
        }
        let mut v = sample();
        seal(&mut v, &[], &PlatformCodec);
        assert_eq!(v, sample());
        let mut v = sendspin_sample();
        seal(&mut v, &[], &PlatformCodec);
        assert_eq!(v, sendspin_sample());
    }

    const CLIENT_GROUP: &str = "/sendspin/client_identity";
    const SERVER_GROUP: &str = "/sendspin/server_identity";
    const SENDSPIN_SECRETS: &[&str] = &["ck-1", "pp-1", "lt-s1", "sk-1", "lt-p1"];

    fn sendspin_sample() -> Value {
        json!({
            "last_speaker_id": "x",
            "sendspin": {
                "enabled": true,
                "source_enabled": true,
                "last_playback_server": "srv-1",
                "client_key": "ck-1",
                "pairing_psk": "pp-1",
                "server_pairings": [
                    { "server_id": "srv-1", "psk": "lt-s1", "server_name": "MA", "last_used_unix": 5 }
                ],
                "server_key": "sk-1",
                "player_pairings": {
                    "pl-1": { "psk": "lt-p1", "name": "Kitchen", "instance": "kitchen" }
                }
            }
        })
    }

    /// An identity minted by a version that can't see the sealed groups,
    /// with nothing paired to it (blank fields as such a version writes them).
    fn mint_unpaired(v: &mut Value, n: u8) {
        let s = &mut v["sendspin"];
        s["client_key"] = json!(format!("ck-{n}"));
        s["pairing_psk"] = json!(format!("pp-{n}"));
        s["server_pairings"] = json!([]);
        s["server_key"] = json!(format!("sk-{n}"));
        s["player_pairings"] = json!({});
    }

    #[test]
    fn sendspin_identities_seal_as_one_entry_each() {
        let codec = FakeCodec(0x33);
        let mut v = sendspin_sample();
        let r = unseal(&mut v, &codec);
        assert!(r.needs_save, "plain Sendspin secrets ask for a rewrite");
        seal(&mut v, &[], &codec);
        let text = v.to_string();
        for s in SENDSPIN_SECRETS {
            assert!(!text.contains(s), "{s} in {text}");
        }
        // Only the settings stay in the section; every secret field is out.
        let s = v["sendspin"].as_object().unwrap();
        let mut keys: Vec<&str> = s.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["enabled", "last_playback_server", "source_enabled"]);
        let p = v[PROTECTED_KEY].as_object().unwrap();
        assert_eq!(p.len(), 2);
        assert!(p[CLIENT_GROUP].as_str().unwrap().starts_with(PROTECTED_PREFIX));
        assert!(p[SERVER_GROUP].as_str().unwrap().starts_with(PROTECTED_PREFIX));
        let r = unseal(&mut v, &codec);
        assert!(r.locked.is_empty() && !r.needs_save);
        assert_eq!(v, sendspin_sample());
        // Blank fields are dropped and nothing is stored for them.
        let mut v = json!({ "sendspin": { "enabled": false, "client_key": null,
            "server_pairings": [], "player_pairings": {} } });
        assert!(!unseal(&mut v, &codec).needs_save);
        seal(&mut v, &[], &codec);
        assert_eq!(v, json!({ "sendspin": { "enabled": false } }));
    }

    #[test]
    fn versions_without_groups_keep_them_and_their_unpaired_identity_gives_way() {
        let codec = FakeCodec(0x44);
        let mut sealed = sendspin_sample();
        seal(&mut sealed, &[], &codec);
        // A version without groups: no Sendspin identity in sight, and the
        // group entries are unknown to it.
        let mut old = sealed.clone();
        let r = unseal_with(&mut old, &codec, &[]);
        assert_eq!(r.locked.len(), 2);
        assert!(old["sendspin"].get("client_key").is_none());
        // It mints identities of its own and saves them in the plain fields.
        mint_unpaired(&mut old, 2);
        seal_with(&mut old, &r.locked, &codec, &[]);
        assert_eq!(old["sendspin"]["client_key"], "ck-2");
        assert_eq!(old[PROTECTED_KEY], sealed[PROTECTED_KEY], "groups kept byte for byte");
        // Back here: nothing was paired with the minted identities, so the
        // sealed groups win, and the file is rewritten without them.
        let r = unseal(&mut old, &codec);
        assert!(r.needs_save && r.locked.is_empty());
        assert_eq!(old, sendspin_sample());
    }

    #[test]
    fn a_group_paired_again_by_an_older_version_wins() {
        let codec = FakeCodec(0x45);
        let mut v = sendspin_sample();
        seal(&mut v, &[], &codec);
        let original = v[PROTECTED_KEY].clone();
        let r = unseal_with(&mut v, &codec, &[]);
        mint_unpaired(&mut v, 2);
        // The client role is paired again; the server role is not.
        v["sendspin"]["server_pairings"] =
            json!([{ "server_id": "srv-2", "psk": "lt-s2", "last_used_unix": 9 }]);
        seal_with(&mut v, &r.locked, &codec, &[]);
        let r = unseal(&mut v, &codec);
        assert!(r.needs_save);
        let s = &v["sendspin"];
        assert_eq!(s["client_key"], "ck-2");
        assert_eq!(s["pairing_psk"], "pp-2");
        assert_eq!(s["server_pairings"][0]["psk"], "lt-s2");
        assert_eq!(s["server_key"], "sk-1");
        assert_eq!(s["player_pairings"], sendspin_sample()["sendspin"]["player_pairings"]);
        // The replaced group is gone; the server group is untouched.
        assert!(r.locked.is_empty());
        seal(&mut v, &r.locked, &codec);
        let p = v[PROTECTED_KEY].as_object().unwrap();
        assert_eq!(p.len(), 2);
        assert_ne!(p[CLIENT_GROUP], original[CLIENT_GROUP], "replaced");
        assert_eq!(p[SERVER_GROUP], original[SERVER_GROUP]);
        let mut back = v.clone();
        let r = unseal(&mut back, &codec);
        assert!(r.locked.is_empty() && !r.needs_save);
        assert_eq!(back["sendspin"]["server_pairings"].as_array().unwrap().len(), 1);
        assert_eq!(back["sendspin"]["server_pairings"][0]["server_id"], "srv-2");

        // Replaced again: still one group per role.
        let r = unseal_with(&mut back, &codec, &[]);
        back["sendspin"]["client_key"] = json!("ck-3");
        back["sendspin"]["server_pairings"] = json!([{ "server_id": "srv-3", "psk": "lt-s3" }]);
        seal_with(&mut back, &r.locked, &codec, &[]);
        let r = unseal(&mut back, &codec);
        assert_eq!(back["sendspin"]["client_key"], "ck-3");
        seal(&mut back, &r.locked, &codec);
        assert_eq!(back[PROTECTED_KEY].as_object().unwrap().len(), 2);
    }

    #[test]
    fn fields_a_later_version_adds_to_a_group_are_kept() {
        let codec = FakeCodec(0x48);
        let mut v = sendspin_sample();
        seal(&mut v, &[], &codec);
        let mut group = open_group(&v[PROTECTED_KEY][CLIENT_GROUP], &codec).unwrap();
        group.insert("server_cert".into(), json!("cert-1"));
        v[PROTECTED_KEY][CLIENT_GROUP] = seal_value(&Value::Object(group), &codec).unwrap();
        let reopen = |v: &Value| open_group(&v[PROTECTED_KEY][CLIENT_GROUP], &codec).unwrap();

        let r = unseal(&mut v, &codec);
        assert_eq!(v, sendspin_sample());
        let mut saved = v.clone();
        seal(&mut saved, &r.locked, &codec);
        assert_eq!(reopen(&saved)["server_cert"], "cert-1");
        assert_eq!(saved[PROTECTED_KEY].as_object().unwrap().len(), 2, "nothing kept aside");
        // A change to the group keeps it too.
        v["sendspin"]["server_pairings"][0]["last_used_unix"] = json!(6);
        seal(&mut v, &r.locked, &codec);
        let g = reopen(&v);
        assert_eq!(g["server_cert"], "cert-1");
        assert_eq!(g["server_pairings"][0]["last_used_unix"], 6);
        assert_eq!(v[PROTECTED_KEY].as_object().unwrap().len(), 2);
        // And through the next load and save.
        let r = unseal(&mut v, &codec);
        seal(&mut v, &r.locked, &codec);
        assert_eq!(reopen(&v)["server_cert"], "cert-1");
    }

    #[test]
    fn an_unopenable_group_is_kept_until_a_paired_one_replaces_it() {
        let mut sealed = sendspin_sample();
        seal(&mut sealed, &[], &FakeCodec(7));
        // Another user / machine: the groups don't open, so it mints its
        // own identities. Saving keeps the stored groups as they are, and
        // the unpaired identities beside them.
        let mut v = sealed.clone();
        let r = unseal(&mut v, &FakeCodec(8));
        assert_eq!(r.locked.len(), 2);
        mint_unpaired(&mut v, 3);
        let mut saved = v.clone();
        seal(&mut saved, &r.locked, &FakeCodec(8));
        let p = saved[PROTECTED_KEY].as_object().unwrap();
        assert_eq!(p[CLIENT_GROUP], sealed[PROTECTED_KEY][CLIENT_GROUP]);
        assert_eq!(p[SERVER_GROUP], sealed[PROTECTED_KEY][SERVER_GROUP]);
        assert_eq!(p.len(), 4);
        assert_eq!(saved["sendspin"], sealed["sendspin"], "nothing plain");
        // The next launch there gets the same identities back, still with
        // the stored groups kept.
        let mut next = saved.clone();
        let r3 = unseal(&mut next, &FakeCodec(8));
        assert_eq!(next["sendspin"]["client_key"], "ck-3");
        assert_eq!(next["sendspin"]["server_key"], "sk-3");
        let mut resaved = next.clone();
        seal(&mut resaved, &r3.locked, &FakeCodec(8));
        assert_eq!(resaved[PROTECTED_KEY][CLIENT_GROUP], sealed[PROTECTED_KEY][CLIENT_GROUP]);
        // Where the stored groups open, they win and the minted ones go.
        let mut home = saved.clone();
        let r4 = unseal(&mut home, &FakeCodec(7));
        assert!(r4.locked.is_empty());
        assert_eq!(home, sendspin_sample());
        seal(&mut home, &r4.locked, &FakeCodec(7));
        assert_eq!(home[PROTECTED_KEY].as_object().unwrap().len(), 2);
        // Once a server pairs with the new client identity, its group
        // replaces the stored one; the server role's group stays.
        v["sendspin"]["server_pairings"] = json!([{ "server_id": "srv-3", "psk": "lt-s3" }]);
        seal(&mut v, &r.locked, &FakeCodec(8));
        let p = v[PROTECTED_KEY].as_object().unwrap();
        assert_ne!(p[CLIENT_GROUP], sealed[PROTECTED_KEY][CLIENT_GROUP]);
        assert_eq!(p[SERVER_GROUP], sealed[PROTECTED_KEY][SERVER_GROUP]);
        // The replaced group and its pending identity are gone; the
        // unpaired server identity is still pending beside its group.
        let mut keys: Vec<&str> = p.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, [CLIENT_GROUP, SERVER_GROUP, "/sendspin/server_identity_pending"]);
        let mut later = v.clone();
        let r2 = unseal(&mut later, &FakeCodec(8));
        assert_eq!(r2.locked.len(), 1, "only the server group is kept");
        seal(&mut later, &r2.locked, &FakeCodec(8));
        assert_eq!(later[PROTECTED_KEY], v[PROTECTED_KEY]);
        let mut back = v.clone();
        unseal(&mut back, &FakeCodec(8));
        assert_eq!(back["sendspin"]["client_key"], "ck-3");
        // Without any protector, everything stays where it is.
        let mut v2 = sealed.clone();
        let r2 = unseal(&mut v2, &PlatformCodecForTests);
        mint_unpaired(&mut v2, 4);
        seal(&mut v2, &r2.locked, &PlatformCodecForTests);
        assert_eq!(v2[PROTECTED_KEY], sealed[PROTECTED_KEY]);
        assert_eq!(v2["sendspin"]["client_key"], "ck-4");
    }

    #[test]
    fn a_group_with_many_players_still_opens() {
        let codec = FakeCodec(0x47);
        let mut v = sendspin_sample();
        let players = v["sendspin"]["player_pairings"].as_object_mut().unwrap();
        for i in 0..2000 {
            players.insert(
                format!("{i:043}"),
                json!({ "psk": format!("{i:043}"), "name": "Living Room Speaker",
                        "instance": "living room speaker" }),
            );
        }
        // And one peer's name of several MB (names are cut when stored,
        // but whatever is sealed must open again).
        players["pl-1"]["name"] = json!("n".repeat(3 << 20));
        let want = v.clone();
        seal(&mut v, &[], &codec);
        assert!(v[PROTECTED_KEY][SERVER_GROUP].as_str().unwrap().len() > 4 << 20);
        let r = unseal(&mut v, &codec);
        assert!(r.locked.is_empty());
        assert_eq!(v, want);
    }

    #[test]
    fn removed_pairings_stay_removed() {
        let codec = FakeCodec(0x46);
        let mut v = sendspin_sample();
        seal(&mut v, &[], &codec);
        let r = unseal(&mut v, &codec);
        // The only server pairing is revoked; the identity stays.
        v["sendspin"]["server_pairings"] = json!([]);
        seal(&mut v, &r.locked, &codec);
        let r = unseal(&mut v, &codec);
        assert!(r.locked.is_empty());
        assert_eq!(v["sendspin"]["client_key"], "ck-1");
        assert!(v["sendspin"].get("server_pairings").is_none());
        assert_eq!(v["sendspin"]["player_pairings"]["pl-1"]["psk"], "lt-p1");
    }

    #[test]
    fn malformed_groups_never_panic() {
        let codec = FakeCodec(5);
        let not_an_object = seal_value(&json!("just a string"), &codec).unwrap();
        for bad in [json!("dpapi:v1:!!!"), json!(3), json!(null), not_an_object] {
            let mut v = json!({ "sendspin": { "client_key": "ck-9" },
                PROTECTED_KEY: { CLIENT_GROUP: bad.clone(), SERVER_GROUP: bad.clone() } });
            let r = unseal(&mut v, &codec);
            assert_eq!(r.locked.len(), 2, "{bad}");
            assert_eq!(v["sendspin"]["client_key"], "ck-9");
            seal(&mut v, &r.locked, &codec);
            assert_eq!(v[PROTECTED_KEY].get(CLIENT_GROUP), Some(&bad));
            assert_eq!(v[PROTECTED_KEY].get(SERVER_GROUP), Some(&bad));
            assert!(v["sendspin"].get("client_key").is_none());
        }
        // A group that opens but has nowhere to go is kept as well.
        let good = seal_value(&json!({ "client_key": "ck-1" }), &codec).unwrap();
        for parent in [json!(5), json!(null)] {
            let mut v = json!({ "sendspin": parent, PROTECTED_KEY: { CLIENT_GROUP: good.clone() } });
            let r = unseal(&mut v, &codec);
            assert_eq!(r.locked.len(), 1);
        }
        // A missing section is created for it.
        let mut v = json!({ PROTECTED_KEY: { CLIENT_GROUP: good } });
        assert!(unseal(&mut v, &codec).locked.is_empty());
        assert_eq!(v["sendspin"]["client_key"], "ck-1");
    }
}
