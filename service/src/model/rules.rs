//! The decisions that need no I/O, so the engine only supplies facts: when
//! the app moves sound by itself, how a dropout is retried, which changes
//! notify, and whose volume the controls move.

use super::command::VolumeOwner;
use super::facts::{ComesBack, NotificationKind};
use super::state::{
    AfterTries, Dropout, FellBackCause, Looking, NowState, PcOutputId, Return, Secret, SoundAt,
    StayedBecause,
};

/// The quick tries after a speaker stops responding, in ms after the drop.
pub const QUICK_TRIES_AT_MS: [u64; 3] = [2_000, 5_000, 10_000];
/// Without an answer by then, the sound comes back to this PC.
pub const FALL_BACK_AFTER_MS: u64 = 15_000;
/// "Keep trying, and stay silent" tries this often after the quick tries.
pub const KEEP_TRYING_EVERY_MS: u64 = 30_000;
/// At sign-in, wake or start, *Looking for Kitchen…* becomes *Can't find
/// Kitchen right now* after this long.
pub const CANT_FIND_AFTER_MS: u64 = 10_000;
/// The speaker is released this long after Windows switched to another
/// output (kept if Windows switches back meanwhile).
pub const RELEASE_AFTER_WINDOWS_SWITCH_MS: u64 = 10_000;
/// The search runs this often while the panel or Settings is open…
pub const SEARCH_EVERY_MS: u64 = 10_000;
/// …and this often otherwise.
pub const SEARCH_IN_BACKGROUND_EVERY_MS: u64 = 5 * 60_000;
/// A delay change that applies by reconnecting waits this long after the
/// last press, so several presses cost one reconnect.
pub const DELAY_RECONNECT_AFTER_MS: u64 = 1_500;
/// A further reconnect starts at least this long after the previous one
/// began.
pub const DELAY_RECONNECT_GAP_MS: u64 = 5_000;
/// After a removal, a speaker that runs dry within this long gets some back.
pub const DELAY_DRY_WINDOW_MS: u64 = 30_000;
/// What goes back when it does.
pub const DELAY_DRY_BACK_MS: u32 = 50;
/// The confirmation of a delay shortcut shows this long.
pub const CONFIRMATION_MS: u64 = 2_000;
/// At most one notification per speaker in this time (except a speaker
/// coming back, which replaces the one about it leaving).
pub const NOTIFY_GAP_MS: u64 = 30 * 60_000;
/// A speaker back within this long after a fallback notifies.
pub const BACK_NOTIFY_WITHIN_MS: u64 = 60 * 60_000;
/// The level an AirPlay speaker gets the first time (its level can't be
/// read).
pub const FIRST_AIRPLAY_LEVEL: u8 = 25;

/// Whose volume the Now card's slider moves in `state` (`None`: no volume
/// row). `pc_output` is This PC's output.
pub fn volume_owner(state: &NowState, pc_output: &PcOutputId) -> Option<VolumeOwner> {
    use NowState::*;
    let pc = || Some(VolumeOwner::PcOutput(pc_output.clone()));
    match state {
        OnThisPc | FellBack { .. } | SpeakerBack { .. } | Taken { .. } | NeedsSecret { .. }
        | LookingFor { .. } | WindowsSwitched { .. } | MaAway { .. } | MaBack | OutputMissing => pc(),
        Connecting { from, .. } => match from {
            SoundAt::ThisPc => pc(),
            SoundAt::Speaker(id) => Some(VolumeOwner::Speaker(id.clone())),
            SoundAt::MusicAssistant => Some(VolumeOwner::MusicAssistant),
        },
        Playing { speaker, .. }
        | ApplyingDelay { speaker, .. }
        | Ready { speaker }
        | ReadyPlayingElsewhere { speaker, .. }
        | NotTaking { speaker, .. }
        | WindowsRefused { speaker }
        | PlayingSentApps { speaker, .. }
        | ReadyNoAppSent { speaker } => Some(VolumeOwner::Speaker(speaker.clone())),
        MaPlaying { .. } => Some(VolumeOwner::MusicAssistant),
        NotResponding { .. } | ChooseSpeaker | MaWaiting => None,
    }
}

