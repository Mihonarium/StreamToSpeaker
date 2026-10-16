//! Property lists read from the network, parsed with limits on how deep
//! they nest and how many values they hold.
//!
//! `plist::from_bytes::<plist::Value>` builds nested arrays and
//! dictionaries by recursion, a few stack frames per level with no limit,
//! so a body of a few tens of KB of nested one-element arrays overflows a
//! thread's stack — and a stack overflow aborts the whole process; the
//! response size caps are far above that. A binary plist can also refer
//! to one object from many places, and plist's binary reader reads a
//! shared object again at every reference (it refuses only an object that
//! contains itself), so under 1 KB of arrays that each refer several
//! times to the next describes more values than memory holds. Every plist
//! that came from a receiver is parsed with [`parse`] instead: it builds
//! the same `Value`, but refuses nesting deeper than [`MAX_DEPTH`] before
//! the next level costs any stack, and stops at the first value past
//! [`MAX_VALUES`], a shared object counting at every reference, or once
//! its strings and data pass [`MAX_STRING_AND_DATA_BYTES`]. plist's event
//! readers (binary, XML and ASCII) walk the input iteratively, so the
//! depth of the value being built is the only recursion, and stopping
//! there bounds the stack whatever the input.

use plist::{Dictionary, Value};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::cell::Cell;
use std::fmt;
use std::io::{Cursor, Read, Seek};

/// Deepest nesting of arrays and dictionaries accepted. AirPlay plists
/// nest three or four levels (`streams[0]`, `audioFormats[]`,
/// `timingPeerList[]`), so this leaves ample room.
pub const MAX_DEPTH: usize = 32;

/// Most values accepted in one property list: the root, every array
/// member and every dictionary key and value, an object referred to from
/// several places counting at each. A receiver's `/info` answer, the
/// largest plist AirPlay sends, holds a few hundred.
pub const MAX_VALUES: usize = 10_000;

/// Most bytes of strings (dictionary keys included) and data accepted in
/// one property list. A shared string or data object is built again at
/// every reference too, so counting values alone would still let one
/// object near the 1 MiB response cap be built ten thousand times.
/// AirPlay plists carry a few KB.
pub const MAX_STRING_AND_DATA_BYTES: usize = 1 << 20;

/// Parse a binary, XML or ASCII plist that came from the network. A body
/// nested deeper than [`MAX_DEPTH`], holding more than [`MAX_VALUES`]
/// values or more than [`MAX_STRING_AND_DATA_BYTES`] of strings and data
/// fails the way a malformed one does.
///
/// The result equals `plist::from_bytes::<Value>`'s except that dates come
/// back as their XML text and UIDs as integers (plist only produces those
/// two kinds for its own `Value` type); no AirPlay field uses either.
pub fn parse(body: &[u8]) -> Result<Value, plist::Error> {
    from_reader(Cursor::new(body))
}

/// [`parse`] from any seekable reader, so the tests can count what a
/// parse reads.
fn from_reader<R: Read + Seek>(reader: R) -> Result<Value, plist::Error> {
    plist::from_reader::<R, Bounded>(reader).map(|b| b.0)
}

struct Bounded(Value);

impl<'de> de::Deserialize<'de> for Bounded {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let budget = Budget {
            values: Cell::new(MAX_VALUES),
            bytes: Cell::new(MAX_STRING_AND_DATA_BYTES),
        };
        Level { depth: 0, budget: &budget }.deserialize(d).map(Bounded)
    }
}

/// How many more values, and bytes of strings and data, one parse may
/// build. Every level of the parse draws on the same counts.
struct Budget {
    values: Cell<usize>,
    bytes: Cell<usize>,
}

impl Budget {
    /// Count one more value, or fail when that would pass [`MAX_VALUES`].
    fn take_value<E: de::Error>(&self) -> Result<(), E> {
        match self.values.get().checked_sub(1) {
            Some(left) => {
                self.values.set(left);
                Ok(())
            }
            None => Err(too_many_values()),
        }
    }

    /// Count `n` more bytes of strings or data, or fail when that would
    /// pass [`MAX_STRING_AND_DATA_BYTES`].
    fn take_bytes<E: de::Error>(&self, n: usize) -> Result<(), E> {
        match self.bytes.get().checked_sub(n) {
            Some(left) => {
                self.bytes.set(left);
                Ok(())
            }
            None => Err(E::custom(format_args!(
                "property list holds more than {} bytes of strings and data",
                MAX_STRING_AND_DATA_BYTES
            ))),
        }
    }

