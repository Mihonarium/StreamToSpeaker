//! Every number and fixed choice the behaviour uses, in one place. Times
//! in `_MS` are on the monotonic clock unless the name ends in `_WALL_MS`:
//! those are on the wall clock, because they must run across sleep,
//! power-off and restarts.

/// How long the app tries again after the place's speaker stopped
/// responding during play; without an answer it then moves the sound to
/// this PC. Nothing is paused meanwhile, so each second of trying is a
/// second of sound nobody hears. The other value, `20_000`, gives a speaker
/// (or a router) that restarts twice as long to come back, at the price of
/// twice the silence; the tries below keep their proportions.
pub const TRIES_DURING_PLAY_MS: u64 = 10_000;

/// The tries, in ms after the speaker stopped: a tenth, about a third and
/// most of the way through the time above (about 1, 3.5 and 8.5 seconds of
/// 10). A try still running when the next is due counts as failed and gives
/// way to it.
pub const TRIES_AT_MS: [u64; 3] =
    [TRIES_DURING_PLAY_MS / 10, TRIES_DURING_PLAY_MS * 7 / 20, TRIES_DURING_PLAY_MS * 17 / 20];

/// Where the sound goes when headphones that took it from a speaker go
/// away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterHeadphones {
    /// It stays on this PC, on its other output, and a note offers the way
    /// back to the speaker.
    StayOnThisPc,
    /// The speaker is the place again without anyone switching: Windows
    /// goes back to the app's device, and what plays connects the speaker
    /// as any start of sound does. No note.
    BackToSpeaker,
}

/// `StayOnThisPc`: the app never moves the sound to a speaker by itself.
/// The other value, `BackToSpeaker`, sends what plays in the headphones to
/// the speaker the moment they are switched off.
pub const AFTER_HEADPHONES: AfterHeadphones = AfterHeadphones::StayOnThisPc;

/// What the place is after sound started and the place's speaker didn't
/// respond.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterFailedStart {
    /// This PC: the sound is there, and stays there until a person
    /// switches.
    ThisPc,
    /// The speaker stays the place. Windows stays on the app's device, and
    /// what plays is passed on to the PC's own output for as long as it
    /// plays; the next start of sound tries the speaker again. The place is
    /// then not where the sound is: the situation names the speaker, with
    /// the reason it can't play, while sound plays on this PC.
    Speaker,
}

/// `ThisPc`: the place is always where the sound is. The other value,
/// `Speaker`, lets a speaker that was only off for a while play the next
/// sound again without a click.
pub const PLACE_AFTER_FAILED_START: AfterFailedStart = AfterFailedStart::ThisPc;

/// Whether a person's switch to a speaker that plays something else (or
/// may be in use) asks before it stops what plays there. `false`: it
/// doesn't; the speaker's row and the note say what plays there before the
/// click, and a switch from any surface takes the speaker over. The other
/// value, `true`, puts a question after the click (in the panel, or by the
/// clock for the tray and Windows' list) and refuses the switch from a
/// surface that can't ask. A test chime on a busy speaker asks either way.
pub const ASK_BEFORE_TAKING_A_BUSY_SPEAKER: bool = false;

/// Whether a switch that is still connecting can be cancelled by itself
/// (*Cancel* under the words that say it connects): the attempt is dropped
/// and the sound stays where it is. The other value, `false`, leaves no
/// such command: the situation doesn't offer it and the core ignores it.
/// Either way a click on the row of the place drops the attempt, and a
/// newer switch takes its place.
pub const CANCEL_WHILE_CONNECTING: bool = true;

/// How long the place's speaker gets to be ready when sound starts and it
/// isn't connected: the length of the tries during play, so that the
/// engine has one number. An answer that it can't play (it plays something
/// else, asks for its code, isn't found, refuses) ends the try at once;
/// this is the limit for a speaker that is still getting ready or stays
/// silent. Nothing is held meanwhile, so each second is a second of sound
/// nobody hears. The other value, `2_000`, moves the sound to this PC
/// sooner when the speaker is off, at the price of calling a speaker that
/// takes longer than that to wake (a TV, a receiver, a speaker on slow
/// Wi-Fi) not responding each time sound starts after a rest.
pub const START_TRY_MS: u64 = TRIES_DURING_PLAY_MS;

