//! Every sentence the everyday surfaces show, in one table keyed for
//! translation.
//!
//! A sentence is a template with named placeholders (`{speaker}`, `{ms}`),
//! so a translation can put the name, the number or the time wherever its
//! grammar wants them. A sentence whose words depend on a number names that
//! number as its plural selector and gives one form per plural category of
//! the language (English has two, Russian four), so "1 time" and "3 times"
//! come from the translator, not from code.
//!
//! English is complete by construction: [`english`] is an exhaustive match,
//! so a key without English text does not compile. The tests render every
//! state on every surface and fail when a key is never used, when a
//! template and its arguments disagree, or when another language is
//! missing a key or changes a key's placeholders.

use std::collections::BTreeSet;

/// The language the words are written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    English,
    /// A language that exists only in the tests: other word order, four
    /// plural forms, a decimal comma. It proves the model leaves both to
    /// the table. Missing keys fall back to English.
    #[cfg(test)]
    Test,
}

impl Lang {
    /// Every language with a table. A new language is added here and in
    /// [`Lang::template`]; the tests then demand every key from it.
    pub const ALL: &'static [Lang] = &[Lang::English];

    /// The template for `key`, or `None` when this language lacks it (the
    /// caller then falls back to English; the tests fail).
    pub fn template(self, key: Key) -> Option<Template> {
        match self {
            Lang::English => Some(english(key)),
            #[cfg(test)]
            Lang::Test => test_language(key),
        }
    }

    /// The plural category a number falls into in this language.
    pub fn plural(self, n: Number) -> Plural {
        match self {
            // English: "1" is singular; "0", "2", "0.5" and "1.5" are not.
            Lang::English => {
                if n.scale == 0 && n.value == 1 {
                    Plural::One
                } else {
                    Plural::Other
                }
            }
            // Russian's rule.
            #[cfg(test)]
            Lang::Test => {
                let n = n.normalized();
                if n.scale > 0 {
                    return Plural::Other;
                }
                let (m10, m100) = (n.value % 10, n.value % 100);
                if m10 == 1 && m100 != 11 {
                    Plural::One
                } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                    Plural::Few
                } else {
                    Plural::Many
                }
            }
        }
    }

    /// The plural categories this language's plural templates must give.
    pub fn plural_categories(self) -> &'static [Plural] {
        match self {
            Lang::English => &[Plural::One, Plural::Other],
            #[cfg(test)]
            Lang::Test => &[Plural::One, Plural::Few, Plural::Many, Plural::Other],
        }
    }

    fn decimal_separator(self) -> char {
        match self {
            Lang::English => '.',
            #[cfg(test)]
            Lang::Test => ',',
        }
    }

    /// A number as this language writes it ("0.15").
    pub fn number(self, n: Number) -> String {
        let n = n.normalized();
        if n.scale == 0 {
            return n.value.to_string();
        }
        let digits = n.value.unsigned_abs().to_string();
        let scale = n.scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        let sign = if n.value < 0 { "-" } else { "" };
        format!("{sign}{int}{}{frac}", self.decimal_separator())
    }

    /// A time of day as this language writes it ("21:14").
    pub fn time(self, t: LocalTime) -> String {
        match self {
            Lang::English => format!("{:02}:{:02}", t.hour, t.minute),
            #[cfg(test)]
            Lang::Test => format!("{}.{:02}", t.hour, t.minute),
        }
    }

    /// `text` as a whole sentence: a full stop is added unless it already
    /// ends in one, an ellipsis or a question mark.
    pub fn sentence(self, text: &str) -> String {
        if text.ends_with(['.', '…', '?', '!']) {
            text.to_string()
        } else {
            format!("{text}.")
        }
    }

    /// Several parts of a one-line status, joined ("Playing on Kitchen ·
    /// Sonos · volume 32").
    pub fn join_parts(self, parts: &[String]) -> String {
        parts.join(" · ")
    }

    /// Renders `key` with `args`.
    pub fn get(self, key: Key, args: &[(&str, Arg<'_>)]) -> String {
        render(self, key, args)
    }
}

/// A time of day on the PC's clock, as the engine read it. The model has
/// no clock of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalTime {
    pub hour: u8,
    pub minute: u8,
}

impl LocalTime {
    pub const fn new(hour: u8, minute: u8) -> Self {
        LocalTime { hour, minute }
    }
}

/// A decimal number: `value / 10^scale` (`Number { value: 15, scale: 2 }`
/// is 0.15). Kept exact so that "0.15" never prints as "0.15000001".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Number {
    pub value: i64,
    pub scale: u8,
}

impl Number {
    pub const fn int(value: i64) -> Self {
        Number { value, scale: 0 }
    }

    /// The same number without trailing zeros after the point.
    pub fn normalized(self) -> Self {
        let mut n = self;
        while n.scale > 0 && n.value % 10 == 0 {
            n.value /= 10;
            n.scale -= 1;
        }
        n
    }
}

/// A plural category (CLDR's names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Plural {
    One,
    Few,
    Many,
    Other,
}

/// One entry of a language's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    Text(&'static str),
    /// Words that depend on the number in the placeholder `on`.
    Plural {
        on: &'static str,
        one: Option<&'static str>,
        few: Option<&'static str>,
        many: Option<&'static str>,
        other: &'static str,
    },
}

impl Template {
    fn form(&self, category: Plural) -> &'static str {
        match *self {
            Template::Text(t) => t,
            Template::Plural { one, few, many, other, .. } => match category {
                Plural::One => one.unwrap_or(other),
                Plural::Few => few.unwrap_or(other),
                Plural::Many => many.unwrap_or(other),
                Plural::Other => other,
            },
        }
    }

    /// Every form this template has, for checks.
    pub fn forms(&self) -> Vec<&'static str> {
        match *self {
            Template::Text(t) => vec![t],
            Template::Plural { one, few, many, other, .. } => {
                [one, few, many, Some(other)].into_iter().flatten().collect()
            }
        }
    }

    /// The placeholder names this template uses (all forms together, plus
    /// the plural selector).
    pub fn placeholders(&self) -> BTreeSet<&'static str> {
        let mut names: BTreeSet<&'static str> =
            self.forms().into_iter().flat_map(placeholders_in).collect();
        if let Template::Plural { on, .. } = *self {
            names.insert(on);
        }
        names
    }
}

