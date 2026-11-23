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
    // The panel's closed delay line, which has no amount field.
    SpokenPanelDelay,
    SpokenPanelDelayOwn,
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
    RefuseOnlyFromWindow,
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

    // The Settings window and its dialogs.
    SetWindowTitle,
    SetNavSpeakers,
    SetNavGeneral,
    SetNavPrivacy,
    SetNavHelp,
    SetNavAbout,
    SetNavLab,
    SetKindAirPlay,
    SetUpnpKind,
    SetKindStatus,
    SetListAnd,
    SetDate,
    SetJanuary,
    SetFebruary,
    SetMarch,
    SetApril,
    SetMay,
    SetJune,
    SetJuly,
    SetAugust,
    SetSeptember,
    SetOctober,
    SetNovember,
    SetDecember,
    SetToday,
    SetYesterday,
    SetLeft,
    SetDbPlus,
    SetDbMinus,
    SetDbZero,
    SetCancel,
    SetClose,
    SetShow,
    SetHide,
    SetCopy,
    SetCopied,
    SetChange,
    SetRemove,
    SetDetails,
    SetNext,
    SetBackButton,
    SetTryAgain,
    SetConnect,
    SetReconnect,
    SetCheckNow,
    SetOpenPanel,
    SetExperimental,
    SetEnglish,
    SetYourSpeakers,
    SetNoSpeakersYet,
    SetNoSpeakersYetLine,
    SetHiddenHeading,
    SetHiddenLine,
    SetShowNamed,
    SetFindingSpeakers,
    SetLookOn,
    SetAllNetworks,
    SetLookingOn,
    SetLookingPinned,
    SetNotLookingOff,
    SetLookingNowhere,
    SetNetworkNamed,
    SetNetworkPlain,
    SetAddByAddress,
    SetSearchAgain,
    SetCantFindLink,
    SetMoreKinds,
    SetSendspinSwitch,
    SetSendspinLine,
    SetPlayingNow,
    SetReadyNow,
    SetConnectingNow,
    SetNotFoundRow,
    SetNotFoundSeen,
    SetAddedByAddress,
    SetCantPlayRow,
    SetPlayingNowCap,
    SetReadyCap,
    SetConnectingCap,
    SetAvailableCap,
    SetNotFoundCap,
    SetPlayOn,
    SetPlayingButton,
    SetConnectingButton,
    SetTestSound,
    SetTestSoundOn,
    SetCantPlayTitle,
    SetCantPlayFix,
    SetUntestedPairTitle,
    SetUntestedPairLine,
    SetReportProblem,
    SetDelayHeading,
    SetOwnDelay,
    SetSonosOwn,
    SetSendspinOwn,
    SetKeepsAsked,
    SetPairAlwaysShort,
    SetMeasuredOn,
    SetShortDelayAbout,
    SetShortDelayLine,
    SetSteadyAbout,
    SetSteadyLine,
    SetChoiceReconnects,
    SetSymptoms,
    SetRemembered,
    SetAppliesLive,
    SetAppliesAtOnce,
    SetAppliesReconnect,
    SetAppliesPair,
    SetRange,
    SetRangeSteady,
    SetRangeShort,
    SetRangeAddOnly,
    SetShortcutsOnLine,
    SetShortcutsOffLine,
    SetConnectsWith,
    SetWaySonos,
    SetWayAirPlay,
    SetWayAirPlayLine,
    SetWaysKeptApart,
    SetGroupHeading,
    SetSonosGroupLine,
    SetPairLine,
    SetAppleTvGroup,
    SetAppleTvGroupLine,
    SetVolumeLimit,
    SetNeverLouder,
    SetNoLimit,
    SetVolumeLimitLine,
    SetToneHeading,
    SetBass,
    SetTreble,
    SetAllBands,
    SetUndo,
    SetResetFlat,
    SetToneLine,
    SetConnectionHeading,
    SetPaired,
    SetPairedOn,
    SetPairedLine,
    SetPairAgain,
    SetForgetPairing,
    SetPasswordTitle,
    SetPasswordSaved,
    SetRemovePassword,
    SetChangePassword,
    SetComesBack,
    SetComesBackLast,
    SetComesBackAppears,
    SetComesBackNever,
    SetComesBackLastLine,
    SetComesBackAppearsLine,
    SetComesBackNeverLine,
    SetSongTitles,
    SetSongTitlesLine,
    SetShowAs,
    SetShowAsSonos,
    SetShowAsHome,
    SetShowAsLine,
    SetDetailsLine,
    SetDetailModel,
    SetDetailAddress,
    SetDetailConnection,
    SetDetailDelay,
    SetDetailProblem,
    SetDetailId,
    SetDetailNone,
    SetCopyDetails,
    SetForgetNamed,
    SetRemoveNamed,
    SetHideFromList,
    SetForgetLine,
    SetRemoveLine,
    SetWhatPlays,
    SetEverything,
    SetEverythingLine,
    SetPlayCalls,
    SetPlayCallsLine,
    SetOnlySent,
    SetOnlySentLine,
    SetOpenVolumeMixer,
    SetStartUp,
    SetStartAtSignIn,
    SetStartAtSignInLine,
    SetSignInFailed,
    SetGoBack,
    SetGoBackLine,
    SetWhenWrong,
    SetWhenStops,
    SetBringBack,
    SetBringBackLine,
    SetStaySilent,
    SetStaySilentLine,
    SetNotify,
    SetNotifyLine,
    SetShortcutsHeading,
    SetShortcutsSwitch,
    SetShortcutsLine,
    SetShortcutRemove,
    SetShortcutAdd,
    SetHold,
    SetForLarge,
    SetChangeKeys,
    SetPressKeys,
    SetShortcutReady,
    SetShortcutTaken,
    SetLanguage,
    SetLanguageWindows,
    SetMaLead,
    SetMaOffForRun,
    SetMaOffer,
    SetMaOfferLine,
    SetMaFailed,
    SetMaPausedTitle,
    SetMaPausedLine,
    SetMaAllowAgain,
    SetMaSettingUp,
    SetMaStep1,
    SetMaStep1Line,
    SetMaFound,
    SetMaFoundAt,
    SetMaFoundBefore,
    SetMaStep2,
    SetMaStep2Line,
    SetMaPaired,
    SetMaUnpair,
    SetMaUnpairNamed,
    SetMaStep3,
    SetMaStep3Line,
    SetMaPlayed,
    SetMaPlayedOn,
    SetMaTicks,
    SetMaAllDone,
    SetOpenMa,
    SetMaToken,
    SetMaTokenLine,
    SetCopyToken,
    SetMaAppearsAs,
    SetMaAppearsAsLine,
    SetMaDelayLine,
    SetWhoCanListen,
    SetOnlyPlaying,
    SetOnlyPlayingLine,
    SetAnyDevice,
    SetAnyDeviceLine,
    SetRecentlyRefused,
    SetRefusedNamed,
    SetRefusedUnknown,
    SetAllowFor,
    SetNotPartOfSpeaker,
    SetFromElsewhere,
    SetPhone,
    SetPhoneLine,
    SetScanOrOpen,
    SetCopyLink,
    SetNewLink,
    SetLinkLine,
    SetApi,
    SetApiLine,
    SetApiHow,
    SetCopyKey,
    SetNewKey,
    SetCantHearLine,
    SetCheckPlayback,
    SetCantFindLine,
    SetCheckFinding,
    SetReportLine,
    SetWriteReport,
    SetLogs,
    SetLogsLine,
    SetOpenLogs,
    SetHowItWorks,
    SetHowItWorksLine,
    SetReadOnline,
    SetShowWelcome,
    SetGuidePage,
    SetGuideWhereTitle,
    SetGuideWhere,
    SetGuideDelayTitle,
    SetGuideDelay,
    SetGuideOffTitle,
    SetGuideOff,
    SetGuideRoomsTitle,
    SetGuideRooms,
    SetGuideAirPlayTitle,
    SetGuideBackTitle,
    SetGuideBack,
    SetGuideAirPlay,
    SetGuideMaTitle,
    SetGuideMa,
    SetCheckLead,
    SetCheckOnPc,
    SetCheckOnPcLine,
    SetCheckPlaying,
    SetCheckAppPlays,
    SetCheckWindows,
    SetCheckWindowsElsewhere,
    SetSendTo,
    SetCheckAppElsewhere,
    SetCheckRestartApp,
    SetCheckConnected,
    SetCheckNotConnected,
    SetCheckTaking,
    SetCheckNotAsking,
    SetTryAirPlay,
    SetCheckRefused,
    SetAllowIt,
    SetCheckCantTell,
    SetCheckCantTellLine,
    SetTrySteady,
    SetCheckVolume,
    SetCheckVolumeLevel,
    SetCheckMuted,
    SetCheckZero,
    SetSetTo,
    SetCheckAllGood,
    SetCheckCantHearRoom,
    SetCheckUntested,
    SetCheckAppleTv,
    SetCheckLive,
    SetFindLooking,
    SetFindFound,
    SetFindVpn,
    SetFindVpnLine,
    SetLookOnlyOn,
    SetFindPrivate,
    SetFindPublic,
    SetOpenNetworkSettings,
    SetFindFirewallOpen,
    SetFindFirewallBlocks,
    SetAllowPrivate,
    SetFindAirPlayFailed,
    SetFindFailed,
    SetLookEverywhere,
    SetFindSameNetwork,
    SetFindSameNetworkLine,
    SetAddByAddressShort,
    SetAppVersion,
    SetAppLine,
    SetDriverHeading,
    SetDriverRunning,
    SetDriverWaiting,
    SetDriverUntil,
    SetRestartNow,
    SetUpdatesHeading,
    SetDevBuild,
    SetNotChecked,
    SetChecking,
    SetUpToDate,
    SetCheckedAt,
    SetUpdateHow,
    SetUpdateHowPage,
    SetUpdateFailed,
    SetDownloading,
    SetClosesForSetup,
    SetClosesForSetupPc,
    SetNotSigned,
    SetUpdateNow,
    SetGetUpdate,
    SetWhatsNew,
    SetSkipVersion,
    SetInstall,
    SetCheckUpdatesNow,
    SetDailyCheck,
    SetDailyCheckLine,
    SetMoreHeading,
    SetSupport,
    SetSupportLine,
    SetDonate,
    SetPrivacyTitle,
    SetPrivacyLine,
    SetLicence,
    SetLicenceLine,
    SetViewSource,
    SetShowLab,
    SetShowLabLine,
    SetLabTitle,
    SetLabLine,
    SetLabReset,
    SetCarriedTitle,
    SetCarriedAirPlay,
    SetCarriedHomePod,
    SetCarriedBuffered,
    SetCarriedSpeakers,
    SetCarriedNoneKnown,
    SetCarriedReset,
    SetLabApply,
    SetLabApplyLine,
    SetLabGradually,
    SetLabAtOnce,
    SetLabCatchUp,
    SetLabCatchUpLine,
    SetLabFrames,
    SetLabDrift,
    SetLabDriftLine,
    SetLabPpm,
    SetLabPacing,
    SetLabPacingLine,
    SetLabMs,
    SetLabThisRun,
    SetLabConnections,
    SetLabAp2,
    SetLabAp2Line,
    SetLabAutomatic,
    SetLabRealtime,
    SetLabMfi,
    SetLabMfiLine,
    SetLabAlac,
    SetLabAlacLine,
    SetLabCounters,
    SetLabCountersLine,
    SetPairWith,
    SetOnYourTv,
    SetOnThisPc,
    SetLookAtTv,
    SetAskingForCode,
    SetCheckingCode,
    SetNoAnswerPairing,
    SetTimeRanOut,
    SetNewCode,
    SetCodeWrong,
    SetTypeCode,
    SetCodeName,
    SetGoesInBy,
    SetGoesIn,
    SetPairOnce,
    SetPasswordFor,
    SetAsksPassword,
    SetPasswordWrong,
    SetPasswordKept,
    SetPasswordOnce,
    SetIdentityChanged,
    SetSendspinForgot,
    SetSendspinWrong,
    SetSpeakerShowsCode,
    SetPasteToken,
    SetTokenName,
    SetNoCodeToken,
    SetUseCodeInstead,
    SetPair,
    SetMaPairTitle,
    SetInMa,
    SetMaPairStep1,
    SetMaPairStep2,
    SetMaPairStep3,
    SetCopyCode,
    SetWaitingForMa,
    SetWaitingForMaLeft,
    SetClosesItself,
    SetAddTitle,
    SetAddLead,
    SetAddress,
    SetAddressHint,
    SetFind,
    SetNameOptional,
    SetCheckingAddress,
    SetNothingAt,
    SetFoundNamed,
    SetFoundLine,
    SetFoundTwoWays,
    SetAddNamed,
    SetWayRecommended,
    SetWayPlain,
    SetByHand,
    SetByHandWhat,
    SetConnectsBy,
    SetPort,
    SetDescriptionAddress,
    SetAddButton,
    SetToneTitle,
    SetPreset,
    SetPresetFlat,
    SetPresetBassBoost,
    SetPresetBassCut,
    SetPresetTrebleBoost,
    SetPresetVoice,
    SetPresetLoudness,
    SetPresetSmallSpeaker,
    SetPresetCustom,
    SetHeadroom,
    SetPreamp,
    SetBandName,
    SetToneDialogLine,
    SetWhatHappened,
    SetReportIncludes,
    SetReportHour,
    SetReportHourLine,
    SetReportSpeakers,
    SetReportVersions,
    SetReportLog,
    SetReportLogLine,
    SetPreview,
    SetNothingSent,
    SetCopyReport,
    SetSaveReport,
    SetOpenIssue,
    SetReportCopied,
    SetReportSaved,
    SetIssueOpened,
    SetNothingChanged,
    SetChoiceSteady,
    SetChoiceShort,
    SetDetailStream,
    SetDetailBuffer,
    SetDetailLead,
    SetProbeNoThread,
    SetReportVersionLine,
    SetPairingEnded,
    SetSavePassword,
    SetSendToThis,
    SetPlayOnThis,
    SetForgetThis,

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
        SpokenPanelDelay => text("Delay on {speaker}: {figure}, {change}."),
        SpokenPanelDelayOwn => text("Delay on {speaker}: {figure}, nothing removed or added."),
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
        RefuseOnlyFromWindow => text("To play to “{speaker}”, pick it in the Stream To Speaker window."),
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

        // The Settings window and its dialogs.
        SetWindowTitle => text("Stream To Speaker — Settings"),
        SetNavSpeakers => text("Speakers"),
        SetNavGeneral => text("General"),
        SetNavPrivacy => text("Privacy & remote"),
        SetNavHelp => text("Help"),
        SetNavAbout => text("About"),
        SetNavLab => text("Lab"),
        SetKindAirPlay => text("AirPlay speaker"),
        SetUpnpKind => text("UPnP"),
        SetKindStatus => text("{kind} · {status}"),
        SetListAnd => text("{list} and {last}"),
        SetDate => text("{day} {month}"),
        SetJanuary => text("January"),
        SetFebruary => text("February"),
        SetMarch => text("March"),
        SetApril => text("April"),
        SetMay => text("May"),
        SetJune => text("June"),
        SetJuly => text("July"),
        SetAugust => text("August"),
        SetSeptember => text("September"),
        SetOctober => text("October"),
        SetNovember => text("November"),
        SetDecember => text("December"),
        SetToday => text("today {time}"),
        SetYesterday => text("yesterday {time}"),
        SetLeft => text("{time} left"),
        SetDbPlus => text("+{n} dB"),
        SetDbMinus => text("−{n} dB"),
        SetDbZero => text("0 dB"),
        SetCancel => text("Cancel"),
        SetClose => text("Close"),
        SetShow => text("Show"),
        SetHide => text("Hide"),
        SetCopy => text("Copy"),
        SetCopied => text("Copied"),
        SetChange => text("Change"),
        SetRemove => text("Remove"),
        SetDetails => text("Details"),
        SetNext => text("Next"),
        SetBackButton => text("Back"),
        SetTryAgain => text("Try again"),
        SetConnect => text("Connect"),
        SetReconnect => text("Reconnect"),
        SetCheckNow => text("Check now"),
        SetOpenPanel => text("Open the panel"),
        SetExperimental => text("experimental"),
        SetEnglish => text("English"),
        SetYourSpeakers => text("Your speakers"),
        SetNoSpeakersYet => text("No speakers found yet"),
        SetNoSpeakersYetLine => text("Speakers on this network show up here as they’re found."),
        SetHiddenHeading => text("Hidden ({count})"),
        SetHiddenLine => text("{kind} · hidden from the panel"),
        SetShowNamed => text("Show {speaker}"),
        SetFindingSpeakers => text("Finding speakers"),
        SetLookOn => text("Look for speakers on"),
        SetAllNetworks => text("All networks"),
        SetLookingOn => text("Looking on {networks}."),
        SetLookingPinned => text("Looking only from {address}, as the command line says."),
        SetNotLookingOff => text("Not looking: {adapter} is turned off. Speakers you added by address still work."),
        SetLookingNowhere => text("Not looking: this PC isn’t connected to a network."),
        SetNetworkNamed => text("{adapter} “{network}” ({address})"),
        SetNetworkPlain => text("{adapter} ({address})"),
        SetAddByAddress => text("Add a speaker by address"),
        SetSearchAgain => text("Search again"),
        SetCantFindLink => text("Can’t find your speaker?"),
        SetMoreKinds => text("More kinds of speakers"),
        SetSendspinSwitch => text("Sendspin speakers (experimental)"),
        SetSendspinLine => text("ESPHome and other speakers that play from Music Assistant. This PC plays to them directly."),
        SetPlayingNow => text("Playing now"),
        SetReadyNow => text("Ready"),
        SetConnectingNow => text("Connecting…"),
        SetNotFoundRow => text("not found right now"),
        SetNotFoundSeen => text("not found, last seen {seen}"),
        SetAddedByAddress => text("added by address"),
        SetCantPlayRow => text("this PC can’t play to it"),
        SetPlayingNowCap => text("Playing now"),
        SetReadyCap => text("Ready"),
        SetConnectingCap => text("Connecting…"),
        SetAvailableCap => text("Available"),
        SetNotFoundCap => text("Not found right now"),
        SetPlayOn => text("Play on {speaker}"),
        SetPlayingButton => text("Playing"),
        SetConnectingButton => text("Connecting…"),
        SetTestSound => text("Play a test sound"),
        SetTestSoundOn => text("Play a test sound on {speaker}"),
        SetCantPlayTitle => text("This PC can’t play to {speaker}."),
        SetCantPlayFix => text("Turn on AirPlay in the Apple TV’s settings, then search again."),
        SetUntestedPairTitle => text("Playing to HomePod pairs hasn’t been tested on real HomePods yet."),
        SetUntestedPairLine => text("If {speaker} stays silent, please tell us: the report says what this PC saw."),
        SetReportProblem => text("Report a problem"),
        SetDelayHeading => text("Delay"),
        SetOwnDelay => text("{speaker}’s own delay is {delay}."),
        SetSonosOwn => text("Sonos speakers set their own delay."),
        SetSendspinOwn => text("The speaker says how much it needs; delay can only be added to it."),
        SetKeepsAsked => text("{speaker} keeps the delay this PC asks it for."),
        SetPairAlwaysShort => text("HomePod pairs always play with a short delay ({delay})."),
        SetMeasuredOn => text("{speaker} played {seconds} s after your PC when you checked on {date}."),
        SetShortDelayAbout => text("Short delay — {delay}"),
        SetShortDelayLine => text("Better for video; may drop out on busy Wi-Fi."),
        SetSteadyAbout => text("Steady — {delay}"),
        SetSteadyLine => text("Rides out Wi-Fi hiccups; fine for music."),
        SetChoiceReconnects => text("Changing this reconnects {speaker}: a few seconds of silence. Each choice keeps what you removed or added."),
        SetSymptoms => text("Sound behind the picture? Remove some delay. Breaking up? Add some."),
        SetRemembered => text("Remembered for {speaker} and used every time it plays."),
        SetAppliesLive => text("Remembered for {speaker} and used every time it plays. On {speaker} a change applies while it plays, without a gap."),
        SetAppliesAtOnce => text("Remembered for {speaker} and used every time it plays. On {speaker} a change applies at once, with a short skip."),
        SetAppliesReconnect => text("Remembered for {speaker} and used every time it plays. On {speaker} a change applies by reconnecting: a few seconds of silence."),
        SetAppliesPair => text("One change for both HomePods. On {speaker} a change applies by reconnecting both: a few seconds of silence."),
        SetRange => text("You can remove up to {remove} ms of {speaker}’s delay and add up to {add} ms."),
        SetRangeSteady => text("On Steady, you can remove up to {remove} ms of {speaker}’s delay and add up to {add} ms. For less delay, choose Short delay."),
        SetRangeShort => text("On Short delay, you can remove up to {remove} ms of {speaker}’s delay and add up to {add} ms."),
        SetRangeAddOnly => text("You can add up to {add} ms to {speaker}’s delay."),
        SetShortcutsOnLine => text("Watching full screen? {remove} removes delay and {add} adds some, in any app."),
        SetShortcutsOffLine => text("Watching full screen? Remove or add delay with {remove} and {add} — turn the shortcuts on in General."),
        SetConnectsWith => text("Connects with"),
        SetWaySonos => text("Sonos — {delay} delay (recommended)"),
        SetWayAirPlay => text("AirPlay — {delay} delay"),
        SetWayAirPlayLine => text("Shows song titles in the Sonos app. Use it if the Sonos connection stays silent."),
        SetWaysKeptApart => text("What you remove or add is kept separately for each."),
        SetGroupHeading => text("Group"),
        SetSonosGroupLine => text("Plays on {rooms} together. The volume moves the whole group, like the group slider in the Sonos app. Groups are made in the Sonos app; to play in one room, ungroup it there first."),
        SetPairLine => text("{left} and {right} play together, each its own channel; the volume moves both. Pairs are made in the Home app."),
        SetAppleTvGroup => text("Apple TV with HomePods"),
        SetAppleTvGroupLine => text("{speaker} plays through {members}. Groups are made in the Home app."),
        SetVolumeLimit => text("Volume limit"),
        SetNeverLouder => text("Never louder than"),
        SetNoLimit => text("No limit"),
        SetVolumeLimitLine => text("Volume keys and other apps can’t go above this. At 100 there is no limit."),
        SetToneHeading => text("Tone"),
        SetBass => text("Bass"),
        SetTreble => text("Treble"),
        SetAllBands => text("All bands…"),
        SetUndo => text("Undo"),
        SetResetFlat => text("Reset to flat"),
        SetToneLine => text("Tone belongs to {speaker}, not to the PC. Changes play and are kept at once; Undo reverses the last one."),
        SetConnectionHeading => text("Connection"),
        SetPaired => text("Paired with this PC"),
        SetPairedOn => text("Paired with this PC on {date}"),
        SetPairedLine => text("Pair again if {speaker} was reset."),
        SetPairAgain => text("Pair again"),
        SetForgetPairing => text("Forget pairing"),
        SetPasswordTitle => text("AirPlay password"),
        SetPasswordSaved => text("Saved on this PC."),
        SetRemovePassword => text("Remove the AirPlay password"),
        SetChangePassword => text("Change the AirPlay password"),
        SetComesBack => text("Comes back by itself"),
        SetComesBackLast => text("When it was the last place you chose"),
        SetComesBackAppears => text("Whenever it appears and nothing else is playing"),
        SetComesBackNever => text("Never"),
        SetComesBackLastLine => text("Your sound returns to {speaker} after a restart, sleep or dropout, when nothing else is playing."),
        SetComesBackAppearsLine => text("Your sound goes to {speaker} whenever it appears and nothing else is playing."),
        SetComesBackNeverLine => text("Your sound goes to {speaker} only when you choose it."),
        SetSongTitles => text("Show song titles on {speaker}"),
        SetSongTitlesLine => text("Sends the title and artist of what’s playing on this PC to {speaker}’s display or app."),
        SetShowAs => text("Show as"),
        SetShowAsSonos => text("The name in this app only. The Sonos app keeps its own name."),
        SetShowAsHome => text("The name in this app only. The Home app keeps its own name."),
        SetShowAsLine => text("The name in this app only. The speaker keeps its own name."),
        SetDetailsLine => text("Model, address, connection type and id, the values behind the delay, and the last problem — for support."),
        SetDetailModel => text("Model: {value}"),
        SetDetailAddress => text("Address: {value}"),
        SetDetailConnection => text("Connection: {value}"),
        SetDetailDelay => text("Delay: {value}"),
        SetDetailProblem => text("Last problem: {value}"),
        SetDetailId => text("Id: {value}"),
        SetDetailNone => text("none"),
        SetCopyDetails => text("Copy details"),
        SetForgetNamed => text("Forget {speaker}"),
        SetRemoveNamed => text("Remove {speaker}"),
        SetHideFromList => text("Hide from the list"),
        SetForgetLine => text("Forget removes {speaker}’s pairing, password, volume, delay changes, tone and “comes back” choice, and nothing else."),
        SetRemoveLine => text("Remove takes {speaker} out of your list, with everything kept for it."),
        SetWhatPlays => text("What plays on your speaker"),
        SetEverything => text("Everything this PC plays (recommended)"),
        SetEverythingLine => text("Choosing a speaker makes it Windows’ sound output; choosing This PC switches back."),
        SetPlayCalls => text("Play calls on the speaker too"),
        SetPlayCallsLine => text("Works with call apps that use Windows’ communications device."),
        SetOnlySent => text("Only apps I send there"),
        SetOnlySentLine => text("In Windows’ Volume mixer, set an app’s output to “Stream To Speaker”."),
        SetOpenVolumeMixer => text("Open Volume mixer"),
        SetStartUp => text("Start-up"),
        SetStartAtSignIn => text("Start Stream To Speaker when I sign in"),
        SetStartAtSignInLine => text("It waits by the clock without opening anything."),
        SetSignInFailed => text("Couldn’t change the start-at-sign-in setting: {why}"),
        SetGoBack => text("…and go back to the speaker I was using"),
        SetGoBackLine => text("Only when nothing else is playing and no one else is using it."),
        SetWhenWrong => text("When something goes wrong"),
        SetWhenStops => text("When a speaker stops responding"),
        SetBringBack => text("Bring the sound back to this PC (recommended)"),
        SetBringBackLine => text("After three quick tries, about 15 seconds."),
        SetStaySilent => text("Keep trying, and stay silent meanwhile"),
        SetStaySilentLine => text("Tries every 30 seconds. Nothing plays on this PC until the speaker is back."),
        SetNotify => text("Tell me when sound leaves the place I chose"),
        SetNotifyLine => text("A notification, only while the panel is closed."),
        SetShortcutsHeading => text("Keyboard shortcuts"),
        SetShortcutsSwitch => text("Remove or add delay from the keyboard, in any app"),
        SetShortcutsLine => text("Works during full-screen video. Each press shows a small confirmation by the clock."),
        SetShortcutRemove => text("Remove {ms} ms"),
        SetShortcutAdd => text("Add {ms} ms"),
        SetHold => text("Hold"),
        SetForLarge => text("for {ms} ms."),
        SetChangeKeys => text("Change…"),
        SetPressKeys => text("Press the new keys…"),
        SetShortcutReady => text("Ready"),
        SetShortcutTaken => text("Another program uses this shortcut. Choose another."),
        SetLanguage => text("Language"),
        SetLanguageWindows => text("Windows default ({language})"),
        SetMaLead => text("Send this PC’s sound to Music Assistant, which can play it on any speaker it controls."),
        SetMaOffForRun => text("Music Assistant is turned off for this run of Stream To Speaker (it was started with --no-sendspin)."),
        SetMaOffer => text("Offer this PC to Music Assistant"),
        SetMaOfferLine => text("Makes this PC’s sound available in Music Assistant as a live input."),
        SetMaFailed => text("Couldn’t start offering this PC to Music Assistant: {why}"),
        SetMaPausedTitle => text("Pairing with a code is paused because wrong codes were entered too many times."),
        SetMaPausedLine => text("If that wasn’t you, something on your network may be trying to pair."),
        SetMaAllowAgain => text("Allow pairing again"),
        SetMaSettingUp => text("Setting it up"),
        SetMaStep1 => text("Music Assistant finds this PC"),
        SetMaStep1Line => text("In Music Assistant, add the Sendspin source provider. This PC appears there as “{pc}”."),
        SetMaFound => text("Found by {server}. This PC appears there as “{pc}”."),
        SetMaFoundAt => text("Found by {server} at {address}. This PC appears there as “{pc}”."),
        SetMaFoundBefore => text("Found by Music Assistant. This PC appears there as “{pc}”."),
        SetMaStep2 => text("Pair"),
        SetMaStep2Line => text("In Music Assistant, open Settings → Players → “{pc}” and start its setup. A 6-digit code then appears here; type it there."),
        SetMaPaired => text("Paired with {server}."),
        SetMaUnpair => text("Unpair"),
        SetMaUnpairNamed => text("Unpair {server}"),
        SetMaStep3 => text("Choose where it plays"),
        SetMaStep3Line => text("In Music Assistant, set “Automatically play line-in on” for “{pc}”, or start it from Browse."),
        SetMaPlayed => text("Music Assistant has played this PC’s sound."),
        SetMaPlayedOn => text("Music Assistant played this PC’s sound on {date}."),
        SetMaTicks => text("Each step ticks itself when this PC sees it happen."),
        SetMaAllDone => text("Music Assistant is in your speaker list. Pick it there to send this PC’s sound; it gets nothing otherwise."),
        SetOpenMa => text("Open Music Assistant"),
        SetMaToken => text("Pairing with another Sendspin server?"),
        SetMaTokenLine => text("Music Assistant uses the 6-digit code above. Use this token only if another Sendspin server asks for one."),
        SetCopyToken => text("Copy token"),
        SetMaAppearsAs => text("This PC appears as"),
        SetMaAppearsAsLine => text("The name Music Assistant shows for this PC."),
        SetMaDelayLine => text("Music Assistant sets the delay for each of its speakers, so it isn’t changed here."),
        SetWhoCanListen => text("Who can listen"),
        SetOnlyPlaying => text("Only the speaker you’re playing on (recommended)"),
        SetOnlyPlayingLine => text("Speakers fetch this PC’s sound only while you play on them, including every speaker in a Sonos group."),
        SetAnyDevice => text("Any device on this network that asks"),
        SetAnyDeviceLine => text("Anyone on this network who knows the address can listen to everything this PC plays."),
        SetRecentlyRefused => text("Recently refused"),
        SetRefusedNamed => text("{name} ({address}) · {time}"),
        SetRefusedUnknown => text("{address} (unknown device) · {time}"),
        SetAllowFor => text("Allow for {speaker}"),
        SetNotPartOfSpeaker => text("Not part of a speaker"),
        SetFromElsewhere => text("Control from elsewhere"),
        SetPhone => text("Control Stream To Speaker from your phone"),
        SetPhoneLine => text("Your phone’s browser shows where the sound plays, the volume and the delay."),
        SetScanOrOpen => text("Open this link in your phone’s browser:"),
        SetCopyLink => text("Copy link"),
        SetNewLink => text("Make a new link"),
        SetLinkLine => text("Anyone with this link can control Stream To Speaker on this network. A new link stops the old one working."),
        SetApi => text("Let scripts and home automation control this app"),
        SetApiLine => text("When on, the API answers only requests that carry this key. Scripts written for earlier versions need it too."),
        SetApiHow => text("How to use the API"),
        SetCopyKey => text("Copy key"),
        SetNewKey => text("Make a new key"),
        SetCantHearLine => text("Checks, step by step, where the sound stops, and offers the one fix for that step."),
        SetCheckPlayback => text("Check where the sound stops"),
        SetCantFindLine => text("Checks the network, Windows’ firewall and VPNs, and says what it found."),
        SetCheckFinding => text("Check why a speaker isn’t found"),
        SetReportLine => text("Shows exactly what’s included. Nothing is sent until you choose where."),
        SetWriteReport => text("Write a report"),
        SetLogs => text("Logs"),
        SetLogsLine => text("Kept in your user folder. Included in a report only if you choose."),
        SetOpenLogs => text("Open log folder"),
        SetHowItWorks => text("How it works"),
        SetHowItWorksLine => text("Where your sound goes, what the delay is and how removing or adding milliseconds works, why This PC is the off switch, what Music Assistant does."),
        SetReadOnline => text("Read the guide online"),
        SetShowWelcome => text("Show the welcome again"),
        SetGuidePage => text("{n} of {of}"),
        SetGuideWhereTitle => text("Where your sound goes"),
        SetGuideWhere => text("Your PC’s sound plays in one place at a time: on This PC, or on one of your speakers. Pick a speaker and everything the PC plays comes out there; pick This PC and the sound comes back."),
        SetGuideDelayTitle => text("What the delay is"),
        SetGuideDelay => text("Every speaker keeps a little of your PC’s sound waiting before it plays it. That is its delay: about 0.2 s for a Sonos, about 2 s for an AirPlay speaker on Steady. Removing delay is like cutting some of the waiting sound: the speaker catches up with the picture, but has less in hand when the Wi-Fi hiccups. Adding delay is like putting a little silence in front of the sound: more waits, so the speaker rides out hiccups better. Each speaker remembers what you removed or added."),
        SetGuideOffTitle => text("Why This PC is the off switch"),
        SetGuideOff => text("Choosing This PC gives the sound back to the PC’s own output at once and lets the speaker go. There is nothing else to turn off: quitting the app, signing out or shutting down gives the sound back too. Calls stay on the PC unless you choose otherwise in General, and apps with their own speaker setting follow that setting."),
        SetGuideRoomsTitle => text("Several rooms at once"),
        SetGuideRooms => text("To play in several Sonos rooms, group them in the Sonos app; to play on two HomePods, pair them in the Home app. The group or pair then shows up here as one speaker. Speakers from different makers can’t play together."),
        SetGuideAirPlayTitle => text("What this PC can and can’t tell"),
        SetGuideBackTitle => text("Coming back by itself"),
        SetGuideBack => text("A speaker set to come back “when it was the last place you chose” gets your sound again after a restart, sleep or dropout, but only when nothing else is playing on this PC and no one else is using the speaker. Otherwise the panel asks first."),
        SetGuideAirPlay => text("A Sonos shows that it plays by fetching the sound, so “Playing on Kitchen” means it is taking it. AirPlay speakers and HomePods only accept the sound and report faults, so for them it means connected, sending, and no fault. Can’t hear it? says so instead of guessing."),
        SetGuideMaTitle => text("What Music Assistant does"),
        SetGuideMa => text("Music Assistant can play this PC’s sound on any speaker it controls, and chooses the speaker and its delay itself. It gets the sound only while you pick it in the panel; until then it gets nothing. Set it up in Settings → Music Assistant."),
        SetCheckLead => text("Let’s find where the sound stops."),
        SetCheckOnPc => text("Your sound is on this PC."),
        SetCheckOnPcLine => text("Pick a speaker in the panel; this check then follows the sound there."),
        SetCheckPlaying => text("Something is playing on this PC."),
        SetCheckAppPlays => text("{app} is playing."),
        SetCheckWindows => text("Windows sends it to Stream To Speaker."),
        SetCheckWindowsElsewhere => text("Windows sends sound to {output}."),
        SetSendTo => text("Send it to {speaker}"),
        SetCheckAppElsewhere => text("{app} is still playing on {output}."),
        SetCheckRestartApp => text("Some apps move to a new output only after you restart them."),
        SetCheckConnected => text("Connected to {speaker}."),
        SetCheckNotConnected => text("Not connected."),
        SetCheckTaking => text("{speaker} is taking the sound."),
        SetCheckNotAsking => text("{speaker} accepted the connection but isn’t asking for the sound."),
        SetTryAirPlay => text("Try the AirPlay connection"),
        SetCheckRefused => text("{device}, in {speaker}’s group, asked for the sound and was refused."),
        SetAllowIt => text("Allow it"),
        SetCheckCantTell => text("This PC can’t tell whether {speaker} is playing."),
        SetCheckCantTellLine => text("HomePods and other AirPlay speakers accept the stream and report faults, but not whether they play."),
        SetTrySteady => text("Try Steady instead"),
        SetCheckVolume => text("{speaker}’s volume."),
        SetCheckVolumeLevel => text("{level}, not muted."),
        SetCheckMuted => text("{speaker} is muted."),
        SetCheckZero => text("{speaker}’s volume is 0."),
        SetSetTo => text("Set to {level}"),
        SetCheckAllGood => text("Everything this PC can check looks right."),
        SetCheckCantHearRoom => text("This PC can’t hear the room. If {speaker} is still silent, the speaker itself may be the problem."),
        SetCheckUntested => text("Playing to HomePod pairs hasn’t been tested on real HomePods yet; a report helps."),
        SetCheckAppleTv => text("Apple TVs that play through HomePods may not pass this PC’s sound on. Pick the HomePods instead."),
        SetCheckLive => text("The check runs live and ticks again as things change. It stops at the first broken step; a step this PC can’t know is said so, and the check goes on."),
        SetFindLooking => text("Looking on {network}."),
        SetFindFound => plural("count", "Found {count} speaker there.", "Found {count} speakers there."),
        SetFindVpn => text("This PC is also connected to {network}."),
        SetFindVpnLine => text("If your speakers aren’t on that network, looking there can hide them."),
        SetLookOnlyOn => text("Look only on {adapter}"),
        SetFindPrivate => text("Windows treats “{network}” as a private network."),
        SetFindPublic => text("Windows treats “{network}” as a public network, so speakers can’t answer."),
        SetOpenNetworkSettings => text("Open network settings"),
        SetFindFirewallOpen => text("Windows Firewall lets speakers reach this PC."),
        SetFindFirewallBlocks => text("Windows Firewall blocks Stream To Speaker on this network."),
        SetAllowPrivate => text("Allow on private networks"),
        SetFindAirPlayFailed => text("AirPlay speakers can’t be found right now (the search couldn’t start). You can still add them by address."),
        SetFindFailed => text("Searching failed on {adapter}."),
        SetLookEverywhere => text("Look on all networks"),
        SetFindSameNetwork => text("Make sure the speaker is on and on the same network as this PC."),
        SetFindSameNetworkLine => text("Guest Wi-Fi usually keeps devices apart. A speaker on another network can be added by its address."),
        SetAddByAddressShort => text("Add by address…"),
        SetAppVersion => text("Stream To Speaker {version}"),
        SetAppLine => text("Plays your PC’s sound on speakers in your home."),
        SetDriverHeading => text("Sound output (driver)"),
        SetDriverRunning => text("Sound output {version} is running."),
        SetDriverWaiting => text("Sound output {version} is installed and starts after you restart Windows."),
        SetDriverUntil => text("Until then {version} keeps working."),
        SetRestartNow => text("Restart now…"),
        SetUpdatesHeading => text("Updates"),
        SetDevBuild => text("Development build — update checks are off."),
        SetNotChecked => text("Not checked yet."),
        SetChecking => text("Checking for updates…"),
        SetUpToDate => text("You have the latest version."),
        SetCheckedAt => text("Checked {when}."),
        SetUpdateHow => text("Update now downloads the installer, has Windows check its signature and that it comes from this project, then installs it. Your sound plays on this PC for a moment."),
        SetUpdateHowPage => text("Its page has the installer."),
        SetUpdateFailed => text("Couldn’t check for updates."),
        SetDownloading => text("Downloading the update and checking its signature…"),
        SetClosesForSetup => text("Stream To Speaker closes while Setup runs. Your sound plays on this PC meanwhile and goes back to {speaker} when it’s done."),
        SetClosesForSetupPc => text("Stream To Speaker closes while Setup runs, and opens again when it’s done."),
        SetNotSigned => text("This download isn’t signed by Stream To Speaker, so it won’t be run."),
        SetUpdateNow => text("Update now"),
        SetGetUpdate => text("Get the update"),
        SetWhatsNew => text("What’s new"),
        SetSkipVersion => text("Skip this version"),
        SetInstall => text("Install"),
        SetCheckUpdatesNow => text("Check for updates now"),
        SetDailyCheck => text("Check for updates once a day"),
        SetDailyCheckLine => text("Sends only the app’s version to GitHub. Nothing installs without your click."),
        SetMoreHeading => text("More"),
        SetSupport => text("Support development"),
        SetSupportLine => text("Stream To Speaker is free and open source. Donations pay for signing its sound driver."),
        SetDonate => text("Donate"),
        SetPrivacyTitle => text("Privacy"),
        SetPrivacyLine => text("No usage data is collected."),
        SetLicence => text("Licence and source"),
        SetLicenceLine => text("Source: MPL-2.0. Released installers: free to install and use."),
        SetViewSource => text("View the source"),
        SetShowLab => text("Show Lab (experimental settings)"),
        SetShowLabLine => text("Settings for testing speakers that aren’t fully supported yet."),
        SetLabTitle => text("Settings for testing speakers that aren’t fully supported yet."),
        SetLabLine => text("They can stop a speaker from playing. While any of them differs from its default, the panel says “Lab settings are on”."),
        SetLabReset => text("Reset everything in Lab"),
        SetCarriedTitle => text("Carried over from an earlier version"),
        SetCarriedAirPlay => text("AirPlay buffer: {ms} ms (the default is 2000 ms)."),
        SetCarriedHomePod => text("HomePod delay: {ms} ms (the default is 200 ms)."),
        SetCarriedBuffered => text("Single HomePods use the buffered stream."),
        SetCarriedSpeakers => text("They still give these speakers their delay: {speakers}. Their sound changes if these go back to the defaults."),
        SetCarriedNoneKnown => text("They give AirPlay speakers whose delay you haven’t changed here their delay; none is known yet."),
        SetCarriedReset => text("Use the defaults"),
        SetLabApply => text("Apply delay changes on Sonos and UPnP speakers"),
        SetLabApplyLine => text("Gradually, without a gap — or at once, with a short skip."),
        SetLabGradually => text("Gradually"),
        SetLabAtOnce => text("At once"),
        SetLabCatchUp => text("Catch-up step"),
        SetLabCatchUpLine => text("Frames added or dropped per packet while a change applies. Larger is faster but can click."),
        SetLabFrames => plural("count", "{count} frame", "{count} frames"),
        SetLabDrift => text("Clock drift"),
        SetLabDriftLine => text("A fixed correction for a speaker whose clock runs fast or slow."),
        SetLabPpm => text("{ppm} ppm"),
        SetLabPacing => text("Silence pacing"),
        SetLabPacingLine => text("Wall-clock ms per 10 ms of injected silence. Above 10 drains the speaker’s prebuffer."),
        SetLabMs => text("{ms} ms"),
        SetLabThisRun => text("The catch-up step, clock drift and silence pacing last until Stream To Speaker quits."),
        SetLabConnections => text("Connections"),
        SetLabAp2 => text("AirPlay 2 stream"),
        SetLabAp2Line => text("Normally follows each speaker’s delay choice. Some speakers accept the realtime handshake but never play it."),
        SetLabAutomatic => text("Automatic"),
        SetLabRealtime => text("Realtime"),
        SetLabMfi => text("Try MFi encryption (et=4) first"),
        SetLabMfiLine => text("A refused attempt can make the speaker unresponsive for up to half a minute."),
        SetLabAlac => text("Send uncompressed ALAC"),
        SetLabAlacLine => text("For testing receivers that misbehave with compressed audio."),
        SetLabCounters => text("Live counters"),
        SetLabCountersLine => text("Packets, listeners, resends, the last problem and the counters just before it."),
        SetPairWith => text("Pair with {speaker}"),
        SetOnYourTv => text("On your Apple TV"),
        SetOnThisPc => text("On this PC"),
        SetLookAtTv => text("Look at {speaker}: it’s showing a 4-digit code now. A wrong try makes it show a new one."),
        SetAskingForCode => text("Asking {speaker} to show a code…"),
        SetCheckingCode => text("Checking the code…"),
        SetNoAnswerPairing => text("{speaker} stopped responding, so pairing couldn’t start."),
        SetTimeRanOut => text("Time ran out."),
        SetNewCode => text("Show a new code"),
        SetCodeWrong => text("That code didn’t work. {speaker} is showing a new code."),
        SetTypeCode => text("Type the 4-digit code shown on {speaker}."),
        SetCodeName => text("Code"),
        SetGoesInBy => text("{left} · it goes in by itself after the fourth digit"),
        SetGoesIn => text("It goes in by itself after the fourth digit."),
        SetPairOnce => text("You do this once. The pairing is remembered."),
        SetPasswordFor => text("Password for {speaker}"),
        SetAsksPassword => text("{speaker} asks for its AirPlay password."),
        SetPasswordWrong => text("That password didn’t work. You can check it in the speaker’s own settings."),
        SetPasswordKept => text("Saved on this PC for next time."),
        SetPasswordOnce => text("Used for this connection only: another speaker answers with the same identity, so it isn’t saved."),
        SetIdentityChanged => text("{speaker} answers with a different identity than the speaker this PC was paired with. Pair again only if it’s yours."),
        SetSendspinForgot => text("{speaker} forgot this PC’s pairing. Pair it again with its code."),
        SetSendspinWrong => text("That code didn’t work. Check it on the speaker and type it again."),
        SetSpeakerShowsCode => text("{speaker} is showing or saying a code. Type it here."),
        SetPasteToken => text("Paste {speaker}’s pairing token here. It starts with SP:"),
        SetTokenName => text("Pairing token"),
        SetNoCodeToken => text("No code? Paste its pairing token (starts with SP:)"),
        SetUseCodeInstead => text("Type a code instead"),
        SetPair => text("Pair"),
        SetMaPairTitle => text("Pair this PC with Music Assistant"),
        SetInMa => text("In Music Assistant"),
        SetMaPairStep1 => text("1. Open Settings › Players"),
        SetMaPairStep2 => text("2. Pick {pc}"),
        SetMaPairStep3 => text("3. Start its setup and type the code"),
        SetCopyCode => text("Copy code"),
        SetWaitingForMa => text("Waiting for Music Assistant…"),
        SetWaitingForMaLeft => text("Waiting for Music Assistant… {left}"),
        SetClosesItself => text("This window closes by itself when pairing finishes."),
        SetAddTitle => text("Add a speaker by address"),
        SetAddLead => text("For a speaker on another network, or one that doesn’t show up by itself. It stays in your list until you remove it."),
        SetAddress => text("Address"),
        SetAddressHint => text("192.168.1.20 or kitchen.local"),
        SetFind => text("Find"),
        SetNameOptional => text("Name (optional)"),
        SetCheckingAddress => text("Checking {address}…"),
        SetNothingAt => text("Nothing at {address} answered as a speaker. Check the address and that the speaker is on."),
        SetFoundNamed => text("Found {speaker}"),
        SetFoundLine => text("{kind} at {address} · {delay} delay"),
        SetFoundTwoWays => text("Found two ways to play there:"),
        SetAddNamed => text("Add {speaker}"),
        SetWayRecommended => text("{way} (recommended)"),
        SetWayPlain => text("{way}"),
        SetByHand => text("Enter the details yourself…"),
        SetByHandWhat => text("(type, port, UPnP description address)"),
        SetConnectsBy => text("Type"),
        SetPort => text("Port"),
        SetDescriptionAddress => text("Description address"),
        SetAddButton => text("Add"),
        SetToneTitle => text("{speaker}’s tone"),
        SetPreset => text("Preset"),
        SetPresetFlat => text("Flat"),
        SetPresetBassBoost => text("Bass boost"),
        SetPresetBassCut => text("Bass cut"),
        SetPresetTrebleBoost => text("Treble boost"),
        SetPresetVoice => text("Voice"),
        SetPresetLoudness => text("Loudness"),
        SetPresetSmallSpeaker => text("Small speaker"),
        SetPresetCustom => text("Custom"),
        SetHeadroom => text("Automatic headroom {db} keeps loud passages from distorting."),
        SetPreamp => text("Preamp"),
        SetBandName => text("{band} Hz"),
        SetToneDialogLine => text("Changes play live and are kept as you go. Closing keeps what you hear."),
        SetWhatHappened => text("What happened?"),
        SetReportIncludes => text("The report includes"),
        SetReportHour => text("What happened in the last hour"),
        SetReportHourLine => text("States, connection attempts, faults and delay changes, with times"),
        SetReportSpeakers => text("Speaker names, kinds and addresses"),
        SetReportVersions => text("Versions of the app, the sound driver and Windows"),
        SetReportLog => text("The log file"),
        SetReportLogLine => text("Larger; may contain more addresses"),
        SetPreview => text("Preview"),
        SetNothingSent => text("Nothing is sent until you choose where. Open a GitHub issue copies the report and opens a new issue in your browser, for you to paste it and read it first."),
        SetCopyReport => text("Copy report"),
        SetSaveReport => text("Save as a file"),
        SetOpenIssue => text("Open a GitHub issue"),
        SetReportCopied => text("Copied. Paste it into an e-mail or a message."),
        SetReportSaved => text("Saved in your log folder, which is open now."),
        SetIssueOpened => text("The report is copied and a new issue is open in your browser: paste it there. Nothing is sent until you submit it."),
        SetNothingChanged => text("nothing removed or added"),
        SetChoiceSteady => text("Steady"),
        SetChoiceShort => text("Short delay"),
        SetDetailStream => text("{change} on the stream this PC serves"),
        SetDetailBuffer => text("AirPlay buffer {buffer} ms ({choice} {base} ms, {change})"),
        SetDetailLead => text("extra lead {change}"),
        SetProbeNoThread => text("Couldn’t start looking (no thread available). Try again."),
        SetReportVersionLine => text("Stream To Speaker {version} · {os}"),
        SetPairingEnded => text("That pairing had already ended. Pick the speaker again to pair it."),
        SetSavePassword => text("Save"),
        SetSendToThis => text("Send it there"),
        SetPlayOnThis => text("Play on this speaker"),
        SetForgetThis => text("Forget this speaker"),

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
