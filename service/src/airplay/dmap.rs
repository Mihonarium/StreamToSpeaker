//! DMAP encoder for AirPlay track metadata.
//!
//! AirPlay (RAOP and AirPlay 2 alike) carries "now playing" text in a
//! `SET_PARAMETER` body typed `application/x-dmap-tagged` — the
//! DAAP/DMAP tagged-value format iTunes uses. Each item is
//! `[4-byte tag][4-byte big-endian length][value]`; the track fields are
//! wrapped in an `mlit` (listing item) container, `mikd` first, the same
//! shape OwnTone's `dmap_encode_queue_metadata` produces.

/// One DMAP item: `tag ‖ be32(len) ‖ value`.
fn item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + value.len());
    out.extend_from_slice(tag);
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// Track fields for [`now_playing_body`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrackText<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// Track length in milliseconds (`astm`), when known.
    pub duration_ms: Option<u32>,
}

/// Build the `application/x-dmap-tagged` body for a track. Empty fields
/// are omitted. Always an `mlit` container starting with `mikd` = 2
/// (music), followed by a persistent id (`mper`) derived from the text so
/// a receiver can tell one track from the next.
pub fn now_playing_body(t: &TrackText<'_>) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.extend(item(b"mikd", &[2])); // dmap.itemkind: must come first
    inner.extend(item(b"mper", &persistent_id(t).to_be_bytes())); // dmap.persistentid
    if !t.title.is_empty() {
        inner.extend(item(b"minm", t.title.as_bytes())); // dmap.itemname
    }
    if !t.artist.is_empty() {
        inner.extend(item(b"asar", t.artist.as_bytes())); // daap.songartist
    }
    if !t.album.is_empty() {
        inner.extend(item(b"asal", t.album.as_bytes())); // daap.songalbum
    }
    if let Some(ms) = t.duration_ms.filter(|&ms| ms > 0) {
        inner.extend(item(b"astm", &ms.to_be_bytes())); // daap.songtime
    }
    item(b"mlit", &inner) // dmap.listingitem
}

/// Stable 64-bit id for a track's text (FNV-1a over the fields). Equal
/// text ⇒ equal id; any change ⇒ (almost certainly) a different id.
fn persistent_id(t: &TrackText<'_>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for field in [t.title, t.artist, t.album] {
        for b in field.as_bytes().iter().chain(std::iter::once(&0u8)) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a flat run of DMAP items → (tag, value) pairs.
    fn items(mut buf: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            let tag: [u8; 4] = buf[0..4].try_into().unwrap();
            let len = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
            out.push((tag, buf[8..8 + len].to_vec()));
            buf = &buf[8 + len..];
        }
        out
    }

    #[test]
    fn item_layout_is_tag_len_value() {
        let it = item(b"minm", b"Hi");
        assert_eq!(&it[0..4], b"minm");
        assert_eq!(&it[4..8], &[0, 0, 0, 2]); // be32 length 2
        assert_eq!(&it[8..], b"Hi");
    }

    #[test]
    fn body_wraps_fields_in_mlit_with_mikd_first() {
        let body = now_playing_body(&TrackText {
            title: "Song",
            artist: "Artist",
            album: "",
            duration_ms: Some(215_000),
        });
        let outer = items(&body);
        assert_eq!(outer.len(), 1);
        assert_eq!(&outer[0].0, b"mlit");
        let inner = items(&outer[0].1);
        let tags: Vec<&[u8; 4]> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(tags, vec![b"mikd", b"mper", b"minm", b"asar", b"astm"]);
        assert_eq!(inner[0].1, vec![2]);
        assert_eq!(inner[1].1.len(), 8);
        assert_eq!(inner[2].1, b"Song");
        assert_eq!(inner[3].1, b"Artist");
        assert_eq!(inner[4].1, 215_000u32.to_be_bytes());
    }

    #[test]
    fn utf8_text_is_sent_verbatim_with_byte_lengths() {
        let body = now_playing_body(&TrackText {
            title: "Ünïcødé — 曲",
            ..Default::default()
        });
        let inner = items(&items(&body)[0].1);
        let minm = inner.iter().find(|(t, _)| t == b"minm").unwrap();
        assert_eq!(minm.1, "Ünïcødé — 曲".as_bytes());
    }

    #[test]
    fn blank_fields_and_unknown_duration_are_omitted() {
        let body = now_playing_body(&TrackText::default());
        let inner = items(&items(&body)[0].1);
        let tags: Vec<&[u8; 4]> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(tags, vec![b"mikd", b"mper"]);
        let zero = now_playing_body(&TrackText { duration_ms: Some(0), ..Default::default() });
        assert!(!zero.windows(4).any(|w| w == b"astm"));
    }

    #[test]
    fn persistent_id_tracks_text() {
        let a = TrackText { title: "A", artist: "X", ..Default::default() };
        let b = TrackText { title: "B", artist: "X", ..Default::default() };
        // Field boundaries matter: ("AB","") ≠ ("A","B").
        let c = TrackText { title: "AB", ..Default::default() };
        let d = TrackText { title: "A", artist: "B", ..Default::default() };
        assert_eq!(persistent_id(&a), persistent_id(&a.clone()));
        assert_ne!(persistent_id(&a), persistent_id(&b));
        assert_ne!(persistent_id(&c), persistent_id(&d));
    }
}