/// Sound started and the place's speaker has to be connected first: once
/// it was asked this long ago and hasn't answered anything, its row and
/// the note say that it isn't responding, and the note offers This PC at
/// once. A speaker that answered and is still getting ready goes on
/// reading *Connecting…*. The wait for this PC's network is said the same
/// way, this long after the sound started.
pub const SLOW_CONNECT_NOTE_MS: u64 = 2_000;

/// While this PC has no network in the minute after a wake or a start, or
/// just after its network changed, the try at a sound's start waits this
/// long at most for a connection: the length of the tries again. Nothing
/// is held meanwhile.
pub const NETWORK_WAIT_MS: u64 = TRIES_DURING_PLAY_MS;

/// After this long with nothing playing the app takes its stream off the
/// speaker (where the speaker lets it), and connects again when sound
/// starts.
pub const STREAM_REST_AFTER_WALL_MS: i64 = 30 * 60_000;

/// The Settings option *Switch back to this PC after 30 minutes without
/// sound*.
pub const SWITCH_BACK_AFTER_WALL_MS: i64 = 30 * 60_000;

/// A break: this long or more in which nothing played and nobody switched.
/// Sound that starts on a speaker after one brings **Playing on …**.
pub const BREAK_WALL_MS: i64 = 10 * 60_000;

/// Real sound lasts more than this.
pub const REAL_SOUND_MS: u64 = 3_000;

/// A sound that ends within this, where the app would first have to
/// connect the speaker, doesn't connect it: it is played on the PC's own
/// output once it has ended.
pub const SHORT_SOUND_MS: u64 = 3_000;

/// −50 dBFS as a linear peak: sound below it is no sound.
pub const LEVEL_THRESHOLD: f32 = 0.003_162_277_7;

/// A run of sound ends after this much quiet (a pause between two words of
/// a notification's sound doesn't end it).
pub const SOUND_GAP_MS: u64 = 1_000;

/// Real sound counts as playing until this much quiet (the gap between two
/// songs doesn't make it *Nothing playing*).
pub const REAL_ENDS_AFTER_MS: u64 = 5_000;

/// A sample of the levels covers at most this much time, so a late sample
/// doesn't count as seconds of sound.
pub const MAX_SAMPLE_MS: u64 = 500;

/// For a minute after a wake or a start the app calls no speaker away on
/// its own, and the try at a sound's start waits for this PC's network.
pub const AFTER_WAKE_MS: u64 = 60_000;

/// "Just after this PC's network changed", for the same wait.
pub const AFTER_NETWORK_CHANGE_MS: u64 = 60_000;

/// A change of Windows' output within about this long of an output coming
/// or going is Windows' doing, not a person's switch. Headphones that
/// Windows doesn't switch to within it are switched to by the app.
pub const WINDOWS_DOING_MS: u64 = 2_000;

/// The watcher gives up when the app stops again within this after it
/// started it.
pub const WATCHER_GIVES_UP_WITHIN_MS: u64 = 60_000;

/// A level set from this PC is saved once it has stood this long: a slider
/// on its way isn't written at every step.
pub const LEVEL_SAVED_AFTER_MS: u64 = 2_000;

/// A speaker whose level can't be read starts at the level last set from
/// this PC while it played there within this long…
pub const LEVEL_KEPT_WALL_MS: i64 = 3 * 3_600_000;
/// …and later at no more than this.
pub const LEVEL_LATER_AT_MOST: u8 = 30;
/// The first time, with no level set from this PC yet.
pub const LEVEL_FIRST: u8 = 25;

/// A pop-up by the clock shows this long…
pub const POPUP_MS: u64 = 8_000;
/// …and this long when it has a button.
pub const POPUP_WITH_BUTTON_MS: u64 = 15_000;

/// A confirmation by the clock waits this long at most.
pub const CONFIRMATION_POPUP_MS: u64 = 30 * 60_000;

/// This PC is on another network when the speaker isn't found on its new
/// connections within this.
pub const OTHER_NETWORK_AFTER_MS: u64 = 60_000;

/// At a start, the app looks this long at what plays on this PC before it
/// moves Windows to its device; an app above the threshold in that time
/// counts as playing.
pub const START_LOOK_MS: u64 = 3_000;

