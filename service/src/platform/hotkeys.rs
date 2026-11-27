//! The two delay keys and their larger steps while Shift is held.
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DelayKeys(pub [Vec<String>; 2]);

impl Default for DelayKeys {
    fn default() -> Self {
        Self([",", "."].map(|key| vec!["Ctrl".into(), "Win".into(), key.into()]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayPress {
    pub add: bool,
    pub large: bool,
}

impl DelayPress {
    pub const ALL: [Self; 4] = [
        Self {
            add: false,
            large: false,
        },
        Self {
            add: true,
            large: false,
        },
        Self {
            add: false,
            large: true,
        },
        Self {
            add: true,
            large: true,
        },
    ];

    pub fn keys(self, bindings: &DelayKeys) -> Vec<String> {
        let mut keys = bindings.0[usize::from(self.add)].clone();
        if self.large && !keys.is_empty() && !keys.iter().any(|k| k == "Shift") {
            keys.insert(keys.len() - 1, "Shift".into());
        }
        keys
    }
}

pub type KeySink = Arc<dyn Fn(DelayPress) + Send + Sync>;

pub trait Hotkeys: Send + Sync {
    fn register(&self, keys: &DelayKeys, sink: KeySink) -> KeyRegistration;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStatus { Registered, Taken, Rejected }

/// Dropping the registration releases every key Windows accepted.
pub struct KeyRegistration {
    pub status: [KeyStatus; 4],
    _owner: Box<dyn Send>,
}

impl KeyRegistration {
    pub fn new(status: [KeyStatus; 4], owner: impl Send + 'static) -> Self {
        Self { status, _owner: Box::new(owner) }
    }

    pub fn rows(&self) -> [KeyStatus; 3] {
        let [remove, add, a, b] = self.status;
        let large = if [a, b].contains(&KeyStatus::Rejected) { KeyStatus::Rejected }
            else if a == KeyStatus::Taken || b == KeyStatus::Taken { KeyStatus::Taken }
            else { KeyStatus::Registered };
        [remove, add, large]
    }
}

impl Hotkeys for super::Passive {
    fn register(&self, _: &DelayKeys, _: KeySink) -> KeyRegistration {
        KeyRegistration::new([KeyStatus::Rejected; 4], ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_keeps_conflicts_separate_from_other_failures() {
        use KeyStatus::*;
        let keys = DelayKeys::default();
        let failed = super::super::Platform::passive().hotkeys.register(&keys, Arc::new(|_| {}));
        assert_eq!(failed.rows(), [Rejected; 3]);
        let mixed = KeyRegistration::new([Registered, Taken, Taken, Rejected], ());
        assert_eq!(mixed.rows(), [Registered, Taken, Rejected]);
    }
}