    /// Fail now when a collection declares more members than are left to
    /// count. plist's binary reader has read the references to all of a
    /// collection's members by the time it hands the collection over, so
    /// counting members only as they arrive would let nested collections
    /// that each declare most of the body make it read the body once per
    /// level before the count ran out. XML and ASCII collections declare
    /// no length; their members are counted as they arrive.
    fn check_room<E: de::Error>(&self, declared: Option<usize>) -> Result<(), E> {
        match declared {
            Some(n) if n > self.values.get() => Err(too_many_values()),
            _ => Ok(()),
        }
    }
}

fn too_many_values<E: de::Error>() -> E {
    E::custom(format_args!("property list holds more than {} values", MAX_VALUES))
}

/// Builds one value that sits inside `depth` enclosing arrays and
/// dictionaries, drawing on the parse's `budget`.
#[derive(Clone, Copy)]
struct Level<'b> {
    depth: usize,
    budget: &'b Budget,
}

impl<'b> Level<'b> {
    /// The level of a collection's members, or an error when the
    /// collection itself would be nested too deeply or declares more
    /// members than the budget has left. Checked before any member is
    /// read, so the recursion stops here.
    fn members<E: de::Error>(self, declared: Option<usize>) -> Result<Level<'b>, E> {
        if self.depth >= MAX_DEPTH {
            return Err(E::custom(format_args!(
                "property list nested deeper than {} levels",
                MAX_DEPTH
            )));
        }
        self.budget.check_room(declared)?;
        Ok(Level { depth: self.depth + 1, ..self })
    }
}

impl<'de> DeserializeSeed<'de> for Level<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.budget.take_value()?;
        d.deserialize_any(self)
    }
}

/// Reads a dictionary key, which counts as a value like any other.
struct Key<'b>(&'b Budget);

impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        self.0.take_value()?;
        // A key that isn't a string is refused by String's own visitor
        // without descending into it.
        let key = <String as de::Deserialize>::deserialize(d)?;
        self.0.take_bytes(key.len())?;
        Ok(key)
    }
}

