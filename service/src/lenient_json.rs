//! Reading a settings type from JSON so that a value it can't take costs
//! that value, not the whole file.
//!
//! [`from_value`] parses with the type's own `Deserialize`, through a
//! deserializer that notes the key path where a parse fails. The value at
//! that path is then hidden from the parse, which runs again, until it
//! succeeds. A hidden field takes its default (every field of the settings
//! types has one); a hidden required field makes its struct fail next, so
//! a list or map entry that can't be read is left out as a whole. Nothing
//! in a type has to opt in: a field added later is covered as it is.
//!
//! Paths are reported, never values: values can be secrets.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Error, Map, Value};
use std::cell::RefCell;
use std::collections::HashSet;

/// A key path: object keys, and list positions as decimal text (positions
/// in the tree as given: hiding an item doesn't renumber the others).
pub type KeyPath = Vec<String>;

/// Values hidden one by one before a parse gives up on the details and
/// hides the top-level setting a failure is in: bounds the work on a file
/// full of values this version can't read.
const FINE_REMOVALS: usize = 64;

/// `T` from `tree`, and the paths of the values left out to get it, in the
/// order they were found (a value left out inside one that was left out
/// as well is not listed). `Err` when `T` can't be read even with every
/// value left out (the tree isn't an object, for a struct).
pub fn from_value<T: DeserializeOwned>(tree: &Value) -> Result<(T, Vec<KeyPath>), String> {
    let mut hidden: HashSet<KeyPath> = HashSet::new();
    let mut found: Vec<KeyPath> = Vec::new();
    loop {
        let ctx = Context { failure: RefCell::new(None), hidden: &hidden };
        let error = match T::deserialize(Tracked { value: tree, path: Vec::new(), ctx: &ctx }) {
            Ok(t) => {
                let lost = found
                    .iter()
                    .filter(|p| !(1..p.len()).any(|n| p.get(..n).is_some_and(|a| hidden.contains(a))))
                    .cloned()
                    .collect();
                return Ok((t, lost));
            }
            Err(e) => e,
        };
        let mut path = ctx.failure.into_inner().unwrap_or_default();
        if found.len() >= FINE_REMOVALS {
            path.truncate(1);
        }
        if path.is_empty() || !hidden.insert(path.clone()) {
            return Err(error.to_string());
        }
        found.push(path);
    }
}

/// What every level of one parse shares.
struct Context<'a> {
    /// Where the parse failed, deepest first (see [`note`]).
    failure: RefCell<Option<KeyPath>>,
    /// Values the parse doesn't see.
    hidden: &'a HashSet<KeyPath>,
}

/// A value being parsed at `path`.
struct Tracked<'a> {
    value: &'a Value,
    path: KeyPath,
    ctx: &'a Context<'a>,
}

impl<'a> Tracked<'a> {
    fn child(&self, key: String, value: &'a Value) -> Tracked<'a> {
        let mut path = self.path.clone();
        path.push(key);
        Tracked { value, path, ctx: self.ctx }
    }

    fn note<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        note(&self.path, &self.ctx.failure, result)
    }

    fn hides(&self, key: &str) -> bool {
        let mut path = self.path.clone();
        path.push(key.to_string());
        self.ctx.hidden.contains(&path)
    }

    fn visit_object<V: Visitor<'a>>(&self, map: &'a Map<String, Value>, visitor: V) -> Result<V::Value, Error> {
        let mut access = Entries { parent: self, iter: map.iter(), value: None };
        let out = visitor.visit_map(&mut access)?;
        if access.iter.any(|(k, _)| !self.hides(k)) {
            Err(de::Error::invalid_length(map.len(), &"fewer elements in map"))
        } else {
            Ok(out)
        }
    }

    fn visit_array<V: Visitor<'a>>(&self, items: &'a [Value], visitor: V) -> Result<V::Value, Error> {
        let mut access = Items { parent: self, iter: items.iter().enumerate() };
        let out = visitor.visit_seq(&mut access)?;
        if access.iter.any(|(i, _)| !self.hides(&i.to_string())) {
            Err(de::Error::invalid_length(items.len(), &"fewer elements in array"))
        } else {
            Ok(out)
        }
    }
}

/// Note where `result` failed, at `path`. A failure already noted inside
/// this value is deeper and stays; one noted elsewhere was handled by
/// whoever asked for that value (it parsed in the end) and gives way. A
/// success clears a failure noted inside this value for the same reason.
fn note<T>(path: &KeyPath, failure: &RefCell<Option<KeyPath>>, result: Result<T, Error>) -> Result<T, Error> {
    let mut failure = failure.borrow_mut();
    let inside = failure.as_ref().is_some_and(|f| f.starts_with(path));
    match &result {
        Err(_) if !inside => *failure = Some(path.clone()),
        Ok(_) if inside => *failure = None,
        _ => {}
    }
    result
}

