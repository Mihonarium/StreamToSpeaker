//! Minimal DMAP encoder for RAOP track metadata.
//!
//! RAOP carries "now playing" text in a `SET_PARAMETER` body typed
//! `application/x-dmap-tagged` — the DAAP/DMAP tagged-value format iTunes
//! uses. Each item is `[4-byte tag][4-byte big-endian length][value]`;
//! iTunes wraps the track fields in an `mlit` (listing item) container.
//! We only emit the three fields a speaker display actually shows.

/// One DMAP item: `tag ‖ be32(len) ‖ value`.
fn item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + value.len());
    out.extend_from_slice(tag);
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// Build the `application/x-dmap-tagged` body for a track. Empty fields
/// are omitted. Returns an `mlit` container (empty if all fields blank).
pub fn now_playing_body(title: &str, artist: &str, album: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    if !title.is_empty() {
        inner.extend(item(b"minm", title.as_bytes())); // dmap.itemname
    }
    if !artist.is_empty() {
        inner.extend(item(b"asar", artist.as_bytes())); // daap.songartist
    }
    if !album.is_empty() {
        inner.extend(item(b"asal", album.as_bytes())); // daap.songalbum
    }
    item(b"mlit", &inner) // dmap.listingitem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_layout_is_tag_len_value() {
        let it = item(b"minm", b"Hi");
        assert_eq!(&it[0..4], b"minm");
        assert_eq!(&it[4..8], &[0, 0, 0, 2]); // be32 length 2
        assert_eq!(&it[8..], b"Hi");
    }

    #[test]
    fn body_wraps_fields_in_mlit() {
        let body = now_playing_body("Song", "Artist", "");
        assert_eq!(&body[0..4], b"mlit");
        // outer length = 2 items × (8 header + payload): minm(8+4) + asar(8+6)
        let outer_len = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        assert_eq!(outer_len as usize, body.len() - 8);
        // album omitted → no asal tag present
        assert!(!body.windows(4).any(|w| w == b"asal"));
        assert!(body.windows(4).any(|w| w == b"minm"));
        assert!(body.windows(4).any(|w| w == b"asar"));
    }

    #[test]
    fn all_blank_is_empty_container() {
        let body = now_playing_body("", "", "");
        assert_eq!(&body[0..4], b"mlit");
        assert_eq!(&body[4..8], &[0, 0, 0, 0]);
    }
}