/// The level a speaker plays at when it connects. Connecting never makes a
/// speaker louder: a speaker that reports its level keeps it (and Windows'
/// slider moves to it); one that can't (AirPlay) gets the level last used
/// with it, the first time [`FIRST_AIRPLAY_LEVEL`].
pub fn level_on_connect(reported: Option<u8>, last_used: Option<u8>) -> u8 {
    reported.or(last_used).unwrap_or(FIRST_AIRPLAY_LEVEL)
}

/// A level with the speaker's volume limit applied.
pub fn within_limit(level: u8, limit: Option<u8>) -> u8 {
    match limit {
        Some(limit) => level.min(limit),
        None => level,
    }
}

/// Whether the app would go back to the chosen place by itself once it is
/// available again, for the sentences that promise it.
pub fn returns(comes_back: ComesBack, played_here_since: bool) -> Return {
    if comes_back != ComesBack::Never && !played_here_since {
        Return::ByItself
    } else {
        Return::WillAsk
    }
}

/// What made the app consider moving the sound by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The speaker the sound fell back from answers again.
    BackAfterDropout,
    /// Sign-in, wake from sleep, app start (also after Quit).
    SessionStart,
    /// The app reopened after an update the person started.
    BackAfterUpdate,
    /// A speaker set to *Whenever it appears and nothing else is playing*
    /// appeared.
    Appeared,
    /// Music Assistant, the chosen place, connected again.
    MusicAssistantBack,
    /// Someone else started something on the speaker being played on.
    TakenBySomeoneElse,
}

/// The facts a return depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveFacts {
    pub found: bool,
    /// It says it is playing something else.
    pub busy: bool,
    /// It wants a code or password before it plays.
    pub needs: Option<Secret>,
    /// Something played on this PC since the fallback, sign-in, wake or
    /// start (see [`PlayedDetector`]).
    pub played_here_since: bool,
    /// Something is playing on this PC right now.
    pub playing_here_now: bool,
    /// General → *…and go back to the speaker I was using*.
    pub go_back_at_start: bool,
    /// The speaker's *Comes back by itself*.
    pub comes_back: ComesBack,
    /// After an update: Windows' output is still the one handed back.
    pub output_unchanged: bool,
}

/// What the app does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMove {
    /// Play there, without asking.
    Return,
    /// Stay on this PC and offer the place (the speaker is back).
    Offer(StayedBecause),
    /// Keep looking for it (*Looking for Kitchen…*).
    KeepLooking,
    /// Stop and ask for the code or password; never open a dialog by itself.
    AskForSecret(Secret),
    /// It is busy: stay on this PC and say so; never take it.
    StepAside,
    /// The person picked another output in Windows meanwhile; follow it.
    FollowWindows,
    /// Nothing to do.
    Stay,
}

/// When the app may move sound by itself: only to rescue it, or to restore
/// the person's own choice when nobody is listening to anything else here.
pub fn auto_move(trigger: Trigger, f: MoveFacts) -> AutoMove {
    use AutoMove::*;
    let reachable = || {
        if !f.found {
            Some(KeepLooking)
        } else if f.busy {
            Some(StepAside)
        } else {
            f.needs.map(AskForSecret)
        }
    };
    match trigger {
        Trigger::TakenBySomeoneElse => StepAside,
        Trigger::BackAfterDropout => {
            if let Some(m) = reachable() {
                m
            } else if f.comes_back == ComesBack::Never {
                Offer(StayedBecause::SetNotToReturn)
            } else if f.played_here_since {
                Offer(StayedBecause::PlayedAfterDropout)
            } else {
                Return
            }
        }
        Trigger::SessionStart => {
            if !f.go_back_at_start || f.comes_back == ComesBack::Never {
                Stay
            } else if let Some(m) = reachable() {
                m
            } else if f.played_here_since {
                Offer(StayedBecause::PlayingWhenFound)
            } else {
                Return
            }
        }
        Trigger::BackAfterUpdate => {
            if !f.output_unchanged {
                FollowWindows
            } else {
                reachable().unwrap_or(Return)
            }
        }
        Trigger::Appeared => {
            if f.comes_back != ComesBack::WheneverAppears || f.playing_here_now || f.busy {
                Stay
            } else if let Some(secret) = f.needs {
                AskForSecret(secret)
            } else {
                Return
            }
        }
        Trigger::MusicAssistantBack => {
            if f.played_here_since {
                Offer(StayedBecause::PlayedAfterDropout)
            } else {
                Return
            }
        }
    }
}