/// The `{name}` placeholders in one template string.
pub fn placeholders_in(text: &'static str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

/// A value for a placeholder.
#[derive(Clone, Copy, Debug)]
pub enum Arg<'a> {
    Text(&'a str),
    Num(Number),
}

impl<'a> From<&'a str> for Arg<'a> {
    fn from(s: &'a str) -> Self {
        Arg::Text(s)
    }
}
impl<'a> From<&'a String> for Arg<'a> {
    fn from(s: &'a String) -> Self {
        Arg::Text(s.as_str())
    }
}
impl From<Number> for Arg<'_> {
    fn from(n: Number) -> Self {
        Arg::Num(n)
    }
}
macro_rules! int_arg {
    ($($t:ty),*) => {$(
        impl From<$t> for Arg<'_> {
            fn from(n: $t) -> Self {
                Arg::Num(Number::int(n as i64))
            }
        }
    )*};
}
int_arg!(u8, u16, u32, u64, usize, i32, i64);

/// `t!(lang, Key, name = value, …)`: the sentence `Key` with its
/// placeholders filled.
macro_rules! t {
    ($lang:expr, $key:ident $(, $name:ident = $value:expr)* $(,)?) => {
        $lang.get(
            $crate::model::text::Key::$key,
            &[$((stringify!($name), $crate::model::text::Arg::from($value))),*],
        )
    };
}
pub(crate) use t;

#[cfg(test)]
thread_local! {
    /// Keys rendered on this thread, for the check that no key is unused.
    pub(crate) static USED: std::cell::RefCell<BTreeSet<Key>> =
        const { std::cell::RefCell::new(BTreeSet::new()) };
}