/// Leaf types go to `serde_json`'s own deserializer for the value.
macro_rules! leaf {
    ($($method:ident),*) => {$(
        fn $method<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
            self.note(de::Deserializer::$method(self.value, visitor))
        }
    )*};
}

impl<'a> de::Deserializer<'a> for Tracked<'a> {
    type Error = Error;

    leaf!(
        deserialize_bool, deserialize_i8, deserialize_i16, deserialize_i32, deserialize_i64,
        deserialize_i128, deserialize_u8, deserialize_u16, deserialize_u32, deserialize_u64,
        deserialize_u128, deserialize_f32, deserialize_f64, deserialize_char, deserialize_str,
        deserialize_string, deserialize_bytes, deserialize_byte_buf, deserialize_unit,
        deserialize_identifier, deserialize_ignored_any
    );

    fn deserialize_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        let result = match self.value {
            Value::Object(map) => self.visit_object(map, visitor),
            Value::Array(items) => self.visit_array(items, visitor),
            other => de::Deserializer::deserialize_any(other, visitor),
        };
        self.note(result)
    }

    fn deserialize_option<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.value.is_null() {
            return self.note(visitor.visit_none());
        }
        let result = visitor.visit_some(Tracked { value: self.value, path: self.path.clone(), ctx: self.ctx });
        self.note(result)
    }

    fn deserialize_unit_struct<V: Visitor<'a>>(self, name: &'static str, visitor: V) -> Result<V::Value, Error> {
        self.note(de::Deserializer::deserialize_unit_struct(self.value, name, visitor))
    }

    fn deserialize_newtype_struct<V: Visitor<'a>>(self, _: &'static str, visitor: V) -> Result<V::Value, Error> {
        let result =
            visitor.visit_newtype_struct(Tracked { value: self.value, path: self.path.clone(), ctx: self.ctx });
        self.note(result)
    }

    fn deserialize_seq<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        let result = match self.value {
            Value::Array(items) => self.visit_array(items, visitor),
            other => de::Deserializer::deserialize_seq(other, visitor),
        };
        self.note(result)
    }

    fn deserialize_tuple<V: Visitor<'a>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'a>>(self, _: &'static str, _: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        let result = match self.value {
            Value::Object(map) => self.visit_object(map, visitor),
            other => de::Deserializer::deserialize_map(other, visitor),
        };
        self.note(result)
    }

    fn deserialize_struct<V: Visitor<'a>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let result = match self.value {
            Value::Object(map) => self.visit_object(map, visitor),
            other => de::Deserializer::deserialize_struct(other, name, fields, visitor),
        };
        self.note(result)
    }

    fn deserialize_enum<V: Visitor<'a>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.note(de::Deserializer::deserialize_enum(self.value, name, variants, visitor))
    }
}

/// An object's entries, hidden ones left out; each value is parsed at its
/// key's path.
struct Entries<'p, 'a> {
    parent: &'p Tracked<'a>,
    iter: serde_json::map::Iter<'a>,
    value: Option<(&'a String, &'a Value)>,
}

impl<'a> MapAccess<'a> for Entries<'_, 'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'a>>(&mut self, seed: K) -> Result<Option<K::Value>, Error> {
        let parent = self.parent;
        let Some((key, value)) = self.iter.by_ref().find(|(k, _)| !parent.hides(k)) else {
            return Ok(None);
        };
        self.value = Some((key, value));
        let entry = parent.child(key.clone(), value);
        let deserializer = de::value::BorrowedStrDeserializer::<Error>::new(key.as_str());
        entry.note(seed.deserialize(deserializer)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'a>>(&mut self, seed: V) -> Result<V::Value, Error> {
        // A success here clears a failure noted inside the value and
        // handled by the code that read it.
        match self.value.take() {
            Some((key, value)) => {
                let child = self.parent.child(key.clone(), value);
                let path = child.path.clone();
                note(&path, &self.parent.ctx.failure, seed.deserialize(child))
            }
            None => Err(de::Error::custom("value asked for before its key")),
        }
    }
}

/// A list's items, hidden ones left out; each is parsed at its position's
/// path.
struct Items<'p, 'a> {
    parent: &'p Tracked<'a>,
    iter: std::iter::Enumerate<std::slice::Iter<'a, Value>>,
}