/// The next step after a speaker stopped responding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryStep {
    /// Try again at `at_ms` after the drop.
    TryAt { attempt: u8, at_ms: u64 },
    /// Give the sound back to this PC.
    FallBack,
    /// Stay silent and try every [`KEEP_TRYING_EVERY_MS`].
    KeepTrying,
}

/// The next step, `elapsed_ms` after the drop, with `failed` tries behind.
/// An AirPlay attempt that stalled mid-setup ends the quick tries: a Sonos
/// then refuses new sessions for tens of seconds.
pub fn next_retry(elapsed_ms: u64, failed: u8, stalled_mid_setup: bool, then: AfterTries) -> RetryStep {
    let tries_over = stalled_mid_setup
        || failed as usize >= QUICK_TRIES_AT_MS.len()
        || elapsed_ms >= FALL_BACK_AFTER_MS;
    if tries_over {
        return match then {
            AfterTries::BackToPc => RetryStep::FallBack,
            AfterTries::StaySilent => RetryStep::KeepTrying,
        };
    }
    RetryStep::TryAt { attempt: failed + 1, at_ms: QUICK_TRIES_AT_MS[failed as usize].max(elapsed_ms) }
}

/// The dropout as the state shows it.
pub fn dropout(failed: u8, tries_over: bool, then: AfterTries) -> Dropout {
    if tries_over && then == AfterTries::StaySilent {
        Dropout::KeepTrying { every_secs: (KEEP_TRYING_EVERY_MS / 1000) as u16 }
    } else {
        let of = QUICK_TRIES_AT_MS.len() as u8;
        Dropout::QuickTries { attempt: (failed + 1).min(of), of, then }
    }
}

/// What the notification rules need besides the change of state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyContext {
    pub now_ms: u64,
    /// Something was playing on this PC when it happened.
    pub was_playing: bool,
    /// The panel or the Settings window is in front (the same words show
    /// there).
    pub panel_or_settings_in_front: bool,
    /// General → *Tell me when sound leaves the place I chose*.
    pub tell_me: bool,
    /// The last notification about this speaker, and when.
    pub last_for_speaker: Option<(NotificationKind, u64)>,
    /// When the sound fell back from this speaker, if it did.
    pub fell_back_at_ms: Option<u64>,
}

