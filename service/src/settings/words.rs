//! The window's words: every sentence comes from the string table
//! ([`crate::model::text`]), through [`tr`].

use crate::model::text::{Arg, Key, Lang, LocalTime};

use super::port::{Date, Seen, SpeakerKind};

/// `w!(lang, Key, name = value, …)`: the sentence `Key` with its
/// placeholders filled.
macro_rules! w {
    ($lang:expr, $key:ident $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::settings::words::tr(
            $lang,
            $crate::model::text::Key::$key,
            &[$((stringify!($name), $crate::model::text::Arg::from($value))),*],
        )
    };
}
pub(crate) use w;

#[cfg(test)]
thread_local! {
    /// Tests of layout make every sentence a third longer, as translations
    /// often are.
    pub(crate) static STRETCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The sentence `key` in `lang`.
pub fn tr(lang: Lang, key: Key, args: &[(&str, Arg<'_>)]) -> String {
    let s = lang.get(key, args);
    #[cfg(test)]
    if STRETCH.with(|s| s.get()) {
        return longer(&s);
    }
    s
}

/// `s` a third longer: its first word stretched, as translations stretch.
#[cfg(test)]
pub(crate) fn longer(s: &str) -> String {
    let extra = s.chars().count().div_ceil(3);
    match s.split_once(' ') {
        Some((first, rest)) => format!("{first}{} {rest}", "x".repeat(extra)),
        None => format!("{s}{}", "x".repeat(extra)),
    }
}

/// Whether `k` is one of the sentences of the views and dialogs (the last
/// block of the table, which starts with *About*).
#[cfg(test)]
pub(crate) fn is_settings_key(k: Key) -> bool {
    k >= Key::SetNavAbout || matches!(k, Key::Unmute | Key::ReadyLine | Key::NoticeUpdate | Key::DelayLabel | Key::MsUnit | Key::Reset)
}

/// The kind of a speaker as its row says it: *Sonos*, *AirPlay*.
pub fn kind_short(lang: Lang, k: &SpeakerKind) -> String {
    match k {
        SpeakerKind::Sonos => w!(lang, KindSonos),
        SpeakerKind::SonosGroup => w!(lang, KindSonosGroup),
        SpeakerKind::HomePod => w!(lang, KindHomePod),
        SpeakerKind::HomePodPair => w!(lang, KindHomePodPair),
        SpeakerKind::AppleTv => w!(lang, KindAppleTv),
        SpeakerKind::AirPlay => w!(lang, KindAirPlay),
        SpeakerKind::Sendspin => w!(lang, KindSendspin),
        SpeakerKind::Other { maker: Some(m) } => m.clone(),
        SpeakerKind::Other { maker: None } => w!(lang, KindSpeaker),
    }
}

/// A date as Settings says it: *12 October*.
pub fn date(lang: Lang, d: Date) -> String {
    let month = match d.month {
        1 => w!(lang, SetJanuary),
        2 => w!(lang, SetFebruary),
        3 => w!(lang, SetMarch),
        4 => w!(lang, SetApril),
        5 => w!(lang, SetMay),
        6 => w!(lang, SetJune),
        7 => w!(lang, SetJuly),
        8 => w!(lang, SetAugust),
        9 => w!(lang, SetSeptember),
        10 => w!(lang, SetOctober),
        11 => w!(lang, SetNovember),
        _ => w!(lang, SetDecember),
    };
    w!(lang, SetDate, day = d.day, month = &month)
}

/// *today 21:10*, *yesterday 22:10*, *3 October*.
pub fn seen(lang: Lang, s: Seen) -> String {
    match s {
        Seen::Today(t) => w!(lang, SetToday, time = &lang.time(t)),
        Seen::Yesterday(t) => w!(lang, SetYesterday, time = &lang.time(t)),
        Seen::On(d) => date(lang, d),
    }
}

/// A time of day: *21:12*.
pub fn time(lang: Lang, t: LocalTime) -> String {
    lang.time(t)
}

/// Seconds left as *2:51*.
pub fn left(lang: Lang, seconds: u32) -> String {
    let clock = format!("{}:{:02}", seconds / 60, seconds % 60);
    w!(lang, SetLeft, time = &clock)
}

/// A delay as the delay choice and *Connects with* say it: *about 0.3 s*.
pub fn about(lang: Lang, ms: u32) -> String {
    crate::model::panel::delay_figure(lang, crate::model::facts::SpeakerDelay { own_ms: ms, measured: false, change_ms: 0 })
}