fn render(lang: Lang, key: Key, args: &[(&str, Arg<'_>)]) -> String {
    #[cfg(test)]
    USED.with(|u| {
        u.borrow_mut().insert(key);
    });
    let template = lang.template(key).unwrap_or_else(|| english(key));
    let category = match template {
        Template::Text(_) => Plural::Other,
        Template::Plural { on, .. } => match args.iter().find(|(n, _)| *n == on) {
            Some((_, Arg::Num(n))) => lang.plural(*n),
            _ => {
                debug_assert!(false, "{key:?}: no number for its plural selector `{on}`");
                Plural::Other
            }
        },
    };
    let text = template.form(category);
    debug_assert!(
        args.iter().all(|(name, _)| template.placeholders().contains(name)),
        "{key:?}: an argument its template doesn't use"
    );
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        match args.iter().find(|(n, _)| *n == name) {
            Some((_, Arg::Text(s))) => out.push_str(s),
            Some((_, Arg::Num(n))) => out.push_str(&lang.number(*n)),
            None => debug_assert!(false, "{key:?}: no value for `{name}`"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

macro_rules! keys {
    ($($name:ident,)*) => {
        /// The key of one sentence in the table.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Key { $($name,)* }
        impl Key {
            /// Every key, in table order.
            pub const ALL: &'static [Key] = &[$(Key::$name,)*];
        }
    };
}

keys! {
    // The app's fixed words.
    AppName,
    ListHeading,
    ThisPc,
    MaHeading,
    MaName,
    CantHear,
    SpeakerMissing,
    Settings,
    More,
    QuitReturns,
    QuitPlain,
    Dismiss,
    VolumeName,
    Mute,
    Unmute,
    Muted,
    MaLevelLabel,
    MaLevelCaption,

    // The Now card.
    OnPcTitle,
    OnPcLine,
    OnPcLineSentApps,
    ConnectingTitle,
    ConnectingFromPc,
    ConnectingFromSpeaker,
    ConnectingFromMa,
    PlayingTitle,
    AppsPlaying,
    AppsUnknown,
    AppsTwo,
    AppsMany,
    PlayingMuted,
    ApplyingRemove,
    ApplyingAdd,
    ApplyingOwn,
    ReadyTitle,
    ReadyLine,
    ReadyElsewhereLine,
    NotTakingTitle,
    NotTakingLine,
    NotRespondingTitle,
    RetryingLine,
    RetryingSilentLine,
    KeepTryingLine,
    BackOnPcTitle,
    FellBackLine,
    RefusedPairingLine,
    ReturnsByItself,
    AskedWhenBack,
    SpeakerBackTitle,
    StayedAfterDropout,
    StayedPlayingWhenFound,
    StayedSetNotToReturn,
    TakenLine,
    NeedsPairingTitle,
    NeedsPairingLine,
    NeedsPasswordTitle,
    NeedsPasswordLine,
    LookingLine,
    CantFindTitle,
    CantFindByItself,
    CantFindWillAsk,
    OnOutputTitle,
    WindowsSwitchedLine,
    ChooseTitle,
    ChooseLine,
    RefusedTitle,
    RefusedLine,
    MaPlayingTitle,
    MaChoosesLine,
    MaRoomLine,
    MaWaitingTitle,
    MaWaitingLine,
    MaAwayTitle,
    MaAwayByItself,
    MaAwayWillAsk,
    MaBackTitle,
    MaBackLine,
    OutputMissingTitle,
    OutputMissingLine,
    SentAppsTitle,
    SentAppsLine,
    NoAppSentLine,

    // The panel's top.
    TopConnecting,
    TopPickSpeaker,
    TopMaChooses,
    TopMaRoom,
    DelayReconnectingRemove,
    DelayReconnectingAdd,
    DelayReconnectingOwn,

    // First run.
    FirstPickTitle,
    FirstPickLine,
    FirstLookingMore,
    FirstHearTitle,
    FirstHearLine,
    FirstFindTitle,
    FirstFindLine,
    FirstStartSwitch,
    NotNow,
    HeardIt,
    DidntHear,
    PlayItAgain,
    Done,
    Close,

    // Buttons.
    Cancel,
    OpenVolumeMixer,
    Reconnect,
    WhatElse,
    PlayOnPcNow,
    PlayOnPc,
    TrySpeakerAgain,
    StayOnPc,
    PlayOnSpeaker,
    PlayOnSpeakerAgain,
    PlayThroughMa,
    PairEllipsis,
    Pair,
    EnterPasswordEllipsis,
    EnterPassword,
    PairAgain,
    OpenSoundSettings,
    How,
    MaSettings,
    Repair,
    ChooseApps,
    TryAgain,
    FixInWindows,
    CantFindYourSpeaker,
    AddByAddress,
    UseShortDelay,
    UseSteady,
    RestartEllipsis,
    RestartNow,
    Later,
    Allow,
    TurnOff,
    WhatsNew,
    Donate,
    MoreSpeakersEllipsis,
    OpenShort,
    Reset,
    SpeakerSettings,
    TestSound,
    HideFromList,

    // Rows.
    KindSonos,
    KindSonosGroup,
    KindHomePod,
    KindHomePodPair,
    KindAppleTv,
    KindAirPlay,
    KindSendspin,
    KindSpeaker,
    KindAndDelay,
    RowDelayAbout,
    RowDelayUnder,
    RowDelayMeasured,
    RowFinding,
    RowAsking,
    RowWaiting,
    RowPairing,
    RowNoAnswer,
    RowWrongPassword,
    RowCouldntConnect,
    RowCouldntStart,
    RowCouldntReconnect,
    RowRetrying,
    RowKeepTrying,
    RowNotTaking,
    RowLetGo,
    RowStoppedResponding,
    RowPlayingElseSince,
    RowBusy,
    RowNotFound,
    RowNotFoundShort,
    RowInUse,
    RowPartsFound,
    RowWaitingForPair,
    RowTwoDevices,
    RowWithAddress,
    RowNeedsPairing,
    RowNeedsPassword,
    RowRepairFirst,
    RowMaChooses,
    RowMaPlaying,
    RowMaOffered,
    RowMaAwaySince,
    RowMaAway,
    RowStopPlayingTitle,
    RowStopPlayingLine,
    RowMoreSpeakers,
    RowMoreSpeakersLine,
    SearchingOn,
    Searching,
    SearchingLine,
    NetworkNamed,
    NoneFoundTitle,
    CausePublicNetwork,
    CauseFirewall,
    BusyTitle,
    BusyOtherPcTitle,
    BusyLine,
    SpokenRow,
    SpokenRowStatus,
    SpokenKindDelay,
    SpokenRowDelayAbout,
    SpokenRowDelayUnder,
    SpokenRowDelayMeasured,
    SpokenPlaying,
    SpokenReady,

    // Delay.
    DelayLabel,
    FigureAbout,
    FigureUnder,
    FigureMeasured,
    ChangeRemoved,
    ChangeAdded,
    RemoveMs,
    AddMs,
    RemoveBare,
    AddBare,
    BackToOwn,
    OtherAmount,
    AmountLabel,
    MsUnit,
    AmountPrompt,
    EnterRemoves,
    EnterAdds,
    EnterRemovesToOwn,
    EnterAddsToOwn,
    EnterLimitRemoved,
    EnterLimitAdded,
    EnterNothingRemoved,
    EnterNothingAdded,
    PhoneRemoveResult,
    PhoneAddResult,
    PhoneRemoveToOwn,
    PhoneAddToOwn,
    PhoneRemoveMost,
    PhoneAddMost,
    NotAcceptedBack,
    NotAcceptedOwn,
    DroppedOutBack,
    DroppedOutOwn,
    SteadyRemoveHint,
    LimitRemoved,
    LimitAdded,
    LimitSteadyRemoved,
    NoLessDelay,
    Removing,
    Adding,
    ReconnectPending,
    ReconnectRunning,
    SwitchedShortNothing,
    SwitchedShortWith,
    SwitchedSteadyNothing,
    SwitchedSteadyWith,
    KeepsShort,
    KeepsSteady,
    DropoutsAdd,
    DropoutsSteady,
    SymptomHint,
    FixedOnSteady,
    SoundBehind,
    SoundBreakingUp,
    SpokenDelay,
    SpokenDelayOwn,
    SpokenFigureAbout,
    SpokenFigureUnder,
    SpokenFigureMeasured,
    SpokenRemoved,
    SpokenAdded,
    SpokenOwnDelay,
    SpokenRemoveMs,
    SpokenAddMs,
    SpokenAmountField,
    SpokenDone,

    // The tray.
    TooltipFormat,
    TipConnecting,
    TipApplying,
    TipFellBack,
    TipRefusedPairing,
    TipSpeakerBack,
    TipTaken,
    TipLooking,
    TipCantFind,
    TipChoose,
    TipMaWaiting,
    TipMaBack,
    TipOutputMissing,

    // The phone remote and the command line.
    StatusApplyingRemove,
    StatusApplyingAdd,
    StatusApplyingOwn,
    StatusFellBack,
    StatusRefusedPairing,
    StatusSpeakerBack,
    StatusTaken,
    StatusNeedsPairing,
    StatusNeedsPassword,
    StatusChoose,
    StatusMaBack,
    PhoneCantReach,
    PhoneLinkRevoked,
    CliPartDelay,
    CliPartDelayChange,
    CliPartVolume,
    CliPartMuted,
    CliDelayHead,
    CliDelayHeadChange,
    CliRangeBoth,
    CliRangeAddOnly,
    CliRangeNone,
    CliRemoved,
    CliAdded,
    CliRemovedToOwn,
    CliAddedToOwn,
    CliRemovedMost,
    CliAddedMost,
    CliCantRemove,
    CliCantAdd,
    CliNoLess,
    CliSetTo,
    CliSetToMost,
    CliBackToOwn,
    CliRemovingSuffix,
    CliAddingSuffix,
    CliReconnectingSuffix,
    RefuseNoDelay,
    RefuseMaDelay,
    RefuseSteady,
    RefuseUnknownPlace,
    NotRunning,
    NoReply,
    PickerHeader,
    PickerPrompt,

    // Windows' name for the sound output.
    DeviceForPlace,
    DeviceNotRunning,

    // The confirmation of a delay shortcut.
    ConfirmRemoved,
    ConfirmAdded,
    ConfirmOwn,
    ConfirmReconnecting,
    ConfirmRemovedMost,
    ConfirmAddedMost,
    ConfirmCantRemove,
    ConfirmCantAdd,
    ConfirmLimitRemoved,
    ConfirmLimitAdded,
    ConfirmNoLess,
    ConfirmNoSteady,
    ConfirmUseShortOnPage,
    ConfirmNoSpeakerDelay,
    ConfirmMaDelay,

    // Notifications.
    NotifyStoppedTitle,
    NotifyBackOnPc,
    NotifySilent,
    NotifyTakenTitle,
    NotifyStillOnPc,
    NotifyUntilPaired,
    NotifyUntilPassword,
    NotifyRestartTitle,
    NotifyRestartBody,
    NotifyCantPlayTitle,
    NotifyCantPlayBody,

    // The notice line.
    NoticeRestart,
    NoticeNotLooking,
    NoticeAdapterOff,
    NoticeRefused,
    NoticeLab,
    NoticeUpdate,
    NoticeUpdated,
    NoticeDonate,
}

const fn text(s: &'static str) -> Template {
    Template::Text(s)
}

const fn plural(on: &'static str, one: &'static str, other: &'static str) -> Template {
    Template::Plural { on, one: Some(one), few: None, many: None, other }
}

/// The English table.
pub fn english(key: Key) -> Template {
    use Key::*;
    match key {
        AppName => text("Stream To Speaker"),
        ListHeading => text("Play on"),
        ThisPc => text("This PC"),
        MaHeading => text("Through Music Assistant"),
        MaName => text("Music Assistant"),
        CantHear => text("Can’t hear it?"),
        SpeakerMissing => text("Speaker missing?"),
        Settings => text("Settings"),
        More => text("More"),
        QuitReturns => text("Quit (sound returns to this PC)"),
        QuitPlain => text("Quit Stream To Speaker"),
        Dismiss => text("Dismiss"),
        VolumeName => text("Volume"),
        Mute => text("Mute"),
        Unmute => text("Unmute"),
        Muted => text("Muted"),
        MaLevelLabel => text("Level sent to Music Assistant"),
        MaLevelCaption => text("Each speaker’s own volume is set in Music Assistant."),

        OnPcTitle => text("Sound is on this PC"),
        OnPcLine => text("Choose a speaker below to play everything there."),
        OnPcLineSentApps => {
            text("Choose a speaker below, then send apps to it in Windows’ Volume mixer.")
        }
        ConnectingTitle => text("Connecting to {speaker}…"),
        ConnectingFromPc => text("Your sound stays on this PC until {speaker} is ready."),
        ConnectingFromSpeaker => text("Your sound stays on {from} until {speaker} is ready."),
        ConnectingFromMa => {
            text("Your sound stays with Music Assistant until {speaker} is ready.")
        }
        PlayingTitle => text("Playing on {speaker}"),
        AppsPlaying => plural("count", "{apps} is playing", "{apps} are playing"),
        AppsUnknown => text("Sound is playing"),
        AppsTwo => text("{first} and {second}"),
        AppsMany => plural("others", "{first} and {others} other app", "{first} and {others} other apps"),
        PlayingMuted => text("{playing} · {speaker} is muted"),
        ApplyingRemove => text("Reconnecting to remove {ms} ms: a few seconds of silence."),
        ApplyingAdd => text("Reconnecting to add {ms} ms: a few seconds of silence."),
        ApplyingOwn => {
            text("Reconnecting to go back to {speaker}’s own delay: a few seconds of silence.")
        }
        ReadyTitle => text("{speaker} is ready"),
        ReadyLine => text("Nothing on this PC is playing right now."),
        ReadyElsewhereLine => text(
            "{app} is playing on {output}, not here. Some apps move to a new output only after you restart them.",
        ),
        NotTakingTitle => text("{speaker} isn’t taking the sound"),
        NotTakingLine => {
            text("This PC is sending sound, but {speaker} stopped asking for it at {time}.")
        }
        NotRespondingTitle => text("{speaker} isn’t responding"),
        RetryingLine => plural(
            "seconds",
            "Trying again ({attempt} of {tries}). If {speaker} doesn’t respond within {seconds} second, your sound comes back to this PC.",
            "Trying again ({attempt} of {tries}). If {speaker} doesn’t respond within {seconds} seconds, your sound comes back to this PC.",
        ),
        RetryingSilentLine => text(
            "Trying again ({attempt} of {tries}). Nothing plays on this PC until {speaker} is back.",
        ),
        KeepTryingLine => plural(
            "seconds",
            "Trying again every {seconds} second. Nothing plays on this PC until {speaker} is back.",
            "Trying again every {seconds} seconds. Nothing plays on this PC until {speaker} is back.",
        ),
        BackOnPcTitle => text("Sound is back on this PC"),
        FellBackLine => {
            text("{speaker} stopped responding at {time}, so your sound came back to {output}.")
        }
        RefusedPairingLine => text(
            "{speaker} refused this PC’s pairing at {time}, so your sound came back to {output}.",
        ),
        ReturnsByItself => text(
            "It goes back to {speaker} by itself when {speaker} is back, unless something plays here first.",
        ),
        AskedWhenBack => text("When {speaker} is back, you’ll be asked."),
        SpeakerBackTitle => text("{speaker} is back"),
        StayedAfterDropout => text(
            "Your sound stayed on this PC because something played here after {speaker} stopped responding.",
        ),
        StayedPlayingWhenFound => text(
            "Your sound stayed on this PC because something was playing here when {speaker} was found.",
        ),
        StayedSetNotToReturn => text(
            "Your sound stayed on this PC because {speaker} is set not to come back by itself.",
        ),
        TakenLine => text(
            "{speaker} started playing something else at {time}, so your sound came back to {output}.",
        ),
        NeedsPairingTitle => text("{speaker} needs pairing"),
        NeedsPairingLine => text(
            "{speaker} wants a one-time code before it can play your sound. Your sound stays on this PC until then.",
        ),
        NeedsPasswordTitle => text("{speaker} needs its password"),
        NeedsPasswordLine => text(
            "{speaker} asks for its password before it can play your sound. Your sound stays on this PC until then.",
        ),
        LookingLine => text("Looking for {speaker}…"),
        CantFindTitle => text("Can’t find {speaker} right now"),
        CantFindByItself => text(
            "Your sound stays on this PC. It goes to {speaker} when {speaker} is found, unless something plays here first.",
        ),
        CantFindWillAsk => {
            text("Your sound stays on this PC. When {speaker} is found, you’ll be asked.")
        }
        OnOutputTitle => text("Sound is on {output}"),
        WindowsSwitchedLine => text(
            "Windows switched its output to {output}, so Stream To Speaker let go of {speaker} at {time}.",
        ),
        ChooseTitle => text("Where should Stream To Speaker play?"),
        ChooseLine => text("Pick a speaker. If you don’t, your sound goes back to {output}."),
        RefusedTitle => text("Windows didn’t switch to {speaker}"),
        RefusedLine => {
            text("Windows kept {output} as its output. In Sound settings, choose “{device}”.")
        }
        MaPlayingTitle => text("Playing through Music Assistant"),
        MaChoosesLine => text("{playing} · Music Assistant chooses the speaker."),
        MaRoomLine => text("{playing} · Music Assistant plays it in {room}."),
        MaWaitingTitle => text("Waiting for Music Assistant to play it"),
        MaWaitingLine => text(
            "Music Assistant has this PC’s sound but hasn’t started playing it. Start it there, or set it to start by itself.",
        ),
        MaAwayTitle => text("Music Assistant isn’t connected"),
        MaAwayByItself => text(
            "Your sound is on this PC until Music Assistant is back. It goes back to Music Assistant when it connects, unless something plays here first.",
        ),
        MaAwayWillAsk => text(
            "Your sound is on this PC until Music Assistant is back. When Music Assistant is back, you’ll be asked.",
        ),
        MaBackTitle => text("Music Assistant is back"),
        MaBackLine => text(
            "Your sound stayed on this PC because something played here while Music Assistant was away.",
        ),
        OutputMissingTitle => text("The Stream To Speaker sound output is missing"),
        OutputMissingLine => text(
            "Windows doesn’t have the output this app plays through, so your sound stays on this PC. Repair puts it back; it needs administrator approval.",
        ),
        SentAppsTitle => text("Playing {apps} on {speaker}"),
        SentAppsLine => text(
            "Only apps you send here play on {speaker}; everything else stays on this PC.",
        ),
        NoAppSentLine => text(
            "No app is sending sound here. In Windows’ Volume mixer, set an app’s output to “{device}”.",
        ),

        TopConnecting => text("Your sound moves there when it is ready."),
        TopPickSpeaker => text("Pick a speaker to play this PC’s sound there."),
        TopMaChooses => text("Music Assistant chooses the speaker."),
        TopMaRoom => text("Music Assistant plays it in {room}."),
        DelayReconnectingRemove => text("reconnecting to remove {ms} ms…"),
        DelayReconnectingAdd => text("reconnecting to add {ms} ms…"),
        DelayReconnectingOwn => text("reconnecting to go back to {speaker}’s own delay…"),

        FirstPickTitle => text("Where should this PC’s sound play?"),
        FirstPickLine => text("Speakers show up here as they’re found."),
        FirstLookingMore => text("Looking for more speakers…"),
        FirstHearTitle => text("Did you hear the chime on {speaker}?"),
        FirstHearLine => {
            text("{speaker} now plays all of this PC’s sound. To bring it back, pick This PC.")
        }
        FirstFindTitle => text("Stream To Speaker stays by the clock"),
        FirstFindLine => {
            text("Windows may tuck its icon under ^. Drag it onto the taskbar to keep it in view.")
        }
        FirstStartSwitch => text("Start when I sign in, and go back to {speaker}"),
        NotNow => text("Not now"),
        HeardIt => text("I heard it"),
        DidntHear => text("I didn’t hear it"),
        PlayItAgain => text("Play it again"),
        Done => text("Done"),
        Close => text("Close"),

        Cancel => text("Cancel"),
        OpenVolumeMixer => text("Open Volume mixer"),
        Reconnect => text("Reconnect"),
        WhatElse => text("What else can I try?"),
        PlayOnPcNow => text("Play on this PC now"),
        PlayOnPc => text("Play on this PC"),
        TrySpeakerAgain => text("Try {speaker} again"),
        StayOnPc => text("Stay on this PC"),
        PlayOnSpeaker => text("Play on {speaker}"),
        PlayOnSpeakerAgain => text("Play on {speaker} again"),
        PlayThroughMa => text("Play through Music Assistant"),
        PairEllipsis => text("Pair…"),
        Pair => text("Pair"),
        EnterPasswordEllipsis => text("Enter password…"),
        EnterPassword => text("Enter password"),
        PairAgain => text("Pair again"),
        OpenSoundSettings => text("Open Sound settings"),
        How => text("How?"),
        MaSettings => text("Music Assistant settings"),
        Repair => text("Repair"),
        ChooseApps => text("Choose apps…"),
        TryAgain => text("Try again"),
        FixInWindows => text("Fix in Windows settings"),
        CantFindYourSpeaker => text("Can’t find your speaker?"),
        AddByAddress => text("Add by address"),
        UseShortDelay => text("Use Short delay"),
        UseSteady => text("Use Steady"),
        RestartEllipsis => text("Restart…"),
        RestartNow => text("Restart now"),
        Later => text("Later"),
        Allow => text("Allow"),
        TurnOff => text("Turn off"),
        WhatsNew => text("What’s new"),
        Donate => text("Donate"),
        MoreSpeakersEllipsis => text("More speakers…"),
        OpenShort => text("Open"),
        Reset => text("Reset"),
        SpeakerSettings => text("Speaker settings"),
        TestSound => text("Play a test sound"),
        HideFromList => text("Hide from this list"),

        KindSonos => text("Sonos"),
        KindSonosGroup => text("Sonos group"),
        KindHomePod => text("HomePod"),
        KindHomePodPair => text("HomePod pair"),
        KindAppleTv => text("Apple TV"),
        KindAirPlay => text("AirPlay"),
        KindSendspin => text("Sendspin speaker"),
        KindSpeaker => text("Speaker"),
        KindAndDelay => text("{kind} · {delay}"),
        RowDelayAbout => text("about {seconds} s delay"),
        RowDelayUnder => text("under {seconds} s delay"),
        RowDelayMeasured => text("{seconds} s delay, measured"),
        RowFinding => text("Finding {speaker}…"),
        RowAsking => text("Asking {speaker} to play…"),
        RowWaiting => text("Waiting for {speaker} to start…"),
        RowPairing => text("Pairing — finish in the window that opened"),
        RowNoAnswer => text("Couldn’t connect — no answer."),
        RowWrongPassword => text("Couldn’t connect — wrong password."),
        RowCouldntConnect => text("Couldn’t connect."),
        RowCouldntStart => text("Couldn’t start connecting."),
        RowCouldntReconnect => text("Couldn’t reconnect {speaker}."),
        RowRetrying => text("Isn’t responding — trying again ({attempt} of {tries})"),
        RowKeepTrying => text("Isn’t responding — trying every {seconds} s"),
        RowNotTaking => text("Connected · not taking the sound"),
        RowLetGo => text("Let go at {time}"),
        RowStoppedResponding => text("Stopped responding at {time}"),
        RowPlayingElseSince => text("Playing something else since {time}"),
        RowBusy => text("{kind} · playing something else"),
        RowNotFound => text("Not found right now"),
        RowNotFoundShort => text("Not found"),
        RowInUse => text("In use"),
        RowPartsFound => text("{found} of {all} found"),
        RowWaitingForPair => text("Waiting for the second HomePod"),
        RowTwoDevices => text("{kind} · two devices answer as {name}"),
        RowWithAddress => text("{name} ({address})"),
        RowNeedsPairing => text("Needs pairing"),
        RowNeedsPassword => text("Needs its password"),
        RowRepairFirst => text("Repair first"),
        RowMaChooses => text("Chooses the speaker in Music Assistant"),
        RowMaPlaying => text("Playing · Music Assistant chooses the speaker"),
        RowMaOffered => text("Offered · not playing yet"),
        RowMaAwaySince => text("Not connected · last seen {time}"),
        RowMaAway => text("Not connected"),
        RowStopPlayingTitle => text("Stop playing on a speaker"),
        RowStopPlayingLine => text("Apps you send here go quiet until you pick a speaker"),
        RowMoreSpeakers => text("More speakers ({count})"),
        RowMoreSpeakersLine => text("Never used from this PC"),
        SearchingOn => text("Looking for speakers on {network}…"),
        Searching => text("Looking for speakers…"),
        SearchingLine => plural(
            "seconds",
            "Every {seconds} second while this is open",
            "Every {seconds} seconds while this is open",
        ),
        NetworkNamed => text("{adapter} “{network}”"),
        NoneFoundTitle => text("No speakers found yet."),
        CausePublicNetwork => {
            text("Windows treats “{network}” as a public network, so speakers can’t answer.")
        }
        CauseFirewall => text("Windows Firewall blocks Stream To Speaker on this network."),
        BusyTitle => text("{speaker} is playing something else right now."),
        BusyOtherPcTitle => text("{speaker} is playing {pc}’s sound right now."),
        BusyLine => {
            text("Play this PC’s sound there instead? What’s playing on {speaker} will stop.")
        }
        SpokenRow => text("{name}. {detail}."),
        SpokenRowStatus => text("{name}. {detail}. {status}"),
        SpokenKindDelay => text("{kind}, {delay}"),
        SpokenRowDelayAbout => text("about {seconds} second delay"),
        SpokenRowDelayUnder => text("under {seconds} second delay"),
        SpokenRowDelayMeasured => text("{seconds} second delay, measured"),
        SpokenPlaying => text("Playing."),
        SpokenReady => text("Ready."),

        DelayLabel => text("Delay"),
        FigureAbout => text("about {seconds} s"),
        FigureUnder => text("under {seconds} s"),
        FigureMeasured => text("{seconds} s, measured"),
        ChangeRemoved => text("{ms} ms removed"),
        ChangeAdded => text("{ms} ms added"),
        RemoveMs => text("Remove {ms} ms"),
        AddMs => text("Add {ms} ms"),
        RemoveBare => text("Remove"),
        AddBare => text("Add"),
        BackToOwn => text("Back to {speaker}’s own delay"),
        OtherAmount => text("Other amount…"),
        AmountLabel => text("Amount"),
        MsUnit => text("ms"),
        AmountPrompt => text("Type how many milliseconds, then choose Remove or Add."),
        EnterRemoves => text("Enter removes {ms} ms: {total} in all."),
        EnterAdds => text("Enter adds {ms} ms: {total} in all."),
        EnterRemovesToOwn => text("Enter removes {ms} ms: back to {speaker}’s own delay."),
        EnterAddsToOwn => text("Enter adds {ms} ms: back to {speaker}’s own delay."),
        EnterLimitRemoved => {
            text("{speaker} allows at most {max} ms removed, so Enter removes {ms} ms.")
        }
        EnterLimitAdded => text("{speaker} allows at most {max} ms added, so Enter adds {ms} ms."),
        EnterNothingRemoved => {
            text("{speaker} allows at most {max} ms removed, so Enter changes nothing.")
        }
        EnterNothingAdded => {
            text("{speaker} allows at most {max} ms added, so Enter changes nothing.")
        }
        PhoneRemoveResult => text("Remove {ms} ms: {total} in all."),
        PhoneAddResult => text("Add {ms} ms: {total} in all."),
        PhoneRemoveToOwn => text("Remove {ms} ms: back to {speaker}’s own delay."),
        PhoneAddToOwn => text("Add {ms} ms: back to {speaker}’s own delay."),
        PhoneRemoveMost => text("Remove {ms} ms: {total} in all, the most {speaker} allows."),
        PhoneAddMost => text("Add {ms} ms: {total} in all, the most {speaker} allows."),
        NotAcceptedBack => text("{speaker} didn’t accept {tried}, so it’s back to {total}."),
        NotAcceptedOwn => text("{speaker} didn’t accept {tried}, so it’s back to its own delay."),
        DroppedOutBack => {
            text("{speaker} dropped out with {was}, so {ms} ms went back: {total}.")
        }
        DroppedOutOwn => {
            text("{speaker} dropped out with {was}, so it went back to its own delay.")
        }
        SteadyRemoveHint => {
            text("{speaker} is {late} late on Steady. For video, use Short delay ({short}).")
        }
        LimitRemoved => text("{speaker} allows at most {max} ms removed."),
        LimitAdded => text("{speaker} allows at most {max} ms added."),
        LimitSteadyRemoved => text(
            "On Steady, {speaker} allows at most {max} ms removed. For less delay, use Short delay.",
        ),
        NoLessDelay => text("{speaker} already plays with as little delay as it can."),
        Removing => text("Removing… {ms} ms to go"),
        Adding => text("Adding… {ms} ms to go"),
        ReconnectPending => text("{speaker} reconnects to apply this: a few seconds of silence."),
        ReconnectRunning => text("Reconnecting {speaker}…"),
        SwitchedShortNothing => text("Short delay starts with nothing removed or added."),
        SwitchedShortWith => text("Short delay starts with {total}."),
        SwitchedSteadyNothing => text("Steady starts with nothing removed or added."),
        SwitchedSteadyWith => text("Steady starts with {total}."),
        KeepsShort => text("Short delay keeps its {total}."),
        KeepsSteady => text("Steady keeps its {total}."),
        DropoutsAdd => plural(
            "count",
            "{speaker} dropped out {count} time in 10 minutes. Adding {ms} ms may help.",
            "{speaker} dropped out {count} times in 10 minutes. Adding {ms} ms may help.",
        ),
        DropoutsSteady => plural(
            "count",
            "{speaker} dropped out {count} time in 10 minutes. Steady may help.",
            "{speaker} dropped out {count} times in 10 minutes. Steady may help.",
        ),
        SymptomHint => {
            text("Sound behind the picture? Remove some delay. Sound breaking up? Add some.")
        }
        FixedOnSteady => text(
            "On Steady, {speaker} keeps this delay itself, so none of it can be removed yet. For video, use Short delay ({short}).",
        ),
        SoundBehind => text("Sound behind the picture?"),
        SoundBreakingUp => text("Sound breaking up?"),
        SpokenDelay => {
            text("Delay on {speaker}: {figure}, {change}. Press Enter to type an amount.")
        }
        SpokenDelayOwn => text(
            "Delay on {speaker}: {figure}, nothing removed or added. Press Enter to type an amount.",
        ),
        SpokenFigureAbout => plural("seconds", "about {seconds} second", "about {seconds} seconds"),
        SpokenFigureUnder => plural("seconds", "under {seconds} second", "under {seconds} seconds"),
        SpokenFigureMeasured => {
            plural("seconds", "{seconds} second, measured", "{seconds} seconds, measured")
        }
        SpokenRemoved => plural("ms", "{ms} millisecond removed", "{ms} milliseconds removed"),
        SpokenAdded => plural("ms", "{ms} millisecond added", "{ms} milliseconds added"),
        SpokenOwnDelay => text("{speaker}’s own delay"),
        SpokenRemoveMs => {
            plural("ms", "Remove {ms} millisecond of delay", "Remove {ms} milliseconds of delay")
        }
        SpokenAddMs => plural("ms", "Add {ms} millisecond of delay", "Add {ms} milliseconds of delay"),
        SpokenAmountField => text("Milliseconds to remove or add"),
        SpokenDone => text("Done: {result}"),

        TooltipFormat => text("Stream To Speaker – {status}"),
        TipConnecting => text("Connecting to {speaker}"),
        TipApplying => text("Playing on {speaker} (changing its delay)"),
        TipFellBack => text("{speaker} stopped responding. Sound is on this PC"),
        TipRefusedPairing => text("{speaker} refused this PC’s pairing. Sound is on this PC"),
        TipSpeakerBack => text("{speaker} is back. Sound is on this PC"),
        TipTaken => text("{speaker} is playing something else. Sound is on this PC"),
        TipLooking => text("Looking for {speaker}"),
        TipCantFind => text("Can’t find {speaker}"),
        TipChoose => text("Choose a speaker"),
        TipMaWaiting => text("Waiting for Music Assistant"),
        TipMaBack => text("Music Assistant is back. Sound is on this PC"),
        TipOutputMissing => text("Sound output missing"),

        StatusApplyingRemove => text("Playing on {speaker} (reconnecting to remove {ms} ms)"),
        StatusApplyingAdd => text("Playing on {speaker} (reconnecting to add {ms} ms)"),
        StatusApplyingOwn => {
            text("Playing on {speaker} (reconnecting to go back to its own delay)")
        }
        StatusFellBack => text("Sound is on this PC ({speaker} stopped responding)"),
        StatusRefusedPairing => text("Sound is on this PC ({speaker} refused this PC’s pairing)"),
        StatusSpeakerBack => text("Sound is on this PC ({speaker} is back)"),
        StatusTaken => text("Sound is on this PC ({speaker} is playing something else)"),
        StatusNeedsPairing => text("{speaker} needs pairing — pair it on the PC"),
        StatusNeedsPassword => text("{speaker} needs its password — enter it on the PC"),
        StatusChoose => text("Choose a speaker on the PC"),
        StatusMaBack => text("Sound is on this PC (Music Assistant is back)"),
        PhoneCantReach => text("Can’t reach {pc}. Last update {time}."),
        PhoneLinkRevoked => text(
            "This link no longer works. Get a new one on your PC: Stream To Speaker → Settings → Privacy & remote.",
        ),
        CliPartDelay => text("delay {figure}"),
        CliPartDelayChange => text("delay {figure}, {change}"),
        CliPartVolume => text("volume {level}"),
        CliPartMuted => text("muted"),
        CliDelayHead => text("{speaker}: delay {figure}."),
        CliDelayHeadChange => text("{speaker}: delay {figure}, {change}."),
        CliRangeBoth => text("You can remove up to {removed} ms and add up to {added} ms."),
        CliRangeAddOnly => text("You can add up to {added} ms."),
        CliRangeNone => text("On Steady, none of it can be removed yet."),
        CliRemoved => text("{speaker}: removed {ms} ms — {total} in all."),
        CliAdded => text("{speaker}: added {ms} ms — {total} in all."),
        CliRemovedToOwn => text("{speaker}: removed {ms} ms — back to its own delay."),
        CliAddedToOwn => text("{speaker}: added {ms} ms — back to its own delay."),
        CliRemovedMost => {
            text("{speaker}: removed {ms} ms — {total} in all, the most {speaker} allows.")
        }
        CliAddedMost => {
            text("{speaker}: added {ms} ms — {total} in all, the most {speaker} allows.")
        }
        CliCantRemove => {
            text("{speaker}: can’t remove more — {speaker} allows at most {max} ms removed.")
        }
        CliCantAdd => text("{speaker}: can’t add more — {speaker} allows at most {max} ms added."),
        CliNoLess => {
            text("{speaker}: can’t remove more — it already plays with as little delay as it can.")
        }
        CliSetTo => text("{speaker}: set to {total}."),
        CliSetToMost => text("{speaker}: set to {total}, the most {speaker} allows."),
        CliBackToOwn => text("{speaker}: back to its own delay."),
        CliRemovingSuffix => text("Removing…"),
        CliAddingSuffix => text("Adding…"),
        CliReconnectingSuffix => text("Reconnecting to apply: a few seconds of silence."),
        RefuseNoDelay => text("{status} — no speaker delay to change."),
        RefuseMaDelay => {
            text("{status} — Music Assistant sets the delay for each of its speakers.")
        }
        RefuseSteady => {
            text("{speaker}: no delay changes on Steady. Use Short delay on {speaker}’s page.")
        }
        RefuseUnknownPlace => text("No speaker called “{name}” is in the list."),
        NotRunning => text("Stream To Speaker isn’t running."),
        NoReply => text("Stream To Speaker didn’t answer."),
        PickerHeader => text("Stream To Speaker {version} — looking for speakers on {network}…"),
        PickerPrompt => text("Play on [1-{count}], Enter = this PC, r = search again: "),

        DeviceForPlace => text("{place} (Stream To Speaker)"),
        DeviceNotRunning => text("Stream To Speaker (not running)"),

        ConfirmRemoved => text("Removed {ms} ms"),
        ConfirmAdded => text("Added {ms} ms"),
        ConfirmOwn => text("{speaker}’s own delay"),
        ConfirmReconnecting => text("Reconnecting to apply: a few seconds of silence"),
        ConfirmRemovedMost => text("Removed {ms} ms: the most {speaker} allows"),
        ConfirmAddedMost => text("Added {ms} ms: the most {speaker} allows"),
        ConfirmCantRemove => text("Can’t remove more"),
        ConfirmCantAdd => text("Can’t add more"),
        ConfirmLimitRemoved => text("{speaker} allows at most {max} ms removed"),
        ConfirmLimitAdded => text("{speaker} allows at most {max} ms added"),
        ConfirmNoLess => text("{speaker} already plays with as little delay as it can"),
        ConfirmNoSteady => text("No delay changes on Steady"),
        ConfirmUseShortOnPage => text("Use Short delay on {speaker}’s page"),
        ConfirmNoSpeakerDelay => text("No speaker delay to change"),
        ConfirmMaDelay => text("Music Assistant sets each speaker’s delay"),

        NotifyStoppedTitle => text("{speaker} stopped responding"),
        NotifyBackOnPc => text("Your sound is back on this PC."),
        NotifySilent => text("Nothing plays on this PC until it’s back."),
        NotifyTakenTitle => text("{speaker} is playing something else"),
        NotifyStillOnPc => text("Your sound is still on this PC."),
        NotifyUntilPaired => text("Your sound stays on this PC until you pair it."),
        NotifyUntilPassword => text("Your sound stays on this PC until you enter it."),
        NotifyRestartTitle => text("Restart to finish updating"),
        NotifyRestartBody => text(
            "The new sound output starts after Windows restarts. Stream To Speaker keeps working until then.",
        ),
        NotifyCantPlayTitle => text("Stream To Speaker can’t play"),
        NotifyCantPlayBody => text("Its sound output is missing from Windows."),

        NoticeRestart => text("Restart Windows to finish updating."),
        NoticeNotLooking => text("Not looking for speakers:"),
        NoticeAdapterOff => text("the network adapter “{adapter}” is off."),
        NoticeRefused => text("{device} asked for the sound and was refused."),
        NoticeLab => text("Lab settings are on."),
        NoticeUpdate => text("Version {version} is ready to install."),
        NoticeUpdated => text("Updated to {version}."),
        NoticeDonate => {
            text("Stream To Speaker is free. Donations pay for signing its sound driver.")
        }
    }
}

/// The test language's few sentences.
#[cfg(test)]
fn test_language(key: Key) -> Option<Template> {
    use Key::*;
    let four = |on, one, few, many, other| Template::Plural {
        on,
        one: Some(one),
        few: Some(few),
        many: Some(many),
        other,
    };
    Some(match key {
        FellBackLine => text("{time} — {output}: {speaker} stopped."),
        ChangeRemoved => text("removed: {ms}"),
        FigureAbout => text("≈{seconds} s"),
        DropoutsAdd => four(
            "count",
            "{ms} for {speaker}? {count} drop (one)",
            "{ms} for {speaker}? {count} drops (few)",
            "{ms} for {speaker}? {count} drops (many)",
            "{ms} for {speaker}? {count} drops (other)",
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_every_key_with_the_same_placeholders() {
        for &lang in Lang::ALL {
            for &key in Key::ALL {
                let t = lang.template(key).unwrap_or_else(|| panic!("{lang:?} lacks {key:?}"));
                assert_eq!(
                    t.placeholders(),
                    english(key).placeholders(),
                    "{lang:?} {key:?} uses other placeholders than English"
                );
                if let Template::Plural { one, few, many, .. } = t {
                    for cat in lang.plural_categories() {
                        let given = match cat {
                            Plural::One => one.is_some(),
                            Plural::Few => few.is_some(),
                            Plural::Many => many.is_some(),
                            Plural::Other => true,
                        };
                        assert!(given, "{lang:?} {key:?} lacks its {cat:?} form");
                    }
                }
            }
        }
    }

    #[test]
    fn every_template_is_well_formed() {
        for &key in Key::ALL {
            let t = english(key);
            for form in t.forms() {
                assert_eq!(
                    form.matches('{').count(),
                    form.matches('}').count(),
                    "{key:?}: unbalanced braces"
                );
                assert!(!form.contains('\''), "{key:?}: a straight apostrophe or quote");
                assert!(!form.contains('"'), "{key:?}: a straight quotation mark");
                assert!(!form.contains("..."), "{key:?}: three dots instead of an ellipsis");
            }
        }
    }

    #[test]
    fn numbers_and_times_as_english_writes_them() {
        let en = Lang::English;
        assert_eq!(en.number(Number { value: 15, scale: 2 }), "0.15");
        assert_eq!(en.number(Number { value: 20, scale: 2 }), "0.2");
        assert_eq!(en.number(Number { value: 200, scale: 2 }), "2");
        assert_eq!(en.number(Number { value: 174, scale: 2 }), "1.74");
        assert_eq!(en.number(Number { value: 5, scale: 2 }), "0.05");
        assert_eq!(en.number(Number::int(1000)), "1000");
        assert_eq!(en.time(LocalTime::new(9, 5)), "09:05");
        assert_eq!(en.time(LocalTime::new(21, 14)), "21:14");
        assert_eq!(en.plural(Number::int(1)), Plural::One);
        assert_eq!(en.plural(Number::int(3)), Plural::Other);
        assert_eq!(en.plural(Number { value: 15, scale: 2 }), Plural::Other);
    }

    #[test]
    fn plural_forms_follow_the_number() {
        let en = Lang::English;
        assert_eq!(
            t!(en, DropoutsAdd, speaker = "Kitchen", count = 1u32, ms = 100u32),
            "Kitchen dropped out 1 time in 10 minutes. Adding 100 ms may help."
        );
        assert_eq!(
            t!(en, DropoutsAdd, speaker = "Kitchen", count = 3u32, ms = 100u32),
            "Kitchen dropped out 3 times in 10 minutes. Adding 100 ms may help."
        );
    }

    #[test]
    fn a_sentence_gets_one_full_stop() {
        let en = Lang::English;
        assert_eq!(en.sentence("Playing on Kitchen"), "Playing on Kitchen.");
        assert_eq!(en.sentence("Connecting to Kitchen…"), "Connecting to Kitchen…");
        assert_eq!(en.sentence("Done."), "Done.");
    }
}