impl<'a> SeqAccess<'a> for Items<'_, 'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'a>>(&mut self, seed: T) -> Result<Option<T::Value>, Error> {
        let parent = self.parent;
        match self.iter.by_ref().find(|(i, _)| !parent.hides(&i.to_string())) {
            Some((i, value)) => {
                let child = parent.child(i.to_string(), value);
                let path = child.path.clone();
                note(&path, &parent.ctx.failure, seed.deserialize(child)).map(Some)
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::HashMap;

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        #[default]
        A,
        B,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Entry {
        host: String,
        #[serde(default)]
        port: u16,
        #[serde(default)]
        kind: Kind,
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    struct Inner {
        on: bool,
        level: Option<u32>,
        bands: [f32; 3],
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    struct Outer {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        count: u32,
        #[serde(default)]
        inner: Inner,
        #[serde(default)]
        entries: Vec<Entry>,
        #[serde(default)]
        by_id: HashMap<String, Inner>,
        #[serde(default)]
        adapter: Option<Entry>,
        #[serde(flatten)]
        rest: Map<String, Value>,
    }

    fn path(p: &[&str]) -> KeyPath {
        p.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_good_tree_parses_as_serde_would() {
        let tree = json!({"name": "n", "count": 3, "inner": {"on": true, "level": 2, "bands": [1.0, 2.0, 3.0]},
            "entries": [{"host": "h", "port": 9, "kind": "b"}], "by_id": {"x": {"on": true}},
            "adapter": {"host": "a"}, "later": [1]});
        let (lenient, removed) = from_value::<Outer>(&tree).unwrap();
        assert_eq!(lenient, serde_json::from_value::<Outer>(tree).unwrap());
        assert!(removed.is_empty());
    }

    #[test]
    fn each_value_that_cannot_be_read_is_left_out_alone() {
        let tree = json!({
            "name": 5,
            "count": 1.5,
            "inner": {"on": "yes", "level": 7, "bands": [1.0, "x", 3.0]},
            "entries": [{"host": "keep", "port": 70000}, {"host": 4}, {"host": "kind", "kind": "c"}, {"host": "ok"}],
            "by_id": {"x": {"on": true, "level": -1}, "y": "not an object"},
            "adapter": {"host": ["a"]},
            "later": {"kept": true}
        });
        let (o, removed) = from_value::<Outer>(&tree).unwrap();
        assert_eq!(o.name, None);
        assert_eq!(o.count, 0);
        assert_eq!(o.inner, Inner { on: false, level: Some(7), bands: [0.0; 3] });
        let entries: Vec<(&str, u16, &Kind)> = o.entries.iter().map(|e| (e.host.as_str(), e.port, &e.kind)).collect();
        assert_eq!(entries, [("keep", 0, &Kind::A), ("kind", 0, &Kind::A), ("ok", 0, &Kind::A)]);
        assert_eq!(o.by_id.len(), 1);
        assert_eq!(o.by_id["x"], Inner { on: true, ..Default::default() });
        assert_eq!(o.adapter, None);
        assert_eq!(o.rest["later"], json!({"kept": true}));
        // In the order the parse meets them (keys sorted, as `serde_json`
        // keeps them), at their positions in the file; a value inside one
        // left out as a whole is not listed.
        assert_eq!(
            removed,
            [
                path(&["adapter"]),
                path(&["by_id", "x", "level"]),
                path(&["by_id", "y"]),
                path(&["count"]),
                path(&["entries", "0", "port"]),
                path(&["entries", "1"]),
                path(&["entries", "2", "kind"]),
                path(&["inner", "bands"]),
                path(&["inner", "on"]),
                path(&["name"]),
            ]
        );
    }

    #[test]
    fn a_tree_that_is_not_an_object_is_refused() {
        for tree in [json!([1]), json!("x"), json!(null), json!(3)] {
            assert!(from_value::<Outer>(&tree).is_err(), "{tree}");
        }
    }

    #[test]
    fn a_tree_full_of_bad_values_ends_in_bounded_work() {
        let entries: Vec<Value> = (0..5000).map(|i| json!({"host": i})).collect();
        let tree = json!({"name": "kept", "entries": entries, "by_id": {"x": {"on": true}}});
        let (o, removed) = from_value::<Outer>(&tree).unwrap();
        assert_eq!(o.name.as_deref(), Some("kept"));
        assert!(o.entries.is_empty());
        assert_eq!(o.by_id.len(), 1, "other settings are not touched");
        // The entries left out one by one are inside the list left out
        // as a whole: only the list is listed.
        assert_eq!(removed, [path(&["entries"])]);
    }

    #[test]
    fn a_failure_handled_inside_a_value_is_forgotten() {
        // A field read leniently by its own code (here: any JSON, then
        // tried as a number) must not leave a stale path behind that a
        // later failure would be blamed on.
        fn number_or_none<'de, D: de::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
            let v = Value::deserialize(d)?;
            Ok(serde_json::from_value(v).ok())
        }
        fn try_u32_else_none<'de, D: de::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
            Ok(u32::deserialize(d).ok())
        }
        #[derive(Debug, Deserialize)]
        struct Lenient {
            #[serde(default, deserialize_with = "number_or_none")]
            a: Option<u32>,
            #[serde(default, deserialize_with = "try_u32_else_none")]
            b: Option<u32>,
            #[serde(default)]
            c: u32,
        }
        let (l, removed) = from_value::<Lenient>(&json!({"a": "x", "b": "y", "c": "z"})).unwrap();
        assert_eq!((l.a, l.b, l.c), (None, None, 0));
        assert_eq!(removed, [path(&["c"])]);
    }
}