impl<'de> Visitor<'de> for Level<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a property list value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Boolean(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Integer(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Integer(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Real(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.budget.take_bytes(v.len())?;
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.budget.take_bytes(v.len())?;
        Ok(Value::String(v))
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Value, E> {
        self.budget.take_bytes(v.len())?;
        Ok(Value::Data(v.to_vec()))
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Value, E> {
        self.budget.take_bytes(v.len())?;
        Ok(Value::Data(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let members = self.members(seq.size_hint())?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(members)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        // An entry is two values: its key and its value.
        let members = self.members(map.size_hint().map(|n| n.saturating_mul(2)))?;
        let mut dict = Dictionary::new();
        while let Some(key) = map.next_key_seed(Key(self.budget))? {
            let value = map.next_value_seed(members)?;
            dict.insert(key, value);
        }
        Ok(Value::Dictionary(dict))
    }
}

/// Hostile and realistic bodies for the parse tests here and at the call
/// sites.
#[cfg(test)]
pub(crate) mod samples {
    use plist::{Dictionary, Value};

    /// Lays out a binary plist: the header, the objects in the order they
    /// are added (object 0 is the root), the offset table and the trailer.
    struct BinaryPlist {
        out: Vec<u8>,
        offsets: Vec<u32>,
    }

    impl BinaryPlist {
        fn new() -> Self {
            BinaryPlist { out: b"bplist00".to_vec(), offsets: Vec::new() }
        }

        /// Append the next object, already encoded.
        fn object(&mut self, encoded: &[u8]) {
            self.offsets.push(self.out.len() as u32);
            self.out.extend_from_slice(encoded);
        }

        /// Append bytes that start no object. An object that declares more
        /// members than its own encoding holds runs on over them.
        fn filler(&mut self, bytes: &[u8]) {
            self.out.extend_from_slice(bytes);
        }

        /// The finished body, its object references `ref_size` bytes wide.
        fn finish(mut self, ref_size: u8) -> Vec<u8> {
            let table = self.out.len() as u64;
            for o in &self.offsets {
                self.out.extend_from_slice(&o.to_be_bytes());
            }
            self.out.extend_from_slice(&[0; 6]);
            self.out.push(4); // offset size
            self.out.push(ref_size);
            self.out.extend_from_slice(&(self.offsets.len() as u64).to_be_bytes());
            self.out.extend_from_slice(&0u64.to_be_bytes()); // root object
            self.out.extend_from_slice(&table.to_be_bytes());
            self.out
        }
    }

    /// The marker that starts an object of type `kind` (0xA array, 0xD
    /// dictionary, 0x4 data, 0x5 ASCII string) of `len` members or bytes.
    fn marker(kind: u8, len: usize) -> Vec<u8> {
        if len < 15 {
            return vec![kind << 4 | len as u8];
        }
        // The length follows as a 4-byte integer object.
        let mut m = vec![kind << 4 | 0xF, 0x12];
        m.extend_from_slice(&(len as u32).to_be_bytes());
        m
    }

    /// A binary plist of `depth` nested one-element arrays around a
    /// `false`. Every array is its own object, so plist's check for an
    /// object that contains itself never fires.
    pub fn nested_arrays_binary(depth: usize) -> Vec<u8> {
        let mut p = BinaryPlist::new();
        for child in 1..=depth as u32 {
            let mut array = marker(0xA, 1);
            array.extend_from_slice(&child.to_be_bytes());
            p.object(&array);
        }
        p.object(&[0x08]); // false
        p.finish(4)
    }

    /// The same nesting as an XML plist.
    pub fn nested_arrays_xml(depth: usize) -> Vec<u8> {
        let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">");
        s.push_str(&"<array>".repeat(depth));
        s.push_str("<false/>");
        s.push_str(&"</array>".repeat(depth));
        s.push_str("</plist>");
        s.into_bytes()
    }

    /// The same nesting as an ASCII (OpenStep) plist.
    pub fn nested_arrays_ascii(depth: usize) -> Vec<u8> {
        let mut s = "(".repeat(depth);
        s.push_str("x");
        s.push_str(&")".repeat(depth));
        s.into_bytes()
    }

    /// `depth` nested dictionaries `{ "k": { "k": … true } }`, binary,
    /// written by hand: building it as a `Value` would recurse that deep
    /// when dropped.
    pub fn nested_dicts_binary(depth: usize) -> Vec<u8> {
        // Objects: the dictionaries 0..depth, the leaf, then the key.
        let key = depth as u32 + 1;
        let mut p = BinaryPlist::new();
        for child in 1..=depth as u32 {
            let mut dict = marker(0xD, 1); // one entry: key ref, value ref
            dict.extend_from_slice(&key.to_be_bytes());
            dict.extend_from_slice(&child.to_be_bytes());
            p.object(&dict);
        }
        p.object(&[0x09]); // true
        p.object(&[0x51, b'k']); // the one key, shared
        p.finish(4)
    }

    /// A binary plist of `depth` nested arrays in which every array holds
    /// `fanout` references to the one below it, around a `false`: about
    /// `depth * fanout` bytes that describe `fanout`^`depth` copies of the
    /// `false`. No object contains itself, so plist's own check never
    /// fires.
    pub fn shared_arrays_binary(fanout: usize, depth: usize) -> Vec<u8> {
        assert!(depth < 256, "references are one byte");
        let mut p = BinaryPlist::new();
        for child in 1..=depth {
            let mut array = marker(0xA, fanout);
            array.resize(array.len() + fanout, child as u8);
            p.object(&array);
        }
        p.object(&[0x08]); // false
        p.finish(1)
    }

    /// `levels` nested arrays that overlap: each holds a reference to the
    /// next, then runs on over the arrays after it and a shared tail of
    /// `tail` references to a `false`. Every array declares more than
    /// `tail` members, yet the body is only about `tail` bytes.
    pub fn overlapping_arrays_binary(levels: usize, tail: usize) -> Vec<u8> {
        assert!(levels < 256 && tail >= 15, "one-byte references, 4-byte lengths");
        // An array's own bytes: its marker, then its first reference.
        const OWN: usize = 7;
        let mut p = BinaryPlist::new();
        for k in 0..levels {
            // From this array's first reference to the end of the tail.
            let members = OWN * (levels - k) + tail - 6;
            let mut array = marker(0xA, members);
            array.push(k as u8 + 1);
            assert_eq!(array.len(), OWN);
            p.object(&array);
        }
        p.filler(&vec![levels as u8; tail]);
        p.object(&[0x08]); // false, object `levels`
        p.finish(1)
    }

    /// `levels` nested dictionaries that overlap the same way: each
    /// declares `entries` entries, its first key a string and its first
    /// value the next dictionary, and its key and value references run on
    /// over the dictionaries after it into a shared tail of `4 * entries`
    /// bytes (plist wants the trailer to start at least that far past a
    /// dictionary's marker).
    pub fn overlapping_dicts_binary(levels: usize, entries: usize) -> Vec<u8> {
        // A dictionary's own bytes: its marker, then its first key reference.
        const OWN: usize = 7;
        assert!(levels + 2 < 256 && entries >= 15, "one-byte references, 4-byte lengths");
        assert!(entries + 6 >= OWN * levels, "value references start in the tail");
        // Objects: the dictionaries 0..levels, the key, then a `true`.
        let (key, leaf) = (levels as u8, levels as u8 + 1);
        let mut p = BinaryPlist::new();
        for _ in 0..levels {
            let mut dict = marker(0xD, entries);
            dict.push(key);
            assert_eq!(dict.len(), OWN);
            p.object(&dict);
        }
        // Every reference in the tail is the key, a string, which serves
        // as a key or a value, except where a dictionary's value
        // references start: there, its first value.
        let tail_start = 8 + OWN * levels; // after the header and the dictionaries
        let mut tail = vec![key; 4 * entries];
        for k in 0..levels {
            let first_value = 8 + OWN * k + 6 + entries;
            tail[first_value - tail_start] = if k + 1 < levels { k as u8 + 1 } else { leaf };
        }
        p.filler(&tail);
        p.object(&[0x51, b'k']); // the key, object `levels`
        p.object(&[0x09]); // true, object `levels + 1`
        p.finish(1)
    }

    /// An array of `refs` references to one data object of `len` bytes:
    /// about `refs + len` bytes that describe `refs * len` bytes of data.
    pub fn shared_data_binary(refs: usize, len: usize) -> Vec<u8> {
        let mut p = BinaryPlist::new();
        let mut array = marker(0xA, refs);
        array.resize(array.len() + refs, 1);
        p.object(&array);
        let mut data = marker(0x4, len);
        data.resize(data.len() + len, 0xAB);
        p.object(&data);
        p.finish(1)
    }

    /// A dictionary of `entries` entries whose keys all refer to one
    /// string of `len` bytes, every value a `true`.
    pub fn shared_key_binary(entries: usize, len: usize) -> Vec<u8> {
        let mut p = BinaryPlist::new();
        let mut dict = marker(0xD, entries);
        dict.resize(dict.len() + entries, 1); // the key references
        dict.resize(dict.len() + entries, 2); // the value references
        p.object(&dict);
        let mut key = marker(0x5, len);
        key.resize(key.len() + len, b'k');
        p.object(&key);
        p.object(&[0x09]); // true
        p.finish(1)
    }

    fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
        Value::Dictionary(entries.into_iter().collect::<Dictionary>())
    }

    /// A receiver's `GET /info` answer, the largest plist AirPlay sends,
    /// in the shape and size Apple TVs and HomePods give it: the fields
    /// the app reads, the format, latency and display tables, and the TXT
    /// records as data. Made up, not a capture of any one device.
    pub fn info_answer() -> Value {
        let format = |ty: u64| {
            dict([
                ("audioInputFormats", 0x03FF_FFFCu64.into()),
                ("audioOutputFormats", 0x03FF_FFFCu64.into()),
                ("type", ty.into()),
            ])
        };
        let latency = |ty: u64, audio_type: &str| {
            dict([
                ("audioType", audio_type.into()),
                ("inputLatencyMicros", 0u64.into()),
                ("outputLatencyMicros", 400_000u64.into()),
                ("type", ty.into()),
            ])
        };
        let display = dict([
            ("features", 30u64.into()),
            ("heightPixels", 2160u64.into()),
            ("maxFPS", 60u64.into()),
            ("overscanned", false.into()),
            ("refreshRate", (1.0 / 60.0).into()),
            ("rotation", false.into()),
            ("uuid", "E5F7A68D-7B0F-4305-984B-974F677A150B".into()),
            ("widthPixels", 3840u64.into()),
        ]);
        // TXT records, each `key=value` after its length byte.
        let txt = |records: &[&str]| {
            let mut data = Vec::new();
            for r in records {
                data.push(r.len() as u8);
                data.extend_from_slice(r.as_bytes());
            }
            Value::Data(data)
        };
        let pk = "ab".repeat(32);
        let airplay_txt = [
            "acl=0",
            "deviceid=AA:BB:CC:DD:EE:FF",
            "features=0x445F8A00,0x1C340",
            "flags=0x244",
            "gid=5C1D7E2A-3B4F-4A6C-9D8E-0F1A2B3C4D5E",
            "igl=1",
            "gcgl=1",
            "model=AppleTV11,1",
            "protovers=1.1",
            "pi=2E3F8A1C-5B6D-4E7F-8091-A2B3C4D5E6F7",
            "psi=7A8B9C0D-1E2F-4A3B-8C4D-5E6F7A8B9C0D",
            &format!("pk={pk}"),
            "srcvers=860.7.1",
            "osvers=17.0",
            "vv=2",
        ];
        let raop_txt = [
            "cn=0,1,2,3", "da=true", "et=0,3,5", "ft=0x445F8A00,0x1C340", "sf=0x244", "md=0,1,2",
            "am=AppleTV11,1", &format!("pk={pk}"), "tp=UDP", "vn=65537", "vs=860.7.1", "vv=2",
        ];
        dict([
            ("audioFormats", vec![format(96), format(100), format(101), format(103)].into()),
            (
                "audioLatencies",
                vec![
                    latency(96, "default"),
                    latency(96, "media"),
                    latency(96, "telephony"),
                    latency(100, "default"),
                    latency(100, "media"),
                    latency(101, "default"),
                    latency(101, "media"),
                    latency(103, "default"),
                ]
                .into(),
            ),
            ("build", "21K69".into()),
            ("deviceID", "AA:BB:CC:DD:EE:FF".into()),
            ("displays", vec![display].into()),
            ("features", 0x1C340_445F8A00u64.into()),
            ("initialVolume", (-18.5).into()),
            ("keepAliveLowPower", true.into()),
            ("keepAliveSendStatsAsBody", true.into()),
            ("macAddress", "AA:BB:CC:DD:EE:F0".into()),
            ("model", "AppleTV11,1".into()),
            ("name", "Living Room".into()),
            ("nameIsFactoryDefault", false.into()),
            ("pi", "2E3F8A1C-5B6D-4E7F-8091-A2B3C4D5E6F7".into()),
            ("pk", Value::Data(vec![0xAB; 32])),
            (
                "playbackCapabilities",
                dict([
                    ("supportsFPSSecureStop", true.into()),
                    ("supportsInterstitials", true.into()),
                    ("supportsUIForAudioOnlyContent", true.into()),
                ]),
            ),
            ("protocolVersion", "1.1".into()),
            ("psi", "7A8B9C0D-1E2F-4A3B-8C4D-5E6F7A8B9C0D".into()),
            ("sdk", "AirPlay;2.0.2".into()),
            ("senderAddress", "192.168.1.10:49152".into()),
            ("sourceVersion", "860.7.1".into()),
            ("statusFlags", 4u64.into()),
            ("txtAirPlay", txt(&airplay_txt)),
            ("txtRAOP", txt(&raop_txt)),
            ("volumeControlType", 4u64.into()),
            ("vv", 2u64.into()),
        ])
    }

    /// A SETUP (session) answer: the event and timing ports and the
    /// receiver's timing peer record.
    pub fn setup_session_answer() -> Value {
        let addresses = vec![Value::from("192.168.1.20"), Value::from("fe80::1c2b:3a4d:5e6f:7081")];
        dict([
            ("eventPort", 50123u64.into()),
            ("timingPort", 50124u64.into()),
            (
                "timingPeerInfo",
                dict([
                    ("Addresses", addresses.into()),
                    ("ID", "9C3F4A2B-1D5E-4F60-8A7B-C9D0E1F2A3B4".into()),
                    ("SupportsClockPortMatchingOverride", true.into()),
                ]),
            ),
        ])
    }

    /// A SETUP (stream) answer for a realtime stream.
    pub fn setup_stream_answer() -> Value {
        let stream = dict([
            ("audioBufferSize", 8_388_608u64.into()),
            ("controlPort", 50201u64.into()),
            ("dataPort", 50200u64.into()),
            ("latencyMin", 11025u64.into()),
            ("streamID", 1u64.into()),
            ("type", 96u64.into()),
        ]);
        dict([("streams", vec![stream].into())])
    }

    /// Run `f` on a thread with a 256 KiB stack — far less than a real
    /// worker gets, and far less than an unbounded parse of a deep body
    /// needs — and return its result. A stack overflow would abort the
    /// whole test binary instead of returning.
    pub fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::samples::*;
    use super::*;
    use std::io::{self, SeekFrom};

    const HOSTILE_DEPTH: usize = 100_000;

    /// A reader that counts the bytes read from it and fails once they
    /// reach `limit`, so a parse that would build far too much ends there
    /// instead of taking the test's memory and time with it.
    struct Metered<'a> {
        body: Cursor<&'a [u8]>,
        read: usize,
        limit: usize,
    }

    impl Read for Metered<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.read >= self.limit {
                return Err(io::Error::other("the test's read limit was reached"));
            }
            let n = self.body.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    impl Seek for Metered<'_> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.body.seek(pos)
        }
    }

    /// Parse a body that must be refused, reading about `limit` bytes of
    /// it at most: the error, and how many bytes the parse read.
    fn refusal_metered(body: &[u8], limit: usize) -> (String, usize) {
        let mut r = Metered { body: Cursor::new(body), read: 0, limit };
        let err = from_reader(&mut r).map(|_| ()).expect_err("parsed a hostile body");
        (err.to_string(), r.read)
    }

    fn binary_and_xml(value: &Value) -> [Vec<u8>; 2] {
        let mut binary = Vec::new();
        plist::to_writer_binary(&mut binary, value).unwrap();
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, value).unwrap();
        [binary, xml]
    }

    #[test]
    fn deep_bodies_fail_without_exhausting_a_small_stack() {
        let bodies = [
            ("binary", nested_arrays_binary(HOSTILE_DEPTH)),
            ("XML", nested_arrays_xml(HOSTILE_DEPTH)),
            ("ASCII", nested_arrays_ascii(HOSTILE_DEPTH)),
            ("binary dictionaries", nested_dicts_binary(HOSTILE_DEPTH)),
        ];
        for (kind, body) in bodies {
            let err = on_small_stack(move || parse(&body).map(|_| ()).unwrap_err().to_string());
            assert!(err.contains("nested deeper than 32"), "{kind}: {err}");
        }
    }

    #[test]
    fn nesting_up_to_the_limit_parses() {
        for (body, ok) in [
            (nested_arrays_binary(MAX_DEPTH), true),
            (nested_arrays_binary(MAX_DEPTH + 1), false),
            (nested_arrays_xml(MAX_DEPTH), true),
            (nested_arrays_xml(MAX_DEPTH + 1), false),
            (nested_dicts_binary(MAX_DEPTH), true),
            (nested_dicts_binary(MAX_DEPTH + 1), false),
        ] {
            let want = plist::from_bytes::<Value>(&body).unwrap();
            match parse(&body) {
                Ok(v) => {
                    assert!(ok, "accepted a body nested {} deep", MAX_DEPTH + 1);
                    assert_eq!(v, want);
                }
                Err(e) => assert!(!ok, "refused a body within the limit: {e}"),
            }
        }
    }

    /// Under 1 KB of arrays that each refer sixteen times to the one below
    /// describe 16^32 copies of a `false`, and plist reads a shared object
    /// again at every reference. The parse stops at the value limit having
    /// read a few bytes per value; without the limit it would go on until
    /// memory ran out (here, until the meter stopped it).
    #[test]
    fn shared_references_stop_at_the_value_limit() {
        let body = shared_arrays_binary(16, MAX_DEPTH);
        assert!(body.len() < 1024, "{} bytes", body.len());
        let (err, read) = refusal_metered(&body, 256 * 1024);
        assert!(err.contains("more than 10000 values"), "{err}");
        assert!(read < 4 * MAX_VALUES, "read {read} bytes");
    }

    /// plist reads the references to all the members a collection
    /// declares as it opens it. In these bodies the collections overlap,
    /// each declaring thousands of members in a body no more than a few
    /// times that size; the first, whose members could never fit in the
    /// value limit, is refused as it opens, and the body is read about
    /// once instead of once per level. A dictionary entry counts twice
    /// (its key and its value), so the dictionaries declare fewer entries
    /// than the limit, but more than half of it.
    #[test]
    fn a_collection_too_long_for_the_value_limit_is_refused_as_it_opens() {
        let entries = MAX_VALUES * 3 / 5;
        assert!(entries < MAX_VALUES && 2 * entries > MAX_VALUES);
        for (kind, body) in [
            ("arrays", overlapping_arrays_binary(MAX_DEPTH, 64 * 1024)),
            ("dictionaries", overlapping_dicts_binary(MAX_DEPTH, entries)),
        ] {
            let (err, read) = refusal_metered(&body, 8 * body.len());
            assert!(err.contains("more than 10000 values"), "{kind}: {err}");
            assert!(read < 2 * body.len(), "{kind}: read {read} of {} bytes", body.len());
        }
    }

    #[test]
    fn values_up_to_the_limit_parse() {
        // The root counts as a value, and so does every dictionary key.
        let array = |members: usize| Value::Array(vec![Value::Boolean(true); members]);
        let dict = |entries: usize| {
            Value::Dictionary((0..entries).map(|i| (format!("k{i}"), Value::Boolean(true))).collect())
        };
        // One count for the whole body: no collection in these holds more
        // than 100 members, but together they come to exactly the limit,
        // and to one past it.
        let grid = |rows: usize, members: usize| Value::Array(vec![array(members); rows]);
        for (value, ok) in [
            (array(MAX_VALUES - 1), true),
            (array(MAX_VALUES), false),
            (dict((MAX_VALUES - 1) / 2), true),
            (dict((MAX_VALUES - 1) / 2 + 1), false),
            (grid(99, 100), true),
            (grid(100, 99), false),
        ] {
            for body in binary_and_xml(&value) {
                match parse(&body) {
                    Ok(v) => {
                        assert!(ok, "accepted more than {MAX_VALUES} values");
                        assert_eq!(v, value);
                    }
                    Err(e) => {
                        assert!(!ok, "refused a body within the limit: {e}");
                        assert!(e.to_string().contains("more than 10000 values"), "{e}");
                    }
                }
            }
        }
    }

    /// A string or data object of 64 KiB that is referred to thousands
    /// of times describes hundreds of MB in a body of about 70 KB. The
    /// parse stops once the strings and data it built pass the byte limit,
    /// having read little more than the limit.
    #[test]
    fn shared_strings_and_data_stop_at_the_byte_limit() {
        for body in [shared_data_binary(9_000, 64 * 1024), shared_key_binary(4_000, 64 * 1024)] {
            assert!(body.len() < 80 * 1024, "{} bytes", body.len());
            let (err, read) = refusal_metered(&body, 4 * MAX_STRING_AND_DATA_BYTES);
            assert!(err.contains("more than 1048576 bytes of strings and data"), "{err}");
            assert!(read < 2 * MAX_STRING_AND_DATA_BYTES, "read {read} bytes");
        }
    }

    #[test]
    fn strings_and_data_up_to_the_limit_parse() {
        // Sixteen copies of 64 KiB of data make exactly the limit.
        let data = Value::Data(vec![0xAB; MAX_STRING_AND_DATA_BYTES / 16]);
        let within = Value::Array(vec![data; 16]);
        let mut over = within.clone();
        if let Value::Array(items) = &mut over {
            items.push(Value::String("x".into()));
        }
        for (value, ok) in [(within, true), (over, false)] {
            for body in binary_and_xml(&value) {
                match parse(&body) {
                    Ok(v) => {
                        assert!(ok, "accepted more than {MAX_STRING_AND_DATA_BYTES} bytes");
                        // Not assert_eq!, which would print a megabyte.
                        assert!(v == value);
                    }
                    Err(e) => {
                        assert!(!ok, "refused a body within the limit: {e}");
                        assert!(e.to_string().contains("bytes of strings and data"), "{e}");
                    }
                }
            }
        }
    }

    #[test]
    fn realistic_bodies_parse_as_plist_parses_them() {
        for answer in [info_answer(), setup_session_answer(), setup_stream_answer()] {
            for body in binary_and_xml(&answer) {
                assert_eq!(parse(&body).unwrap(), plist::from_bytes::<Value>(&body).unwrap());
                assert_eq!(parse(&body).unwrap(), answer);
            }
        }
        assert!(parse(b"").is_err());
        assert!(parse(b"bplist00 truncated").is_err());
    }

    #[test]
    fn dates_and_uids_come_back_as_plain_values() {
        let xml = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
            <key>when</key><date>2026-10-15T12:00:00Z</date></dict></plist>";
        let v = parse(xml).unwrap();
        let when = v.as_dictionary().and_then(|d| d.get("when")).cloned();
        assert_eq!(when, Some(Value::String("2026-10-15T12:00:00Z".into())));
    }
}