/// Whether the change from `before` to `after` notifies, and with what.
/// Notifications only say what happened to the sound while the person
/// wasn't looking; never a success, never twice in half an hour about one
/// speaker.
pub fn notification_for(before: &NowState, after: &NowState, ctx: NotifyContext) -> Option<NotificationKind> {
    use NotificationKind as N;
    use NowState::*;
    if ctx.panel_or_settings_in_front {
        return None;
    }
    let kind = match (before, after) {
        (OutputMissing, OutputMissing) => return None,
        (_, OutputMissing) => return Some(N::OutputMissing),
        (FellBack { speaker: a, .. }, SpeakerBack { speaker: b, because: StayedBecause::PlayedAfterDropout })
            if a == b =>
        {
            let within = ctx.fell_back_at_ms.is_some_and(|t| ctx.now_ms.saturating_sub(t) <= BACK_NOTIFY_WITHIN_MS);
            if !within {
                return None;
            }
            N::SpeakerBack
        }
        (FellBack { .. }, FellBack { .. }) => return None,
        (_, FellBack { cause: FellBackCause::StoppedResponding, .. }) => {
            if !ctx.was_playing {
                return None;
            }
            N::StoppedResponding
        }
        // The remedy is pairing, so it says that, whatever was playing.
        (_, FellBack { cause: FellBackCause::RefusedPairing, .. }) => N::NeedsSecret(Secret::Code),
        (
            NotResponding { dropout: Dropout::QuickTries { .. }, .. },
            NotResponding { dropout: Dropout::KeepTrying { .. }, .. },
        ) => {
            if !ctx.was_playing {
                return None;
            }
            N::StoppedRespondingSilent
        }
        (Taken { .. }, Taken { .. }) => return None,
        (_, Taken { .. }) => {
            if !ctx.was_playing {
                return None;
            }
            N::Taken
        }
        (NeedsSecret { .. }, NeedsSecret { .. }) => return None,
        (_, NeedsSecret { secret, .. }) => N::NeedsSecret(*secret),
        _ => return None,
    };
    if !ctx.tell_me {
        return None;
    }
    if let Some((last, at)) = ctx.last_for_speaker {
        let recent = ctx.now_ms.saturating_sub(at) < NOTIFY_GAP_MS;
        let replaces = kind == N::SpeakerBack && last == N::StoppedResponding;
        if recent && !replaces {
            return None;
        }
    }
    Some(kind)
}

/// Says whether anything has played on this PC since a moment (a
/// fallback, sign-in, wake or start): an audio session counts once it has
/// been above −50 dBFS for at least 2 s within any 10 s. Windows' system
/// sounds never count, so a notification's ding doesn't stop a return.
///
/// The engine feeds it every session's peak about every 250 ms while a
/// return is pending.
#[derive(Clone, Debug, Default)]
pub struct PlayedDetector {
    /// (time, ms above the threshold since the previous sample).
    loud: std::collections::VecDeque<(u64, u64)>,
    last_ms: Option<u64>,
    played: bool,
}

impl PlayedDetector {
    /// −50 dBFS as a linear peak.
    pub const THRESHOLD: f32 = 0.003_162_277_7;
    pub const NEEDS_MS: u64 = 2_000;
    pub const WINDOW_MS: u64 = 10_000;
    /// A sample covers at most this much time (a late poll doesn't count
    /// as seconds of sound).
    pub const MAX_STEP_MS: u64 = 500;

    pub fn new() -> Self {
        Self::default()
    }

    /// Starts over (at a fallback, sign-in, wake or start).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// One sample: the loudest peak among this PC's sessions at `now_ms`,
    /// system sounds left out by the caller (`IsSystemSoundsSession`).
    pub fn observe(&mut self, now_ms: u64, peak: f32) -> bool {
        let step = match self.last_ms {
            Some(last) => now_ms.saturating_sub(last).min(Self::MAX_STEP_MS),
            None => 0,
        };
        self.last_ms = Some(now_ms);
        if peak > Self::THRESHOLD && step > 0 {
            self.loud.push_back((now_ms, step));
        }
        while self.loud.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) >= Self::WINDOW_MS) {
            self.loud.pop_front();
        }
        if self.loud.iter().map(|(_, d)| d).sum::<u64>() >= Self::NEEDS_MS {
            self.played = true;
        }
        self.played
    }

    pub fn played(&self) -> bool {
        self.played
    }
}