/// After the app moved the sound off a speaker it asks it this often
/// whether it can play again…
pub const LISTEN_FAST_EVERY_MS: u64 = 15_000;
/// …for this long…
pub const LISTEN_FAST_FOR_MS: u64 = 2 * 60_000;
/// …then this often…
pub const LISTEN_SLOW_EVERY_MS: u64 = 60_000;
/// …until this long after the move (announcements are heard throughout).
pub const LISTEN_FOR_MS: u64 = 30 * 60_000;

/// A connection this PC made within this before a speaker stopped
/// responding is named with it (*This PC connected to "Corp VPN" at
/// 21:14.*).
pub const VPN_NAMED_WITHIN_MS: u64 = 60_000;

/// How many outputs other than the app's device the app remembers, most
/// recent first, to find the PC's own output.
pub const OUTPUTS_REMEMBERED: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    /// Each number as the behaviour describes it.
    #[test]
    fn each_number_is_what_the_behaviour_needs() {
        let s = 1_000;
        let min = 60 * s;
        assert_eq!(TRIES_DURING_PLAY_MS, 10 * s, "about 10 seconds of tries");
        assert_eq!(TRIES_AT_MS, [s, 3_500, 8_500], "tries at about 1, 3.5 and 8.5 s");
        assert!(TRIES_AT_MS[2] < TRIES_DURING_PLAY_MS, "the last try has time to answer");
        assert_eq!(AFTER_HEADPHONES, AfterHeadphones::StayOnThisPc);
        assert_eq!(PLACE_AFTER_FAILED_START, AfterFailedStart::ThisPc);
        assert_eq!(START_TRY_MS, TRIES_DURING_PLAY_MS, "a start of sound gets as long as the tries");
        assert_eq!(SLOW_CONNECT_NOTE_MS, 2 * s, "two seconds without an answer");
        assert!(SLOW_CONNECT_NOTE_MS < START_TRY_MS, "the note speaks before the try ends");
        assert!(!ASK_BEFORE_TAKING_A_BUSY_SPEAKER, "a busy speaker is taken without a question");
        assert!(CANCEL_WHILE_CONNECTING, "a switch that connects can be cancelled");
        assert_eq!(NETWORK_WAIT_MS, TRIES_DURING_PLAY_MS, "the wait for the network is the length of the tries");
        assert_eq!(STREAM_REST_AFTER_WALL_MS, 30 * min as i64);
        assert_eq!(SWITCH_BACK_AFTER_WALL_MS, 30 * min as i64);
        assert_eq!(BREAK_WALL_MS, 10 * min as i64);
        assert_eq!(REAL_SOUND_MS, 3 * s);
        assert_eq!(SHORT_SOUND_MS, 3 * s);
        assert!((20.0 * (LEVEL_THRESHOLD as f64).log10() + 50.0).abs() < 1e-3, "−50 dBFS");
        assert_eq!(AFTER_WAKE_MS, min, "a minute after a wake or a start");
        assert_eq!(WINDOWS_DOING_MS, 2 * s, "about two seconds");
        assert_eq!(WATCHER_GIVES_UP_WITHIN_MS, min);
        assert_eq!(LEVEL_KEPT_WALL_MS, 3 * 60 * min as i64);
        assert_eq!(LEVEL_LATER_AT_MOST, 30);
        assert!(LEVEL_FIRST <= LEVEL_LATER_AT_MOST);
        assert_eq!((POPUP_MS, POPUP_WITH_BUTTON_MS), (8 * s, 15 * s));
        assert_eq!(CONFIRMATION_POPUP_MS, 30 * min);
        assert_eq!(OTHER_NETWORK_AFTER_MS, min);
        assert_eq!(START_LOOK_MS, 3 * s);
        assert_eq!((LISTEN_FAST_EVERY_MS, LISTEN_FAST_FOR_MS), (15 * s, 2 * min));
        assert_eq!((LISTEN_SLOW_EVERY_MS, LISTEN_FOR_MS), (min, 30 * min));
        assert!(SOUND_GAP_MS < SHORT_SOUND_MS && REAL_ENDS_AFTER_MS > SOUND_GAP_MS);
    }
}
