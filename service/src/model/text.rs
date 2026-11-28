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
    ThisPc,
    MaName,
    CantHear,
    SpeakerMissing,
    Settings,
    Unmute,

    // First run.
    FirstPickTitle,
    FirstPickAppsTitle,
    FirstSwitchAppsTitle,
    FirstAppsTitle,
    ClickSpeakerApps,
    ChosenAppsOn,
    ChosenAppsNowOn,
    ChosenAppsMoveSoon,
    ChosenAppsWhenStarting,
    ChosenAppsMoved,
    FirstNoChimeApps,
    FirstHearApps,
    FirstPickChime,
    ChimeOnSpeaker,
    FirstSwitchTitle,
    FirstNoChime,
    FirstHearTitle,
    FirstHearLine,
    FirstChimeSent,
    LookingForMore,
    FirstVolume,
    PlayAgain,
    UseWithoutChecking,
    ChooseAnotherSpeaker,
    FirstPlayingTitle,
    FirstStays,
    FirstIcon,
    HeardIt,
    DidntHear,
    Done,

    // The Now card.
    PlayingTitle,
    ReadyLine,
    NotRespondingTitle,
    RefusedTitle,

    // The panel's top.

    // First run.
    Close,

    // Buttons.
    Cancel,
    TrySpeakerAgain,
    ChooseApps,
    MoreSpeakersEllipsis,
    OpenShort,
    Reset,

    // Rows.
    KindSonos,
    KindSonosGroup,
    KindHomePod,
    KindHomePodPair,
    KindAppleTv,
    KindAirPlay,
    KindSendspin,
    KindSpeaker,
    RowNotFoundShort,
    RowWithAddress,
    RowRepairFirst,
    RowMoreSpeakers,
    Searching,
    BusyTitle,
    BusyOtherPcTitle,

    // Delay.
    DelayLabel,
    FigureAbout,
    ChangeRemoved,
    ChangeAdded,
    RemoveMs,
    AddMs,
    RemoveBare,
    AddBare,
    MsUnit,
    Removing,
    Adding,
    SoundBehind,
    SoundBreakingUp,

    // The tray.

    // The phone remote and the command line.
    PlaceStatus,
    CliPartVolume,
    CliPartMuted,
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

    // Windows' name for the sound output.
    DeviceForPlace,
    DeviceNotRunning,

    // The confirmation of a delay shortcut.

    // Notifications.
    NotifyStoppedTitle,
    NotifyTakenTitle,

    // The notice line.
    NoticeUpdate,

    // The situation-based panel and the views by the clock.
    ThisPcInSentence,
    VolumeNumber,
    PlaceVolume,
    MutePlace,
    UnmutePlace,
    RetryNoNetwork,
    WaitForNetwork,
    RetryNotResponding,
    Connecting,
    PausedOnSpeaker,
    NotPlayingYet,
    NotResponding,
    NoNetworkConnection,
    PlayingInJoinedRoom,
    PlayingExcept,
    AppPlaysOnPc,
    Why,
    PlayingChosenApps,
    OnlyChosenApps,
    Playing,
    NothingToPlayOn,
    NothingPlaying,
    NoOutputConnected,
    SwitchToPc,
    PcOutputs,
    SwitchToSpeaker,
    PlayPcSoundOnSpeaker,
    SwitchByWay,
    SpeakerGear,
    MissingWhySpoken,
    RowPlayingElse,
    MayBeInUse,
    SpokenConnecting,
    SpokenNotResponding,
    SpokenNotFound,
    SpokenPlayingElse,
    SpokenMayBeInUse,
    SpokenRepairFirst,
    FoundOnWifi,
    FoundOnNetwork,
    NoSpeakersOnNetwork,
    ClickSpeaker,
    ConnectingFromPlace,
    ConnectingToSpeaker,
    CancelConnecting,
    EnterTheCode,
    EnterThePassword,
    SwitchToPcNow,
    MoveInSeconds,
    MoveWhenSoundStarts,
    LostWifi,
    LostNetwork,
    PcNotConnected,
    SpeakerNotFound,
    PlayingOtherPc,
    GroupedWith,
    StoppedPlayingPc,
    AvailableAgain,
    StaysUntilSwitch,
    AtTime,
    MovedToPc,
    MovedAfterVpn,
    MovedAfterConnection,
    DidntRespond,
    AsksForCode,
    NeedsCode,
    NeedsPassword,
    AsksForPassword,
    SwitchingBackStops,
    DeviceTookOver,
    MovedAfterGroup,
    YouRemoved,
    SoundStayedHere,
    PlayingAtStartup,
    OutputDisconnected,
    MovedToPcSpeakers,
    MovedToOutput,
    OutputConnected,
    FromSpeakerHeadphones,
    FromSpeakerOutput,
    QuietForThirty,
    MovedFromSpeaker,
    SoundStillOn,
    CodeDidntWork,
    PasswordDidntWork,
    CouldntStartPlaying,
    RefusedConnection,
    AppOutputMissing,
    RestoreOutputLine,
    RestoreInSettings,
    RepairOutput,
    OutputTurnedOff,
    TurnOnOutputLine,
    TurnOnFailedLine,
    RepairingOutput,
    StaysMeanwhile,
    RepairDidntWork,
    RestartForOutput,
    RepairNeedsApproval,
    ApprovalNotGiven,
    OutputIsBack,
    CanSwitchAgain,
    WindowsDidntSwitchLine,
    OpenWindowsSound,
    MaNotStarted,
    MaPlaceLine,
    MaStartPcSound,
    HowToStart,
    SpeakerWasPaused,
    PausedBySpeaker,
    MicInUse,
    HearCallLate,
    ChimeStopsIt,
    PlayTestChime,
    SwitchStopsSound,
    SpeakerMayBeInUse,
    MayBeInUseLine,
    ReaderWouldPlay,
    ReaderWouldPlayLine,
    AboutMs,
    Seconds,
    SpeakerIs,
    BehindPictureEnd,
    WaysToSpeaker,
    Directly,
    LessDelay,
    Steadier,
    BehindPicture,
    BehindSteadier,
    ApplyingNewDelay,
    FixedWayDelay,
    FixedSpeakerDelay,
    LowerDelayLimit,
    UpperDelayLimit,
    ChangeStopsSound,
    SoundIsOnPc,
    NowAlsoPlaying,
    GroupedInSonos,
    NoSpeakerChosen,
    ChooseByClock,
    ChooseInList,
    UntilYouSwitch,
    QuitAndPlayOnPc,
    Quit,
    WindowStrip,
    AppStillRunning,
    IconByClock,
    QuitTheApp,

    // Settings: the panel's views and the dialogs.
    SetNavAbout,
    SetChimeTip,
    SetChimeBusy,
    SetExactDelay,
    SetDelayValue,
    SetDefaultDelay,
    SetDelayKeysLink,
    SetInactiveDelay,
    SetTone,
    SetToneLine,
    SetMoreTone,
    SetResetTone,
    SetVolumeLimit,
    SetVolumeLimitLine,
    SetTitlesLine,
    SetNameSaved,
    SetResetSettings,
    SetHiddenHelp,
    SetTechnicalDetails,
    SetResetQuestion,
    SetRemoveQuestion,
    SetRemovalToPc,
    SetKeysHelp,
    SetPhoneHelp,
    SetCallsHelp,
    SetSomeAppsHelp,
    SetQuitChosenApps,
    SetQuietSwitch,
    SetNotesSwitch,
    SetNotesHelp,
    SetMaHelp,
    SetShowLab,
    SetSwitch,
    SetNavLab,
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
    SetShow,
    SetHide,
    SetCopy,
    SetCopied,
    SetRemove,
    SetBackButton,
    SetTryAgain,
    SetConnect,
    SetCheckNow,
    SetEnglish,
    SetShowNamed,
    SetLookOn,
    SetAllNetworks,
    SetLookingPinned,
    SetNotLookingOff,
    SetLookingNowhere,
    SetNetworkNamed,
    SetNetworkPlain,
    SetSearchAgain,
    SetSendspinLine,
    SetReportProblem,
    SetBass,
    SetTreble,
    SetPasswordTitle,
    SetSongTitles,
    SetDetailModel,
    SetDetailAddress,
    SetDetailConnection,
    SetRemoveNamed,
    SetHideFromList,
    SetPlayCalls,
    SetStartAtSignIn,
    SetSignInFailed,
    SetShortcutsSwitch,
    SetShortcutRemove,
    SetShortcutAdd,
    SetChangeKeys,
    SetShortcutTaken,
    SetShortcutRejected,
    SetLanguage,
    SetLanguageWindows,
    SetMaOffForRun,
    SetMaOffer,
    SetMaOfferLine,
    SetMaFailed,
    SetMaAllowAgain,
    SetMaStep1,
    SetMaStep1Line,
    SetMaFound,
    SetMaFoundAt,
    SetMaNotConnected,
    SetMaStep2,
    SetMaStep2Line,
    SetMaPaired,
    SetMaUnpair,
    SetMaStep3,
    SetMaStep3Line,
    SetMaPlayed,
    SetMaPlayedOn,
    SetMaAllDone,
    SetOpenMa,
    SetMaTokenLine,
    SetCopyToken,
    SetMaAppearsAs,
    SetMaDelayLine,
    SetAnyDeviceLine,
    SetRefusedNamed,
    SetRefusedUnknown,
    SetPhone,
    SetCopyLink,
    SetNewLink,
    SetApi,
    SetApiHow,
    SetCopyKey,
    SetNewKey,
    SetWriteReport,
    SetShowWelcome,
    SetCheckLead,
    SetCheckOnPc,
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
    SetFindSearchOff,
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
    SetAppVersion,
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
    SetLabAp2,
    SetLabAp2Line,
    SetLabAutomatic,
    SetLabRealtime,
    SetLabMfi,
    SetLabMfiLine,
    SetLabAlac,
    SetLabAlacLine,
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
    SetAddress,
    SetAddressHint,
    SetCheckingAddress,
    SetNothingAt,
    SetFoundNamed,
    SetFoundLine,
    SetAddNamed,
    SetWayRecommended,
    SetWayPlain,
    SetConnectsBy,
    SetPort,
    SetDescriptionAddress,
    SetAddButton,
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
    SetWhatHappened,
    SetReportIncludes,
    SetReportDiagnostics,
    SetReportDiagnosticsLine,
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
    SetReportFolderClosed,
    SetReportSaveFailed,
    SetIssueOpened,
    SetProbeNoThread,
    SetReportVersionLine,
    SetPairingEnded,
    SetSavePassword,
    SetSendToThis,
    SetSpeakerGone,
    SetEnterCode,
    SetEnterPassword,
    SetFewerBands,
    SetSpokenDbPlus,
    SetSpokenDbMinus,
    SetSpokenDbZero,
    SetSpokenLevel,
    SetNameInApp,
    SetShowInList,
    SetOnlySomeApps,
    SetPhoneLinkLine,
    SetAnyoneListens,
    SetRefusedLine,
    SetAllow,
    SetAllowNamed,
    SetShortcutLine,
    SetPressKeysFor,
    SetPressOtherKeysFor,
    SetShiftKeptForLargeStep,
    SetKeysAlreadyFor,
    SetHoldForLarge,
    SetShowsUpWhen,
    SetLookingOnShort,
    SetNetworkNamedShort,
    SetAddByItsAddress,
    SetAddedNamed,
    SetByHandLink,
    SetFoundTwoWaysNamed,
    SetLookSendspin,
    SetExperimentalCap,
    SetHiddenFromList,
    SetCantPlayHeading,
    SetCantPlayFixShort,
    SetCheckStepByStep,
    SetCheckOnPcLineView,
    SetMaLeadExperimental,
    SetMaPausedBoth,
    SetMaStateOff,
    SetMaStateSettingUp,
    SetMaStateNotConnected,
    SetMaStateReady,
    SetShowToken,
    SetLicences,
    SetShowCounters,
    SetHideCounters,

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
        ThisPc => text("This PC"),
        MaName => text("Music Assistant"),
        CantHear => text("Can’t hear it?"),
        SpeakerMissing => text("Speaker missing?"),
        Settings => text("Settings"),
        Unmute => text("Unmute"),
        FirstPickAppsTitle => text("Where should the apps you choose play?"),
        FirstSwitchAppsTitle => text("Play the apps you choose on {speaker}?"),
        FirstAppsTitle => text("{speaker} is ready for the apps you choose"),
        ChosenAppsOn => text("The apps you choose play on {place}."),
        ChosenAppsNowOn => text("Your chosen apps now play on {place}."),
        ChosenAppsMoveSoon => text("The sound from your chosen apps moves to this PC in a few seconds."),
        ChosenAppsWhenStarting => text("Your chosen apps will play on this PC when they make sound."),
        ChosenAppsMoved => text("The sound from your chosen apps moved to this PC{when}."),
        ClickSpeakerApps => text("Click a speaker for the apps you choose."),
        FirstNoChimeApps => text("{speaker} can’t play a test chime first. Switch to it, then play something in an app you send to Stream To Speaker."),
        FirstHearApps => text("If you did, the apps you send to Stream To Speaker will play on {speaker}."),
        FirstPickTitle => text("Where should this PC’s sound play?"),
        FirstPickChime => text("Click a speaker to hear a short chime on it."),
        ChimeOnSpeaker => text("Play a chime on {speaker}"),
        FirstSwitchTitle => text("Play this PC’s sound on {speaker}?"),
        FirstNoChime => text("{speaker} can’t play a test chime first. Switch to it, then play something on this PC to check."),
        FirstHearTitle => text("Did you hear the chime on {speaker}?"),
        FirstChimeSent => text("This PC sent the chime to {speaker}."),
        FirstHearLine => text("If you did, this PC’s sound will play on {speaker}."),
        LookingForMore => text("Looking for more speakers…"),
        FirstVolume => text("Volume on {speaker}"),
        PlayAgain => text("Play it again"),
        UseWithoutChecking => text("Use {speaker} without checking"),
        ChooseAnotherSpeaker => text("Choose another speaker"),
        FirstPlayingTitle => text("{speaker} now plays this PC’s sound"),
        FirstStays => text("It stays on {speaker} until you switch it."),
        FirstIcon => text("The app’s icon is by the clock, or under the ^ arrow there. A click on it opens the same controls as this window."),
        HeardIt => text("I heard it"),
        DidntHear => text("I didn’t hear it"),
        Done => text("Done"),

        PlayingTitle => text("Playing on {speaker}"),
        ReadyLine => text("Nothing on this PC is playing right now."),
        NotRespondingTitle => text("{speaker} isn’t responding"),
        RefusedTitle => text("Windows didn’t switch to {speaker}"),

        Close => text("Close"),

        Cancel => text("Cancel"),
        TrySpeakerAgain => text("Try {speaker} again"),
        ChooseApps => text("Choose apps…"),
        MoreSpeakersEllipsis => text("More speakers…"),
        OpenShort => text("Open"),
        Reset => text("Reset"),
        KindSonos => text("Sonos"),
        KindSonosGroup => text("Sonos group"),
        KindHomePod => text("HomePod"),
        KindHomePodPair => text("HomePod pair"),
        KindAppleTv => text("Apple TV"),
        KindAirPlay => text("AirPlay"),
        KindSendspin => text("Sendspin speaker"),
        KindSpeaker => text("Speaker"),
        RowNotFoundShort => text("Not found"),
        RowWithAddress => text("{name} ({address})"),
        RowRepairFirst => text("Repair first"),
        RowMoreSpeakers => text("More speakers ({count})"),
        Searching => text("Looking for speakers…"),
        BusyTitle => text("{speaker} is playing something else right now."),
        BusyOtherPcTitle => text("{speaker} is playing {pc}’s sound right now."),
        DelayLabel => text("Delay"),
        FigureAbout => text("about {seconds} s"),
        ChangeRemoved => text("{ms} ms removed"),
        ChangeAdded => text("{ms} ms added"),
        RemoveMs => text("Remove {ms} ms"),
        AddMs => text("Add {ms} ms"),
        RemoveBare => text("Remove"),
        AddBare => text("Add"),
        MsUnit => text("ms"),
        Removing => text("Removing… {ms} ms to go"),
        Adding => text("Adding… {ms} ms to go"),
        SoundBehind => text("Sound behind the picture?"),
        SoundBreakingUp => text("Sound breaking up?"),
        PlaceStatus => text("{place}: {state}"),
        CliPartVolume => text("volume {level}"),
        CliPartMuted => text("muted"),
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
        CliReconnectingSuffix => text("Applying it: the sound stops for a few seconds."),
        RefuseNoDelay => text("{status} — no speaker delay to change."),
        RefuseMaDelay => {
            text("{status} — Music Assistant sets the delay for each of its speakers.")
        }
        RefuseSteady => {
            text("{speaker}: its delay can’t be changed this way. Choose “{way}” on {speaker}’s page.")
        }
        RefuseUnknownPlace => text("No speaker called “{name}” is in the list."),
        RefuseOnlyFromWindow => text("To play to “{speaker}”, pick it in the Stream To Speaker window."),
        DeviceForPlace => text("{place} (Stream To Speaker)"),
        DeviceNotRunning => text("Stream To Speaker (not running)"),

        NotifyStoppedTitle => text("{speaker} stopped responding"),
        NotifyTakenTitle => text("{speaker} is playing something else"),
        NoticeUpdate => text("Version {version} is ready to install."),
        ThisPcInSentence => text("this PC"),
        VolumeNumber => text("{level}"),
        PlaceVolume => text("{place} volume"),
        MutePlace => text("Mute {place}"),
        UnmutePlace => text("Unmute {place}"),
        RetryNoNetwork => text("No network connection. Trying again…"),
        WaitForNetwork => text("Waiting for this PC’s network…"),
        RetryNotResponding => text("Not responding. Trying again…"),
        Connecting => text("Connecting…"),
        PausedOnSpeaker => text("Paused on the speaker"),
        NotPlayingYet => text("Not playing yet"),
        NotResponding => text("Not responding"),
        NoNetworkConnection => text("No network connection"),
        PlayingInJoinedRoom => text("Playing · now also in {room}"),
        PlayingExcept => text("Playing, except {app}"),
        AppPlaysOnPc => text("{app} plays on this PC"),
        Why => text("Why?"),
        PlayingChosenApps => text("Playing · only chosen apps"),
        OnlyChosenApps => text("Only chosen apps play here"),
        Playing => text("Playing"),
        NothingToPlayOn => text("This PC has nothing to play on right now."),
        NothingPlaying => text("Nothing playing"),
        NoOutputConnected => text("No output connected"),
        SwitchToPc => text("Switch to this PC"),
        PcOutputs => text("This PC’s outputs"),
        SwitchToSpeaker => text("Switch to {speaker}"),
        PlayPcSoundOnSpeaker => text("Play this PC’s sound on {speaker}"),
        SwitchByWay => text("Switch to {speaker} by {way}"),
        SpeakerGear => text("{speaker}’s settings: tone, delay, name"),
        MissingWhySpoken => text("Why? Opens “Speaker missing?” for {speaker}"),
        RowPlayingElse => text("Playing something else"),
        MayBeInUse => text("May be in use"),
        SpokenConnecting => text("{name}, connecting…"),
        SpokenNotResponding => text("{name}, not responding"),
        SpokenNotFound => text("{name}, not found"),
        SpokenPlayingElse => text("{name}, playing something else"),
        SpokenMayBeInUse => text("{name}, may be in use"),
        SpokenRepairFirst => text("{name}, repair first"),
        FoundOnWifi => text("Speakers found on this Wi-Fi ({count})"),
        FoundOnNetwork => text("Speakers found on this network ({count})"),
        NoSpeakersOnNetwork => text("No speakers found on the network."),
        ClickSpeaker => text("Click a speaker to play this PC’s sound on it."),
        ConnectingToSpeaker => text("Connecting to {speaker}…"),
        ConnectingFromPlace => text("Connecting to {speaker}. The sound stays on {place} and moves when {speaker} is ready."),
        CancelConnecting => text("Cancel connecting to {speaker}"),
        EnterTheCode => text("Enter the code…"),
        EnterThePassword => text("Enter the password…"),
        SwitchToPcNow => text("Switch to this PC now"),
        MoveInSeconds => text("The sound moves to this PC in a few seconds."),
        MoveWhenSoundStarts => text("Sound that starts now plays on this PC."),
        LostWifi => text("This PC lost its Wi-Fi connection"),
        LostNetwork => text("This PC lost its network connection"),
        PcNotConnected => text("This PC isn’t connected to a network"),
        SpeakerNotFound => text("{speaker} can’t be found"),
        PlayingOtherPc => text("{speaker} is playing {pc}’s sound"),
        GroupedWith => text("{speaker} was grouped with {room}"),
        StoppedPlayingPc => text("{speaker} stopped playing this PC’s sound"),
        AvailableAgain => text("{speaker} is available again"),
        StaysUntilSwitch => text("The sound stays on this PC until you switch."),
        AtTime => text(" at {time}"),
        MovedToPc => text("The app moved the sound to this PC{when}."),
        MovedAfterVpn => text("The app moved the sound to this PC{when}, just after this PC connected to “{vpn}”."),
        MovedAfterConnection => text("The app moved the sound to this PC{when}, just after this PC made a new network connection."),
        DidntRespond => text("{speaker} didn’t respond"),
        AsksForCode => text("{speaker} asks for its code again"),
        NeedsCode => text("{speaker} needs its code"),
        NeedsPassword => text("{speaker} needs its password"),
        AsksForPassword => text("{speaker} asks for its password again"),
        SwitchingBackStops => text("Switching back stops what is playing there."),
        DeviceTookOver => text("Another device may have taken it over. Switching back stops what it plays."),
        MovedAfterGroup => text("The app moved the sound to this PC{when}. {speaker} can play it again once it leaves that group."),
        YouRemoved => text("You removed {speaker}"),
        SoundStayedHere => text("The sound stayed on this PC"),
        PlayingAtStartup => text("It was playing here when the app started."),
        OutputDisconnected => text("{name} disconnected"),
        MovedToPcSpeakers => text("The sound moved to this PC’s speakers{when}."),
        MovedToOutput => text("The sound moved to {output}{when}."),
        OutputConnected => text("{name} connected"),
        FromSpeakerHeadphones => text("The sound moved here from {speaker}. When they disconnect, it plays on this PC’s speakers."),
        FromSpeakerOutput => text("The sound moved here from {speaker}. When it disconnects, it plays on this PC’s speakers."),
        QuietForThirty => text("Nothing played for 30 minutes"),
        MovedFromSpeaker => text("The app moved the sound from {speaker} to this PC{when}."),
        SoundStillOn => text("The sound is still on {place}."),
        CodeDidntWork => text("That code didn’t work"),
        PasswordDidntWork => text("That password didn’t work"),
        CouldntStartPlaying => text("{speaker} couldn’t start playing"),
        RefusedConnection => text("{speaker} refused the connection"),
        AppOutputMissing => text("The app’s sound output is missing"),
        RestoreInSettings => text("Turn Stream To Speaker back on in Windows’ sound settings, or install the app again."),
        RestoreOutputLine => text("The sound stays on this PC. Repair puts it back; it needs administrator approval."),
        RepairOutput => text("Repair"),
        OutputTurnedOff => text("Stream To Speaker is turned off in Windows"),
        TurnOnOutputLine => text("Repair turns it back on."),
        TurnOnFailedLine => text("Windows didn’t turn it back on. Turn it on in Windows’ sound settings."),
        RepairingOutput => text("Repairing the app’s sound output…"),
        StaysMeanwhile => text("The sound stays on this PC meanwhile."),
        RepairDidntWork => text("Repair didn’t work: the app’s sound output is still missing"),
        RestartForOutput => text("Restarting Windows may help; if not, report the problem."),
        RepairNeedsApproval => text("Repair needs administrator approval, and it wasn’t given"),
        ApprovalNotGiven => text("The sound stays on this PC."),
        OutputIsBack => text("The app’s sound output is back"),
        CanSwitchAgain => text("You can switch to a speaker again."),
        WindowsDidntSwitchLine => text("Try again, or choose this app’s output in Windows’ sound settings."),
        OpenWindowsSound => text("Open Windows’ sound settings"),
        MaNotStarted => text("Music Assistant hasn’t started playing"),
        MaPlaceLine => text("Each speaker’s volume and delay are set in Music Assistant. The slider at the top sets the level this PC sends to it."),
        MaStartPcSound => text("Start this PC’s sound in Music Assistant, or set Music Assistant to start it by itself."),
        HowToStart => text("How to start it"),
        SpeakerWasPaused => text("{speaker} was paused"),
        PausedBySpeaker => text("It was paused with its own buttons or app, so this PC’s sound isn’t heard."),
        MicInUse => text("A microphone is in use"),
        HearCallLate => text("If this is a call, you will hear the other person late on {speaker}."),
        ChimeStopsIt => text("A test chime stops it, and it may not start again by itself."),
        PlayTestChime => text("Play a test chime"),
        SwitchStopsSound => text("Switching to {speaker} stops what it is playing."),
        SpeakerMayBeInUse => text("{speaker} may be in use"),
        MayBeInUseLine => text("The app can’t tell whether another device is playing on {speaker}. Switching to {speaker} stops anything playing on it."),
        ReaderWouldPlay => text("Your screen reader’s voice would play on {speaker} too"),
        ReaderWouldPlayLine => text("It would be late there. To keep it on this PC, choose this PC’s output in the screen reader’s own settings."),
        AboutMs => text("about {ms} ms"),
        Seconds => text("{seconds} s"),
        SpeakerIs => text("{speaker} is "),
        BehindPictureEnd => text(" behind the picture."),
        WaysToSpeaker => text("How this PC sends sound to {speaker}"),
        Directly => text("Directly"),
        LessDelay => text("Less delay"),
        Steadier => text("Steadier"),
        BehindPicture => text(" behind the picture"),
        BehindSteadier => text(" behind, but breaks up less"),
        ApplyingNewDelay => text("Applying the new delay…"),
        FixedWayDelay => text("To change the delay, choose {way} above."),
        FixedSpeakerDelay => text("{speaker}’s delay can’t be changed."),
        LowerDelayLimit => text("{speaker} can’t play with less delay than this."),
        UpperDelayLimit => text("{speaker} can’t play with more delay than this."),
        ChangeStopsSound => text("Changing the delay stops {speaker}’s sound for a few seconds."),
        SoundIsOnPc => text("The sound is on this PC."),
        NowAlsoPlaying => text("Now also playing in {room}"),
        GroupedInSonos => text("{room} was grouped with {place} in the Sonos app."),
        NoSpeakerChosen => text("No speaker chosen"),
        ChooseInList => text("The sound is still on {place}. Choose a speaker in the list."),
        ChooseByClock => text("The sound is still on {place}. Click the app’s icon by the clock to choose a speaker."),
        UntilYouSwitch => text("{sentence} Until you switch, the sound stays on {place}."),
        QuitAndPlayOnPc => text("Quit, and play the sound on this PC"),
        Quit => text("Quit"),
        WindowStrip => text("Closing this window doesn’t stop the sound. This icon by the clock opens the same controls."),
        AppStillRunning => text("The app is still running"),
        IconByClock => text("Its icon is by the clock, or under the ^ arrow there."),
        QuitTheApp => text("Quit the app"),

        // Settings: the panel's views and the dialogs.
        SetNavAbout => text("About"),
        SetChimeTip => text("A short chime on {speaker} and nothing else. This PC’s sound stays where it is."),
        SetChimeBusy => text("{speaker} is playing something else. A test chime stops it, and it may not start again by itself."),
        SetDelayValue => text("Set"),
        SetDefaultDelay => text("Use the default: {ms} ms"),
        SetExactDelay => text("Type an exact delay…"),
        SetDelayKeysLink => text("Keyboard shortcuts for the delay…"),
        SetInactiveDelay => text("{speaker} isn’t playing now: you’ll hear a change the next time it plays."),
        SetTone => text("Tone"),
        SetToneLine => text("Changes only the sound this PC sends to {speaker}. {speaker}’s own settings stay as they are."),
        SetMoreTone => text("More tone controls…"),
        SetResetTone => text("Reset tone"),
        SetVolumeLimit => text("Volume limit"),
        SetVolumeLimitLine => text("The loudest this PC can make {speaker}. {speaker}’s own buttons and app aren’t limited."),
        SetTitlesLine => text("Only from music apps such as Spotify or Apple Music. People who look at {speaker}’s display or its app can see them. Nothing from browsers or video players."),
        SetNameSaved => text("Saved"),
        SetResetSettings => text("Reset these settings…"),
        SetHiddenHelp => text("Hidden speakers can be shown again from “Speaker missing?”."),
        SetTechnicalDetails => text("Technical details"),
        SetResetQuestion => text("Erases {speaker}’s pairing, password, volume limit, delay, tone and the choices on this page. {speaker} stays in the list while it is on your network; to take it off the list, hide it."),
        SetRemoveQuestion => text("{speaker} and its settings go from this PC. If it is found again, it is a new speaker."),
        SetRemovalToPc => text("The sound moves to this PC."),
        SetKeysHelp => text("They work while any app is in front."),
        SetPhoneHelp => text("Your phone’s browser lists the speakers, switches between them and changes the delay."),
        SetCallsHelp => text("For call apps that use Windows’ call device. Calls in a browser go where the rest of the sound goes."),
        SetSomeAppsHelp => text("In Windows, set the output of each app you choose to the one with “Stream To Speaker” in its name."),
        SetQuitChosenApps => text("Your chosen apps are silent while Stream To Speaker isn’t running."),
        SetQuietSwitch => text("Switch back to this PC after 30 minutes with nothing playing"),
        SetNotesSwitch => text("Show notes by the clock about where the sound is playing"),
        SetNotesHelp => text("Notes about a speaker that can’t play show even while this is off."),
        SetMaHelp => text("For people who use Music Assistant."),
        SetShowLab => text("Show Lab"),
        SetSwitch => text("Switch"),
        SetNavLab => text("Lab"),
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
        SetShow => text("Show"),
        SetHide => text("Hide"),
        SetCopy => text("Copy"),
        SetCopied => text("Copied"),
        SetRemove => text("Remove"),
        SetBackButton => text("Back"),
        SetTryAgain => text("Ask for a new code"),
        SetConnect => text("Connect"),
        SetCheckNow => text("Check now"),
        SetEnglish => text("English"),
        SetShowNamed => text("Show {speaker}"),
        SetLookOn => text("Look for speakers on"),
        SetAllNetworks => text("All networks"),
        SetLookingPinned => text("Looking only from {address}, as the command line says."),
        SetNotLookingOff => text("Not looking: {adapter} is turned off. Speakers you added by address still work."),
        SetLookingNowhere => text("Not looking: this PC isn’t connected to a network."),
        SetNetworkNamed => text("{adapter} “{network}” ({address})"),
        SetNetworkPlain => text("{adapter} ({address})"),
        SetSearchAgain => text("Search again"),
        SetSendspinLine => text("ESPHome and other speakers that play from Music Assistant. This PC plays to them directly."),
        SetReportProblem => text("Report a problem"),
        SetBass => text("Bass"),
        SetTreble => text("Treble"),
        SetPasswordTitle => text("AirPlay password"),
        SetSongTitles => text("Show song titles on {speaker}"),
        SetDetailModel => text("Model: {value}"),
        SetDetailAddress => text("Address: {value}"),
        SetDetailConnection => text("Connection: {value}"),
        SetRemoveNamed => text("Remove {speaker}…"),
        SetHideFromList => text("Hide from the list"),
        SetPlayCalls => text("Play calls on the speaker too"),
        SetStartAtSignIn => text("Start when I sign in to Windows"),
        SetSignInFailed => text("Couldn’t change the start-at-sign-in setting: {why}"),
        SetShortcutsSwitch => text("Keyboard shortcuts for the delay"),
        SetShortcutRemove => text("Remove {ms} ms"),
        SetShortcutAdd => text("Add {ms} ms"),
        SetChangeKeys => text("Change…"),
        SetShortcutRejected => text("Windows didn’t accept this shortcut."),
        SetShortcutTaken => text("This shortcut is already in use. Choose another."),
        SetLanguage => text("Language"),
        SetLanguageWindows => text("Windows default ({language})"),
        SetMaOffForRun => text("Music Assistant is turned off for this run of Stream To Speaker (it was started with --no-sendspin)."),
        SetMaOffer => text("Let Music Assistant play this PC’s sound"),
        SetMaOfferLine => text("Makes this PC’s sound available in Music Assistant as a live input."),
        SetMaFailed => text("Couldn’t start offering this PC to Music Assistant: {why}"),
        SetMaAllowAgain => text("Allow pairing again"),
        SetMaStep1 => text("Music Assistant finds this PC"),
        SetMaStep1Line => text("In Music Assistant, add the Sendspin source provider. This PC appears there as “{pc}”."),
        SetMaFound => text("Found by {server}. This PC appears there as “{pc}”."),
        SetMaFoundAt => text("Found by {server} at {address}. This PC appears there as “{pc}”."),
        SetMaNotConnected => text("Music Assistant isn’t connected to this PC right now. It connects by itself when it is running on the same network as this PC."),
        SetMaStep2 => text("Pair"),
        SetMaStep2Line => text("In Music Assistant, open Settings → Players → “{pc}” and start its setup. A 6-digit code then appears here; type it there."),
        SetMaPaired => text("Paired with {server}."),
        SetMaUnpair => text("Unpair"),
        SetMaStep3 => text("Choose where it plays"),
        SetMaStep3Line => text("In Music Assistant, set “Automatically play line-in on” for “{pc}”, or start it from Browse."),
        SetMaPlayed => text("Music Assistant has played this PC’s sound."),
        SetMaPlayedOn => text("Music Assistant played this PC’s sound on {date}."),
        SetMaAllDone => text("Music Assistant is in your speaker list. Pick it there to send this PC’s sound; it gets nothing otherwise."),
        SetOpenMa => text("Open Music Assistant"),
        SetMaTokenLine => text("Music Assistant doesn’t need this token. Another Sendspin server may ask for it to pair with this PC."),
        SetCopyToken => text("Copy token"),
        SetMaAppearsAs => text("This PC appears as"),
        SetMaDelayLine => text("Music Assistant sets the delay for each of its speakers, so it isn’t changed here."),
        SetAnyDeviceLine => text("Anyone on this network who knows the address can listen to everything this PC plays."),
        SetRefusedNamed => text("{name} ({address}) · {time}"),
        SetRefusedUnknown => text("{address} (unknown device) · {time}"),
        SetPhone => text("Control from your phone"),
        SetCopyLink => text("Copy link"),
        SetNewLink => text("Make a new link"),
        SetApi => text("Let scripts and home automation control this app"),
        SetApiHow => text("How to use the API"),
        SetCopyKey => text("Copy key"),
        SetNewKey => text("Make a new key"),
        SetWriteReport => text("Write a report"),
        SetShowWelcome => text("Show the welcome again"),
        SetCheckLead => text("Let’s find where the sound stops."),
        SetCheckOnPc => text("Your sound is on this PC."),
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
        SetCheckRefused => text("{device}, in {speaker}’s group, asked for the sound and was refused."),
        SetAllowIt => text("Allow it"),
        SetCheckCantTell => text("This PC can’t tell whether {speaker} is playing."),
        SetCheckCantTellLine => text("HomePods and other AirPlay speakers accept the stream and report faults, but not whether they play."),
        SetTrySteady => text("Use “Steadier”"),
        SetCheckVolume => text("{speaker}’s volume."),
        SetCheckVolumeLevel => text("{level}, not muted."),
        SetCheckMuted => text("{speaker} is muted."),
        SetCheckZero => text("{speaker}’s volume is 0."),
        SetSetTo => text("Set to {level}"),
        SetCheckAllGood => text("Everything this PC can check looks right."),
        SetCheckCantHearRoom => text("This PC can’t hear the room. If {speaker} is still silent, the speaker itself may be the problem."),
        SetCheckUntested => text("Playing to HomePod pairs hasn’t been tested on real HomePods yet; a report helps."),
        SetCheckAppleTv => text("Apple TVs that play through HomePods may not pass this PC’s sound on. Pick the HomePods instead."),
        SetCheckLive => text("The check runs live and ticks again as things change. It stops at the first broken step. When this PC can’t check a step, it says so and goes on."),
        SetFindLooking => text("Looking on {network}."),
        SetFindSearchOff => text("Not looking for speakers: speaker search was turned off when the app was started."),
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
        SetAppVersion => text("Stream To Speaker {version}"),
        SetDevBuild => text("Development build — update checks are off."),
        SetNotChecked => text("Not checked yet."),
        SetChecking => text("Checking for updates…"),
        SetUpToDate => text("You have the latest version."),
        SetCheckedAt => text("Checked {when}."),
        SetUpdateHow => text("Update now downloads the installer, has Windows check its signature and that it comes from this project, then installs it. Your sound plays on this PC for a moment."),
        SetUpdateHowPage => text("Its page has the installer."),
        SetUpdateFailed => text("Couldn’t check for updates."),
        SetDownloading => text("Downloading the update and checking its signature…"),
        SetClosesForSetup => text("Stream To Speaker closes while Setup runs, and your sound plays on this PC meanwhile. When Setup is done, it plays on {speaker} again."),
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
        SetLabAp2 => text("AirPlay 2 stream"),
        SetLabAp2Line => text("Normally follows each speaker’s delay choice. Some speakers accept the realtime handshake but never play it."),
        SetLabAutomatic => text("Automatic"),
        SetLabRealtime => text("Realtime"),
        SetLabMfi => text("Try MFi encryption (et=4) first"),
        SetLabMfiLine => text("A refused attempt can make the speaker unresponsive for up to half a minute."),
        SetLabAlac => text("Send uncompressed ALAC"),
        SetLabAlacLine => text("For testing receivers that misbehave with compressed audio."),
        SetPairWith => text("Pair with {speaker}"),
        SetOnYourTv => text("On your Apple TV"),
        SetOnThisPc => text("On this PC"),
        SetLookAtTv => text("Look at {speaker}: it’s showing a 4-digit code now. A wrong try makes it show a new one."),
        SetAskingForCode => text("Asking {speaker} to show a code…"),
        SetCheckingCode => text("Checking the code…"),
        SetNoAnswerPairing => text("{speaker} didn’t show a code."),
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
        SetAddress => text("Address"),
        SetAddressHint => text("192.168.1.20 or kitchen.local"),
        SetCheckingAddress => text("Checking {address}…"),
        SetNothingAt => text("Nothing at {address} answered as a speaker. Check the address and that the speaker is on."),
        SetFoundNamed => text("Found {speaker}"),
        SetFoundLine => text("{kind} at {address} · {delay} delay"),
        SetAddNamed => text("Add {speaker}"),
        SetWayRecommended => text("{way} (recommended)"),
        SetWayPlain => text("{way}"),
        SetConnectsBy => text("Type"),
        SetPort => text("Port"),
        SetDescriptionAddress => text("Description address"),
        SetAddButton => text("Add"),
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
        SetWhatHappened => text("What happened?"),
        SetReportIncludes => text("The report includes"),
        SetReportDiagnostics => text("Playback diagnostics"),
        SetReportDiagnosticsLine => text("Current counters and the last recorded fault"),
        SetReportSpeakers => text("Speaker names, kinds and addresses"),
        SetReportVersions => text("App version and operating system"),
        SetReportLog => text("The log file"),
        SetReportLogLine => text("It names speakers and their addresses, so it goes only with them."),
        SetPreview => text("Preview"),
        SetNothingSent => text("Nothing is sent until you choose where. Open a GitHub issue copies the report and opens a new issue in your browser, for you to paste it and read it first."),
        SetCopyReport => text("Copy report"),
        SetSaveReport => text("Save as a file"),
        SetOpenIssue => text("Open a GitHub issue"),
        SetReportCopied => text("Copied. Paste it into an e-mail or a message."),
        SetReportFolderClosed => text("Saved in your log folder. The folder couldn’t be opened."),
        SetReportSaveFailed => text("Couldn’t save the report in your log folder."),
        SetReportSaved => text("Saved in your log folder, which is open now."),
        SetIssueOpened => text("The report is copied and a new issue is open in your browser: paste it there. Nothing is sent until you submit it."),
        SetProbeNoThread => text("Couldn’t start the search. Try again."),
        SetReportVersionLine => text("Stream To Speaker {version} · {os}"),
        SetPairingEnded => text("That pairing had already ended. Pick the speaker again to pair it."),
        SetSavePassword => text("Save"),
        SetSendToThis => text("Send it there"),
        SetSpeakerGone => text("This speaker isn’t in the list any more."),
        SetEnterCode => text("Enter code…"),
        SetEnterPassword => text("Enter password…"),
        SetFewerBands => text("Fewer bands"),
        SetSpokenDbPlus => text("{what}, plus {n} decibels"),
        SetSpokenDbMinus => text("{what}, minus {n} decibels"),
        SetSpokenDbZero => text("{what}, 0 decibels"),
        SetSpokenLevel => text("{what} {level}"),
        SetNameInApp => text("Name in this app"),
        SetShowInList => text("Show in the list"),
        SetOnlySomeApps => text("Only send some apps to the speaker"),
        SetPhoneLinkLine => text("Anyone with this link can control this app on this network. A new link stops the old one working."),
        SetAnyoneListens => text("Let any device on this network listen"),
        SetRefusedLine => text("Refused: {who}"),
        SetAllow => text("Allow"),
        SetAllowNamed => text("Allow {who}"),
        SetShortcutLine => text("{what}: {keys}"),
        SetShiftKeptForLargeStep => text("Shift is kept for the 100 ms step. Press other keys for {what}… (Esc stops)"),
        SetPressOtherKeysFor => text("Windows didn’t accept {keys}. Press other keys for {what}… (Esc stops)"),
        SetKeysAlreadyFor => text("{keys} is already the shortcut for {other}. Press other keys for {what}… (Esc stops)"),
        SetPressKeysFor => text("Press the new keys for {what}… (Esc stops)"),
        SetHoldForLarge => text("Hold {key} with a shortcut to change the delay by {ms} ms."),
        SetShowsUpWhen => text("A speaker shows up when it is switched on and on the same network as this PC."),
        SetLookingOnShort => text("Looking on {networks}"),
        SetNetworkNamedShort => text("{adapter} “{network}”"),
        SetAddByItsAddress => text("Add a speaker by its address"),
        SetAddedNamed => text("Added {speaker}."),
        SetByHandLink => text("Enter the details yourself…"),
        SetFoundTwoWaysNamed => text("{speaker} answered two ways. Which should this app use?"),
        SetLookSendspin => text("Look for Sendspin speakers"),
        SetExperimentalCap => text("Experimental."),
        SetHiddenFromList => text("Hidden from the list"),
        SetCantPlayHeading => text("Found, but this app can’t play to it"),
        SetCantPlayFixShort => text("Turn on AirPlay in the Apple TV’s settings."),
        SetCheckStepByStep => text("Check step by step"),
        SetCheckOnPcLineView => text("Pick a speaker in the panel; this check then follows the sound there."),
        SetMaLeadExperimental => text("Send this PC’s sound to Music Assistant, which can play it on any speaker it controls. Experimental."),
        SetMaPausedBoth => text("Pairing with a code is paused because wrong codes were entered too many times. If that wasn’t you, something on your network may be trying to pair."),
        SetMaStateOff => text("not offered"),
        SetMaStateSettingUp => text("being set up"),
        SetMaStateNotConnected => text("not connected"),
        SetMaStateReady => text("ready"),
        SetShowToken => text("Show the token"),
        SetLicences => text("Licences and source"),
        SetShowCounters => text("Show live counters"),
        SetHideCounters => text("Hide live counters"),

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
        DeviceNotRunning => text("Stream To Speaker (stopped)"),
        ChangeRemoved => text("removed: {ms}"),
        FigureAbout => text("≈{seconds} s"),
        SetLabFrames => four("count", "{count} (one)", "{count} (few)", "{count} (many)", "{count} (other)"),
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
        assert_eq!(t!(en, SetLabFrames, count = 1), "1 frame");
        assert_eq!(t!(en, SetLabFrames, count = 3), "3 frames");
        assert_eq!(t!(Lang::Test, SetLabFrames, count = 5), "5 (many)");
    }

    #[test]
    fn a_sentence_gets_one_full_stop() {
        let en = Lang::English;
        assert_eq!(en.sentence("Playing on Kitchen"), "Playing on Kitchen.");
        assert_eq!(en.sentence("Connecting to Kitchen…"), "Connecting to Kitchen…");
        assert_eq!(en.sentence("Done."), "Done.");
    }
}