/// The state while looking for a speaker, `elapsed_ms` after sign-in, wake
/// or start.
pub fn looking(elapsed_ms: u64, comes_back: ComesBack, played_here_since: bool) -> Looking {
    if elapsed_ms < CANT_FIND_AFTER_MS {
        Looking::Searching
    } else {
        Looking::NotFound(returns(comes_back, played_here_since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::state::{Apps, SpeakerId};
    use crate::model::text::LocalTime;

    fn facts() -> MoveFacts {
        MoveFacts {
            found: true,
            busy: false,
            needs: None,
            played_here_since: false,
            playing_here_now: false,
            go_back_at_start: true,
            comes_back: ComesBack::LastChosen,
            output_unchanged: true,
        }
    }

    /// Who moves the sound, and when: every case, row by row.
    #[test]
    fn who_moves_the_sound() {
        use AutoMove::*;
        use Trigger::*;
        let f = facts();
        let cases: &[(&str, Trigger, MoveFacts, AutoMove)] = &[
            ("back, nothing played since: returns", BackAfterDropout, f, Return),
            ("back, busy: never takes it", BackAfterDropout, MoveFacts { busy: true, ..f }, StepAside),
            (
                "back, something played since: offers",
                BackAfterDropout,
                MoveFacts { played_here_since: true, ..f },
                Offer(StayedBecause::PlayedAfterDropout),
            ),
            (
                "back, set never to come back: offers",
                BackAfterDropout,
                MoveFacts { comes_back: ComesBack::Never, ..f },
                Offer(StayedBecause::SetNotToReturn),
            ),
            ("sign-in: returns to the last speaker", SessionStart, f, Return),
            ("sign-in, not found yet: keeps looking", SessionStart, MoveFacts { found: false, ..f }, KeepLooking),
            (
                "sign-in, something played: offers",
                SessionStart,
                MoveFacts { played_here_since: true, ..f },
                Offer(StayedBecause::PlayingWhenFound),
            ),
            ("sign-in, go-back switch off: stays", SessionStart, MoveFacts { go_back_at_start: false, ..f }, Stay),
            ("sign-in, busy: steps aside", SessionStart, MoveFacts { busy: true, ..f }, StepAside),
            (
                "sign-in, needs a code: asks, never pops a dialog",
                SessionStart,
                MoveFacts { needs: Some(Secret::Code), ..f },
                AskForSecret(Secret::Code),
            ),
            (
                "after an update: returns whatever played",
                BackAfterUpdate,
                MoveFacts { played_here_since: true, playing_here_now: true, ..f },
                Return,
            ),
            ("after an update, busy", BackAfterUpdate, MoveFacts { busy: true, ..f }, StepAside),
            ("after an update, not found", BackAfterUpdate, MoveFacts { found: false, ..f }, KeepLooking),
            (
                "after an update, another output picked",
                BackAfterUpdate,
                MoveFacts { output_unchanged: false, ..f },
                FollowWindows,
            ),
            (
                "whenever it appears, nothing playing: plays there",
                Appeared,
                MoveFacts { comes_back: ComesBack::WheneverAppears, ..f },
                Return,
            ),
            (
                "whenever it appears, something playing: stays",
                Appeared,
                MoveFacts { comes_back: ComesBack::WheneverAppears, playing_here_now: true, ..f },
                Stay,
            ),
            (
                "whenever it appears, busy: stays",
                Appeared,
                MoveFacts { comes_back: ComesBack::WheneverAppears, busy: true, ..f },
                Stay,
            ),
            ("appears, not set to: stays", Appeared, f, Stay),
            ("someone else takes it: never takes it back", TakenBySomeoneElse, f, StepAside),
            ("Music Assistant back, nothing played", MusicAssistantBack, f, Return),
            (
                "Music Assistant back, something played",
                MusicAssistantBack,
                MoveFacts { played_here_since: true, ..f },
                Offer(StayedBecause::PlayedAfterDropout),
            ),
        ];
        for (what, trigger, facts, want) in cases {
            assert_eq!(auto_move(*trigger, *facts), *want, "{what}");
        }
    }

    #[test]
    fn three_quick_tries_then_back_to_this_pc() {
        use RetryStep::*;
        let b = AfterTries::BackToPc;
        assert_eq!(next_retry(0, 0, false, b), TryAt { attempt: 1, at_ms: 2_000 });
        assert_eq!(next_retry(2_500, 1, false, b), TryAt { attempt: 2, at_ms: 5_000 });
        assert_eq!(next_retry(6_000, 2, false, b), TryAt { attempt: 3, at_ms: 10_000 });
        assert_eq!(next_retry(11_000, 3, false, b), FallBack);
        assert_eq!(next_retry(15_000, 2, false, b), FallBack, "no answer within 15 s");
        assert_eq!(next_retry(3_000, 1, true, b), FallBack, "a stalled setup ends the tries");
        assert_eq!(next_retry(11_000, 3, false, AfterTries::StaySilent), KeepTrying);
        assert_eq!(
            dropout(1, false, b),
            Dropout::QuickTries { attempt: 2, of: 3, then: b },
            "the second try reads (2 of 3)"
        );
        assert_eq!(dropout(3, true, AfterTries::StaySilent), Dropout::KeepTrying { every_secs: 30 });
        assert_eq!(FALL_BACK_AFTER_MS, 15_000);
    }

    fn ctx() -> NotifyContext {
        NotifyContext {
            now_ms: 10 * 60 * 60_000,
            was_playing: true,
            panel_or_settings_in_front: false,
            tell_me: true,
            last_for_speaker: None,
            fell_back_at_ms: None,
        }
    }

    /// Which changes bring a notification, and which bring none.
    #[test]
    fn which_changes_notify() {
        use NotificationKind as N;
        let k = SpeakerId::new("kitchen");
        let t = LocalTime::new(21, 14);
        let retrying = |then| NowState::NotResponding {
            speaker: k.clone(),
            dropout: Dropout::QuickTries { attempt: 3, of: 3, then },
        };
        let fell_back = NowState::FellBack {
            speaker: k.clone(),
            at: t,
            cause: FellBackCause::StoppedResponding,
            returns: Return::WillAsk,
        };
        let back = NowState::SpeakerBack { speaker: k.clone(), because: StayedBecause::PlayedAfterDropout };
        let playing = NowState::Playing { speaker: k.clone(), apps: Apps::of(&["Spotify"]) };
        let taken = NowState::Taken { speaker: k.clone(), at: t };
        let pairing = NowState::NeedsSecret { speaker: k.clone(), secret: Secret::Code };
        let silent = NowState::NotResponding { speaker: k.clone(), dropout: Dropout::KeepTrying { every_secs: 30 } };
        let c = ctx();
        let refused =
            NowState::FellBack { speaker: k.clone(), at: t, cause: FellBackCause::RefusedPairing, returns: Return::WillAsk };
        let retrying_back = retrying(AfterTries::BackToPc);
        let retrying_silent = retrying(AfterTries::StaySilent);
        let cases: Vec<(&str, &NowState, &NowState, NotifyContext, Option<N>)> = vec![
            ("the tries failed while playing", &retrying_back, &fell_back, c, Some(N::StoppedResponding)),
            (
                "nothing when nothing was playing",
                &retrying_back,
                &fell_back,
                NotifyContext { was_playing: false, ..c },
                None,
            ),
            ("the tries failed with stay silent chosen", &retrying_silent, &silent, c, Some(N::StoppedRespondingSilent)),
            ("taken while playing", &playing, &taken, c, Some(N::Taken)),
            (
                "back within the hour replaces stopped responding",
                &fell_back,
                &back,
                NotifyContext {
                    fell_back_at_ms: Some(c.now_ms - 5 * 60_000),
                    last_for_speaker: Some((N::StoppedResponding, c.now_ms - 5 * 60_000)),
                    ..c
                },
                Some(N::SpeakerBack),
            ),
            (
                "nothing when it is back after more than an hour",
                &fell_back,
                &back,
                NotifyContext { fell_back_at_ms: Some(c.now_ms - 61 * 60_000), ..c },
                None,
            ),
            ("an automatic return needs a code", &NowState::OnThisPc, &pairing, c, Some(N::NeedsSecret(Secret::Code))),
            (
                "a refused pairing says to pair",
                &playing,
                &refused,
                c,
                Some(N::NeedsSecret(Secret::Code)),
            ),
            ("the output went missing", &playing, &NowState::OutputMissing, c, Some(N::OutputMissing)),
            (
                "never while the panel is in front",
                &retrying_back,
                &fell_back,
                NotifyContext { panel_or_settings_in_front: true, ..c },
                None,
            ),
            (
                "the switch turns the notifications about speakers off",
                &playing,
                &taken,
                NotifyContext { tell_me: false, ..c },
                None,
            ),
            (
                "the switch leaves the missing output on",
                &playing,
                &NowState::OutputMissing,
                NotifyContext { tell_me: false, ..c },
                Some(N::OutputMissing),
            ),
            (
                "at most one per speaker per 30 minutes",
                &playing,
                &taken,
                NotifyContext { last_for_speaker: Some((N::StoppedResponding, c.now_ms - 10 * 60_000)), ..c },
                None,
            ),
            ("never for a success", &NowState::OnThisPc, &playing, c, None),
            ("never for the person's own choice", &playing, &NowState::OnThisPc, c, None),
        ];
        for (what, before, after, ctx, want) in cases {
            assert_eq!(notification_for(before, after, ctx), want, "{what}");
        }
    }

    #[test]
    fn volume_belongs_to_where_the_sound_is() {
        let pc = PcOutputId::new("realtek");
        let k = SpeakerId::new("kitchen");
        let l = SpeakerId::new("living");
        assert_eq!(volume_owner(&NowState::OnThisPc, &pc), Some(VolumeOwner::PcOutput(pc.clone())));
        assert_eq!(
            volume_owner(&NowState::Playing { speaker: k.clone(), apps: Apps::default() }, &pc),
            Some(VolumeOwner::Speaker(k.clone()))
        );
        assert_eq!(
            volume_owner(
                &NowState::Connecting {
                    target: l,
                    from: SoundAt::Speaker(k.clone()),
                    stage: crate::model::state::ConnectStage::Asking
                },
                &pc
            ),
            Some(VolumeOwner::Speaker(k.clone())),
            "while switching, the volume of where the sound is now"
        );
        assert_eq!(
            volume_owner(&NowState::MaPlaying { apps: Apps::default(), room: None }, &pc),
            Some(VolumeOwner::MusicAssistant)
        );
        assert_eq!(
            volume_owner(
                &NowState::NotResponding {
                    speaker: k,
                    dropout: Dropout::KeepTrying { every_secs: 30 }
                },
                &pc
            ),
            None
        );
        assert_eq!(level_on_connect(Some(32), Some(80)), 32, "a speaker's own level wins");
        assert_eq!(level_on_connect(None, Some(40)), 40, "AirPlay: the level last used");
        assert_eq!(level_on_connect(None, None), 25, "AirPlay the first time: 25, never 80");
        assert_eq!(within_limit(90, Some(70)), 70);
        assert_eq!(within_limit(50, Some(70)), 50);
    }

    #[test]
    fn played_means_two_seconds_above_minus_50_dbfs_within_ten() {
        let mut d = PlayedDetector::new();
        let loud = 0.01;
        let quiet = 0.001;
        // 1.75 s of sound, polled every 250 ms: not yet.
        let mut t = 0;
        d.observe(t, quiet);
        for _ in 0..7 {
            t += 250;
            assert!(!d.observe(t, loud));
        }
        // Then quiet for 10 s: those 1.75 s leave the window.
        for _ in 0..40 {
            t += 250;
            assert!(!d.observe(t, quiet));
        }
        for _ in 0..7 {
            t += 250;
            assert!(!d.observe(t, loud));
        }
        t += 250;
        assert!(d.observe(t, loud), "2 s within 10 s");
        assert!(d.observe(t + 60_000, quiet), "once played, it stays played");
        d.reset();
        assert!(!d.played());
        // A late poll doesn't count as seconds of sound.
        let mut d = PlayedDetector::new();
        d.observe(0, quiet);
        assert!(!d.observe(5_000, loud));
    }

    #[test]
    fn the_promise_to_return_holds_only_while_nothing_played() {
        assert_eq!(returns(ComesBack::LastChosen, false), Return::ByItself);
        assert_eq!(returns(ComesBack::LastChosen, true), Return::WillAsk);
        assert_eq!(returns(ComesBack::Never, false), Return::WillAsk);
        assert_eq!(looking(5_000, ComesBack::LastChosen, false), Looking::Searching);
        assert_eq!(looking(10_000, ComesBack::LastChosen, true), Looking::NotFound(Return::WillAsk));
    }
}
