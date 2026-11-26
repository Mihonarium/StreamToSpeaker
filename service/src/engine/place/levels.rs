//! The level a speaker starts at. A speaker that reports its level keeps
//! it, but never above *Maximum volume from this PC*. One whose level can't
//! be read (AirPlay) starts at the level last set from this PC while it
//! played there within 3 hours, later at no more than 30, the first time
//! at 25; the maximum caps that too. A speaker reached two ways has one
//! level.

use crate::model::situation::WallTime;

use super::kept::KeptLevel;
use super::numbers::{LEVEL_FIRST, LEVEL_KEPT_WALL_MS, LEVEL_LATER_AT_MOST};

/// The level to set when the speaker starts playing, or `None` to leave it
/// as it is.
pub fn start_level(
    unreadable: bool,
    reported: Option<u8>,
    kept: Option<KeptLevel>,
    max: Option<u8>,
    now: WallTime,
) -> Option<u8> {
    let cap = |level: u8| max.map_or(level, |m| level.min(m));
    if !unreadable {
        return match (reported, max) {
            (Some(level), Some(m)) if level > m => Some(m),
            _ => None,
        };
    }
    let level = match kept {
        Some(k) if now.ms_since(k.at) <= LEVEL_KEPT_WALL_MS => k.level,
        Some(k) => k.level.min(LEVEL_LATER_AT_MOST),
        None => LEVEL_FIRST,
    };
    Some(cap(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airplay_level_after_three_hours_is_at_most_30() {
        let now = WallTime(100 * 3_600_000);
        let set = |level, hours_ago: i64| Some(KeptLevel { level, at: now.plus_ms(-hours_ago * 3_600_000) });
        assert_eq!(start_level(true, None, set(60, 1), None, now), Some(60), "within 3 hours: as it was");
        assert_eq!(start_level(true, None, set(60, 4), None, now), Some(30), "later: at most 30");
        assert_eq!(start_level(true, None, set(20, 4), None, now), Some(20), "later and lower: as it was");
        assert_eq!(start_level(true, None, None, None, now), Some(25), "the first time");
        assert_eq!(start_level(true, None, set(60, 1), Some(40), now), Some(40), "the maximum caps it");
        assert_eq!(start_level(false, Some(32), None, None, now), None, "a speaker's own level stays");
        assert_eq!(start_level(false, Some(80), None, Some(50), now), Some(50), "but not above the maximum");
        assert_eq!(start_level(false, None, None, Some(50), now), None);
    }
}
