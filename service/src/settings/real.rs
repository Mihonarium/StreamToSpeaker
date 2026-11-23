//! The Settings port on the running app: [`AppSettings`].
//!
//! Every view is read from [`App`] and its settings file when a page asks;
//! every act changes the setting at once and saves it. What the app can't
//! do yet is `None` in the views, so the page leaves that control out.
//! Work that touches the network (connecting, probing an address) runs on
//! a thread of its own; nothing here waits for a speaker.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::{App, UpdateOutcome};
use crate::delay::{DelayStatus, Kind, Request};
use crate::equalizer::EQ_PRESETS;
use crate::manual_speakers::{self, ManualKind};
use crate::model::text::{Lang, LocalTime};
use crate::sendspin_app::{is_sendspin_id, PairPromptKind, MA_OUTPUT_ID};
use crate::user_config::AutoConnect;

use super::port::*;
use super::words::w;

/// What the app asks of the desktop around it: opening a page in the
/// browser or a folder, and the time zone. Behind a trait so tests can see
/// what was opened.
pub trait Desktop: Send + Sync {
    fn open_url(&self, url: &str);
    fn open_folder(&self, dir: &std::path::Path);
    /// The local time's offset from UTC now, in seconds.
    fn utc_offset_secs(&self) -> i64;
}

/// The desktop of the PC the app runs on.
pub struct SystemDesktop;

impl Desktop for SystemDesktop {
    fn open_url(&self, url: &str) {
        #[cfg(windows)]
        if let Err(e) = crate::open_url(url) {
            log::warn!("settings: couldn't open the browser: {e}");
        }
        #[cfg(not(windows))]
        log::info!("settings: would open {url}");
    }

    fn open_folder(&self, dir: &std::path::Path) {
        // explorer.exe opens a folder given as its argument; no shell is
        // involved.
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
        #[cfg(not(windows))]
        log::info!("settings: would open {}", dir.display());
    }

    fn utc_offset_secs(&self) -> i64 {
        local_offset()
    }
}

/// The local time's offset from UTC: Windows' local and UTC clocks read
/// together.
#[cfg(windows)]
fn local_offset() -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: both point at SYSTEMTIME values this function owns; the calls
    // only write them.
    unsafe {
        GetLocalTime(&mut local);
        GetSystemTime(&mut utc);
    }
    let secs = |t: &SYSTEMTIME| days_from_civil(t.wYear as i64, t.wMonth as i64, t.wDay as i64) * 86_400 + t.wHour as i64 * 3600 + t.wMinute as i64 * 60;
    // Round to the quarter hour (the two reads may straddle a minute).
    let raw = secs(&local) - secs(&utc);
    (raw as f64 / 900.0).round() as i64 * 900
}

#[cfg(not(windows))]
fn local_offset() -> i64 {
    0
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
#[cfg(any(windows, test))]
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01: (year, month, day).
pub(crate) fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// When `unix` was, as rows say it, seen from `now_unix`, in the zone
/// `offset` seconds from UTC.
pub fn seen_at(unix: u64, now_unix: u64, offset: i64) -> Seen {
    let local = unix as i64 + offset;
    let today = (now_unix as i64 + offset).div_euclid(86_400);
    let day = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let t = LocalTime::new((secs / 3600) as u8, ((secs % 3600) / 60) as u8);
    if day == today {
        Seen::Today(t)
    } else if day == today - 1 {
        Seen::Yesterday(t)
    } else {
        let (_, month, d) = civil_from_days(day);
        Seen::On(Date { day: d, month })
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The Settings port on the running app.
pub struct AppSettings {
    app: Arc<App>,
    desktop: Arc<dyn Desktop>,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    probe: Probe,
    /// Counts the probes started, so a late answer to an earlier *Find*
    /// is dropped.
    probe_seq: u64,
    /// Tone changes to undo, per speaker, newest last.
    tone_undo: HashMap<String, Vec<crate::equalizer::EqSettings>>,
    /// Music Assistant has played this PC's sound while the app ran.
    ma_played: bool,
    /// Music Assistant's code whose window was closed.
    ma_code_closed: Option<String>,
    phone_problem: Option<String>,
    sign_in_problem: Option<String>,
}

/// The repository's pages.
const REPOSITORY: &str = "https://github.com/Mihonarium/StreamToSpeaker";
const DONATE: &str = "https://buymeacoffee.com/ms00";

impl AppSettings {
    pub fn new(app: Arc<App>) -> Self {
        Self::with_desktop(app, Arc::new(SystemDesktop))
    }

    pub fn with_desktop(app: Arc<App>, desktop: Arc<dyn Desktop>) -> Self {
        AppSettings { app, desktop, inner: Arc::new(Mutex::new(Inner::default())) }
    }

    fn lang(&self) -> Lang {
        Lang::English
    }

    fn seen(&self, unix: u64) -> Seen {
        seen_at(unix, now_unix(), self.desktop.utc_offset_secs())
    }

    /// Every speaker the app lists, hidden ones included, as rows.
    fn rows(&self) -> Vec<(SpeakerRow, bool)> {
        let app = &self.app;
        let view = app.speaker_view_opts(true);
        let connecting = app.connecting_to();
        let sending = app.receiving_windows_audio();
        view.speakers
            .iter()
            .filter(|s| s.id != MA_OUTPUT_ID)
            .map(|s| {
                let (name, unsupported) = clean_name(&s.friendly_name);
                let kind = self.kind_of(&s.id, &name);
                let presence = if s.active {
                    if sending {
                        Presence::PlayingNow
                    } else {
                        Presence::Ready
                    }
                } else if connecting.as_deref().is_some_and(|c| clean_name(c).0 == name) {
                    Presence::Connecting
                } else {
                    Presence::Available
                };
                let stored = app.speaker_settings(&s.id);
                let row = SpeakerRow {
                    id: s.id.clone(),
                    name,
                    kind: kind.clone(),
                    presence,
                    added_by_address: s.manual,
                    change_ms: crate::delay::stored_change(&stored, stored.delay_choice),
                    choice: stored.delay_choice,
                    paired: app.has_airplay_pairing(&s.id),
                    untested: kind == SpeakerKind::HomePodPair,
                    cant_play: unsupported.then_some(CantPlay::AirPlayOff),
                };
                (row, s.hidden)
            })
            .collect()
    }

    /// What kind of speaker `id` is, from what discovery announced.
    fn kind_of(&self, id: &str, name: &str) -> SpeakerKind {
        let app = &self.app;
        if is_sendspin_id(id) {
            return SpeakerKind::Sendspin;
        }
        if id.starts_with("airplay:pair:") {
            return SpeakerKind::HomePodPair;
        }
        if id.starts_with("airplay:") {
            return match app.airplay_discovery.as_deref().and_then(|d| d.find_by_id(id)) {
                Some(r) if r.is_homepod() => SpeakerKind::HomePod,
                Some(r) if r.model.as_deref().is_some_and(|m| m.starts_with("AppleTV")) => SpeakerKind::AppleTv,
                _ => SpeakerKind::AirPlay,
            };
        }
        match app.discovery.as_deref().and_then(|d| d.find_by_id(id)) {
            Some(r) if r.is_sonos() && r.is_group() => SpeakerKind::SonosGroup,
            Some(r) if r.is_sonos() => SpeakerKind::Sonos,
            _ if name.contains(" + ") => SpeakerKind::SonosGroup,
            _ => SpeakerKind::Other { maker: None },
        }
    }

    fn delay_card(&self, status: &DelayStatus) -> Option<DelayCard> {
        if status.kind == Kind::MusicAssistant {
            return None;
        }
        let at_once = self.app.user_config.lock().unwrap().delay_at_once;
        Some(DelayCard { status: status.into(), measured: None, small_step_ms: 25, large_step_ms: 100, at_once })
    }

    /// The raw values behind the delay, for Details.
    fn delay_values(&self, status: &DelayStatus) -> Option<String> {
        let lang = self.lang();
        let change = crate::model::delay::change_text(lang, status.change_ms).unwrap_or_else(|| w!(lang, SetNothingChanged));
        match status.kind {
            Kind::Upnp => Some(w!(lang, SetDetailStream, change = &change)),
            Kind::AirPlay { .. } | Kind::HomePod { .. } | Kind::HomePodPair => {
                let choice = match status.choice {
                    Some(crate::delay::DelayChoice::Steady) => w!(lang, SetChoiceSteady),
                    _ => w!(lang, SetChoiceShort),
                };
                let buffer = (status.own_delay_ms as i64 + status.change_ms as i64).max(0) as u32;
                Some(w!(lang, SetDetailBuffer, buffer = buffer, choice = &choice, base = status.own_delay_ms, change = &change))
            }
            Kind::Sendspin => Some(w!(lang, SetDetailLead, change = &change)),
            Kind::MusicAssistant => None,
        }
    }

    /// The tone `id` plays with: its own, or the app-wide curve.
    fn tone_of(&self, id: &str) -> crate::equalizer::EqSettings {
        let uc = self.app.user_config.lock().unwrap();
        uc.speaker_settings.get(id).and_then(|s| s.tone.clone()).unwrap_or_else(|| uc.equalizer.clone())
    }

    fn set_tone(&self, id: &str, eq: crate::equalizer::EqSettings, undoable: bool) {
        let eq = crate::equalizer::EqSettings { enabled: true, ..eq }.sanitized();
        let before = self.tone_of(id);
        if before == eq {
            return;
        }
        if undoable {
            let mut inner = self.inner.lock().unwrap();
            let stack = inner.tone_undo.entry(id.to_string()).or_default();
            stack.push(before);
            if stack.len() > 50 {
                stack.remove(0);
            }
        }
        let mut settings = self.app.speaker_settings(id);
        settings.tone = Some(eq);
        self.app.set_speaker_settings(id, settings);
        self.app.tone_follows_speaker();
    }

    fn playing_id(&self) -> Option<String> {
        self.app.session.lock().unwrap().as_ref().map(|s| s.stable_id())
    }

    /// Start looking for a speaker at `address`, on a thread of its own.
    fn find(&self, address: String, name: String) {
        let seq = {
            let mut inner = self.inner.lock().unwrap();
            inner.probe_seq += 1;
            inner.probe = Probe::Checking { address: address.clone() };
            inner.probe_seq
        };
        let inner = self.inner.clone();
        let lang = self.lang();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-probe".into()).spawn(move || {
            let result = probe(lang, &address, &name);
            let mut i = inner.lock().unwrap();
            if i.probe_seq == seq {
                i.probe = result;
            }
        });
        if spawned.is_err() {
            self.inner.lock().unwrap().probe = Probe::Invalid(w!(lang, SetProbeNoThread));
        }
    }

    fn add(&self, kind: ManualKind, name: &str, host: &str, port: &str) {
        let r = self.app.add_manual_speaker(kind, name, host, port);
        let mut inner = self.inner.lock().unwrap();
        inner.probe = match r {
            Ok(_) => Probe::Added,
            Err(why) => Probe::Invalid(why),
        };
    }
}

/// A speaker list name without the suffixes the old window added (*(AirPlay)*,
/// *(Sendspin)*, *, password*…); and whether one said the app can't play to it.
fn clean_name(name: &str) -> (String, bool) {
    let mut n = name.trim().to_string();
    let mut unsupported = false;
    loop {
        let Some(open) = n.rfind(" (") else { break };
        if !n.ends_with(')') {
            break;
        }
        let inside = &n[open + 2..n.len() - 1];
        let ours = ["AirPlay", "Sendspin", "UPnP", "stereo pair", "added manually"].iter().any(|w| inside.contains(w));
        if !ours {
            break;
        }
        unsupported |= inside.contains("unsupported");
        n.truncate(open);
    }
    if let Some(stripped) = n.strip_suffix(", password") {
        n = stripped.to_string();
    }
    (n, unsupported)
}

/// What answers at `address`: an AirPlay receiver, a Sonos, or both.
fn probe(lang: Lang, address: &str, name: &str) -> Probe {
    let host = address.trim();
    let ip = match manual_speakers::resolve_ipv4(host, 7000) {
        Ok(ip) => ip,
        Err(why) => return Probe::Invalid(why),
    };
    let mut ways = Vec::new();
    // A Sonos answers with its description on port 1400.
    if let Ok(xml) = crate::ssdp::http_get(&format!("http://{ip}:1400/xml/device_description.xml"), Duration::from_secs(3)) {
        let room = tag(&xml, "roomName").or_else(|| tag(&xml, "friendlyName"));
        let found = if name.is_empty() { room.unwrap_or_else(|| host.to_string()) } else { name.to_string() };
        ways.push(Found { name: found, kind: SpeakerKind::Sonos, delay_ms: crate::delay::UPNP_OWN_MS, way: Way::Upnp });
    }
    // An AirPlay receiver accepts a connection on port 7000.
    if let Ok(info) = manual_speakers::probe_info(SocketAddr::new(IpAddr::V4(ip), 7000), Duration::from_secs(3)) {
        let found = if !name.is_empty() { name.to_string() } else { info.and_then(|i| i.name).unwrap_or_else(|| host.to_string()) };
        ways.push(Found { name: found, kind: SpeakerKind::AirPlay, delay_ms: crate::delay::STEADY_BASE_MS, way: Way::AirPlay });
    }
    let _ = lang;
    if ways.is_empty() {
        Probe::Nothing { address: host.to_string() }
    } else {
        Probe::Found { address: host.to_string(), ways }
    }
}

/// The text of the first `<name>` element in `xml`.
fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find("</")? + start;
    let t = xml[start..end].trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl App {
    /// Apply the tone of the speaker that plays (its own, or the app-wide
    /// equalizer it had before tone was kept per speaker) to the stream.
    /// Called whenever a session begins or ends.
    pub(crate) fn tone_follows_speaker(&self) {
        let id = self.session.lock().unwrap().as_ref().map(|s| s.stable_id());
        let eq = {
            let uc = self.user_config.lock().unwrap();
            id.and_then(|id| uc.speaker_settings.get(&id).and_then(|s| s.tone.clone())).unwrap_or_else(|| uc.equalizer.clone())
        };
        self.eq_preview(&eq);
    }

    /// Whether song titles go to the speaker that plays: its own choice,
    /// or the app-wide setting it had before.
    pub(crate) fn song_titles_for_playing(&self) -> bool {
        let id = self.session.lock().unwrap().as_ref().map(|s| s.stable_id());
        let uc = self.user_config.lock().unwrap();
        id.and_then(|id| uc.speaker_settings.get(&id).and_then(|s| s.song_titles)).unwrap_or(uc.forward_now_playing)
    }
}

impl SettingsPort for AppSettings {
    fn header(&self) -> Header {
        let lang = self.lang();
        let app = &self.app;
        let status = if let Some(c) = app.connecting_to() {
            w!(lang, ConnectingTitle, speaker = &clean_name(&c).0)
        } else if app.is_ma_output_selected() {
            w!(lang, MaPlayingTitle)
        } else if let Some(s) = app.selected_speaker() {
            let name = clean_name(&s.friendly_name).0;
            if app.receiving_windows_audio() {
                w!(lang, PlayingTitle, speaker = &name)
            } else {
                w!(lang, ReadyTitle, speaker = &name)
            }
        } else {
            w!(lang, OnPcTitle)
        };
        Header { status, lab: app.user_config.lock().unwrap().show_lab }
    }

    fn speakers(&self) -> SpeakersView {
        let app = &self.app;
        let (hidden, shown): (Vec<_>, Vec<_>) = self.rows().into_iter().partition(|(_, h)| *h);
        let networks = app.discovery.is_some().then(|| {
            let st = app.discovery_net_status();
            let adapters = app.network_adapters();
            let looking = match &st.scope {
                crate::discovery_net::Resolved::All => Looking::On(
                    adapters
                        .iter()
                        .filter_map(|a| a.usable_ipv4().map(|ip| NetworkPlace { adapter: a.name.clone(), network: None, address: ip.to_string() }))
                        .collect(),
                ),
                crate::discovery_net::Resolved::Active { name, ip } => {
                    Looking::On(vec![NetworkPlace { adapter: name.clone(), network: None, address: ip.to_string() }])
                }
                crate::discovery_net::Resolved::Paused { name } => Looking::AdapterOff { adapter: name.clone() },
            };
            Networks {
                adapters: adapters.iter().map(|a| Adapter { key: a.key.clone(), name: a.name.clone() }).collect(),
                chosen: st.selected.as_ref().map(|s| s.key.clone()),
                looking,
                pinned: st.pinned.map(|ip| ip.to_string()),
            }
        });
        SpeakersView {
            networks,
            speakers: shown.into_iter().map(|(r, _)| r).collect(),
            hidden: hidden.into_iter().map(|(r, _)| r).collect(),
            sendspin: app.is_sendspin_available().then(|| app.is_sendspin_enabled()),
        }
    }

    fn speaker(&self, id: &str) -> Option<SpeakerView> {
        let app = &self.app;
        let (row, _) = self.rows().into_iter().find(|(r, _)| r.id == id)?;
        let shown_name = row.name.clone();
        let status = app.delay_status_of(id);
        let stored = app.speaker_settings(id);
        let airplay = app.airplay_discovery.as_deref().and_then(|d| d.find_by_id(id));
        let upnp = app.discovery.as_deref().and_then(|d| d.find_by_id(id));
        let model = airplay.as_ref().and_then(|r| r.model.clone());
        let group = match row.kind {
            SpeakerKind::SonosGroup => upnp.as_ref().map(|r| {
                let mut members = vec![r.zone_name.clone().unwrap_or_else(|| r.friendly_name.clone())];
                members.extend(r.group_members.iter().cloned());
                Group::Sonos { members }
            }),
            SpeakerKind::HomePodPair => airplay.as_ref().and_then(|r| r.pair.as_ref()).and_then(|p| {
                let names: Vec<String> = p.members.iter().map(|m| m.friendly_name.clone()).collect();
                (names.len() >= 2).then(|| Group::HomePodPair { left: names[0].clone(), right: names[1].clone() })
            }),
            _ => None,
        };
        let pairing = if app.has_airplay_pairing(id) {
            Some(Pairing::Paired { on: None })
        } else if app.airplay_password(id).is_some() {
            Some(Pairing::PasswordSaved)
        } else {
            None
        };
        let comes_back = Some(match stored.auto_connect {
            AutoConnect::Default => ComesBack::WhenLastChosen,
            AutoConnect::Always => ComesBack::WhenItAppears,
            AutoConnect::Never => ComesBack::Never,
        });
        // Song titles reach AirPlay 1 receivers and Sendspin speakers.
        let titles = matches!(row.kind, SpeakerKind::AirPlay | SpeakerKind::AppleTv | SpeakerKind::Sendspin);
        let song_titles = titles.then(|| stored.song_titles.unwrap_or(app.is_forward_now_playing()));
        let tone = self.tone_of(id);
        let connection = match &row.kind {
            SpeakerKind::Sonos | SpeakerKind::SonosGroup => "UPnP, Sonos",
            SpeakerKind::Other { .. } => "UPnP",
            SpeakerKind::Sendspin => "Sendspin",
            SpeakerKind::HomePodPair => "AirPlay 2",
            _ if airplay.as_ref().is_some_and(|r| r.requires_airplay2()) => "AirPlay 2",
            _ => "AirPlay",
        };
        let address = app.speaker_view_opts(true).speakers.into_iter().find(|s| s.id == id).map(|s| s.ip);
        let last_problem = app.last_fault().filter(|f| f.before.speaker_id.as_deref() == Some(id)).map(|f| f.what);
        Some(SpeakerView {
            details: Details {
                model: model.clone(),
                address,
                connection: Some(connection.to_string()),
                id: id.to_string(),
                delay_values: self.delay_values(&status),
                last_problem,
            },
            delay: self.delay_card(&status),
            row,
            model,
            test_sound: Some(()),
            connects_with: None,
            group,
            volume_limit: Some(stored.volume_limit.unwrap_or(100).min(100)),
            tone: Some((tone.bands_db[0], tone.bands_db[crate::equalizer::EQ_BANDS - 1])),
            comes_back,
            song_titles,
            pairing,
            show_as: Some(stored.show_as.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| shown_name.clone())),
        })
    }

    fn general(&self) -> GeneralView {
        let app = &self.app;
        let problem = self.inner.lock().unwrap().sign_in_problem.clone();
        let start = match problem {
            Some(why) => Err(why),
            None => Ok(crate::autostart::is_enabled()),
        };
        GeneralView {
            what_plays: None,
            play_calls: None,
            start_at_sign_in: Some(start),
            go_back: Some(app.is_auto_reconnect_on_launch()),
            when_stops: Some(if app.is_auto_reconnect_on_drop() { WhenStops::BringBack } else { WhenStops::StaySilent }),
            notifications: None,
            shortcuts: None,
            language: None,
        }
    }

    fn music_assistant(&self) -> MaView {
        use crate::sendspin::source::SourceState;
        let app = &self.app;
        let state = app.sendspin_source_state();
        let (found_by, failed, streaming) = match &state {
            Some(SourceState::NeedsPairing { server }) => (Some((server.clone(), None)), None, false),
            Some(SourceState::Connected { server, streaming }) => (Some((server.clone(), None)), None, *streaming),
            Some(SourceState::Failed(why)) => (None, Some(why.clone()), false),
            _ => (None, None, false),
        };
        let played = {
            let mut inner = self.inner.lock().unwrap();
            inner.ma_played |= streaming;
            inner.ma_played.then_some(None)
        };
        MaView {
            offer: app.is_sendspin_available().then(|| app.is_sendspin_source_enabled() || app.user_config.lock().unwrap().sendspin.source_enabled),
            failed,
            appears_as: app.sendspin_source_name(),
            found_by,
            code: app.sendspin_source_code().map(|(code, _)| (code, None)),
            paired_with: app.sendspin_paired_servers(),
            played,
            paused: app.is_sendspin_pairing_paused(),
            token: app.is_sendspin_source_enabled().then(|| app.sendspin_pairing_token()),
        }
    }

    fn privacy(&self) -> PrivacyView {
        let app = &self.app;
        let on = app.is_web_ui_enabled();
        PrivacyView {
            only_playing: app.is_privacy_mode(),
            refused: Some(
                app.recently_refused()
                    .into_iter()
                    .map(|r| {
                        let unix = r.at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                        let at = match self.seen(unix) {
                            Seen::Today(t) | Seen::Yesterday(t) => t,
                            Seen::On(_) => LocalTime::new(0, 0),
                        };
                        let address = r.address.to_string();
                        Refused { name: (r.name != address).then_some(r.name), address, at, group_of: None }
                    })
                    .collect(),
            ),
            phone: Some(Phone { on, link: on.then(|| app.web_link()).flatten(), problem: self.inner.lock().unwrap().phone_problem.clone() }),
            api: None,
        }
    }

    fn about(&self) -> AboutView {
        let app = &self.app;
        let dev = crate::release_version().is_none();
        let st = app.update_state();
        let state = if dev {
            UpdateState::DevBuild
        } else if st.checking {
            UpdateState::Checking
        } else {
            match st.last {
                Some((UpdateOutcome::UpToDate, at)) => UpdateState::UpToDate { at: self.seen(at) },
                Some((UpdateOutcome::Available(r), at)) => {
                    UpdateState::Available { version: r.tag.trim_start_matches('v').to_string(), at: Some(self.seen(at)) }
                }
                Some((UpdateOutcome::Failed(why), _)) => UpdateState::Failed { why },
                Some((UpdateOutcome::DevBuild, _)) => UpdateState::DevBuild,
                None => match app.update_banner() {
                    Some(r) => UpdateState::Available { version: r.tag.trim_start_matches('v').to_string(), at: None },
                    None => UpdateState::NeverChecked,
                },
            }
        };
        AboutView {
            version: crate::display_version().to_string(),
            driver: None,
            updates: Updates { daily: (!dev).then(|| app.is_check_for_updates()), state, installs_here: false },
            lab: app.user_config.lock().unwrap().show_lab,
        }
    }

    fn lab(&self) -> LabView {
        let app = &self.app;
        let uc = app.user_config.lock().unwrap().clone();
        let earlier = uc.earlier_delays();
        let default = crate::delay::Earlier::default();
        let carried_over = (earlier != default).then(|| {
            let speakers = self
                .rows()
                .into_iter()
                .filter(|(r, _)| matches!(r.kind, SpeakerKind::AirPlay | SpeakerKind::AppleTv | SpeakerKind::HomePod | SpeakerKind::HomePodPair))
                .filter(|(r, _)| uc.speaker_settings.get(&r.id).map_or(true, |s| s.delay_choice.is_none()))
                .map(|(r, _)| r.name)
                .collect();
            CarriedOver {
                airplay_ms: (earlier.buffer_ms != default.buffer_ms).then_some(earlier.buffer_ms),
                homepod_ms: (earlier.homepod_ms != default.homepod_ms).then_some(earlier.homepod_ms),
                homepod_buffered: earlier.homepod_buffered,
                speakers,
            }
        });
        let counters = app.diagnostics_snapshot().rows().into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        LabView {
            at_once: Some(uc.delay_at_once),
            catch_up_frames: Some(app.latency_adjust_step_frames.load(Ordering::Relaxed)),
            drift_ppm: Some(app.rate_fudge_ppm.load(Ordering::Relaxed)),
            silence_pace_ms: Some(app.silence_pace_ms.load(Ordering::Relaxed) as u32),
            realtime: Some(uc.prefer_realtime_airplay),
            mfi: Some(uc.airplay_mfi_encryption),
            uncompressed_alac: Some(uc.airplay_uncompressed_alac),
            counters: Some(counters),
            carried_over,
        }
    }

    fn playback_check(&self) -> PlaybackCheck {
        let app = &self.app;
        let speaker = app.selected_speaker().map(|s| clean_name(&s.friendly_name).0).or_else(|| app.is_ma_output_selected().then(|| w!(self.lang(), MaName)));
        let mut c = PlaybackCheck {
            speaker: speaker.clone(),
            playing: None,
            windows: None,
            connected: None,
            taking: None,
            volume: None,
            untested: false,
            apple_tv: false,
            on_short: false,
        };
        if speaker.is_none() {
            return c;
        }
        // Each link is checked only while the ones before it hold.
        let playing = app.receiving_windows_audio();
        c.playing = Some(if playing { Ok(None) } else { Err(None) });
        if !playing {
            return c;
        }
        let bound = app.is_speaker_bound() && !app.selected_session_dead();
        c.connected = Some(if bound { Ok(()) } else { Err(()) });
        if !bound {
            return c;
        }
        let status = app.delay_status();
        let kind = status.as_ref().map(|s| s.kind);
        c.untested = kind == Some(Kind::HomePodPair);
        c.on_short = matches!(kind, Some(Kind::AirPlay { .. } | Kind::HomePod { .. }))
            && status.as_ref().and_then(|s| s.choice) == Some(crate::delay::DelayChoice::Short);
        let taking = match kind {
            Some(Kind::Upnp) => {
                if app.subscriber_count() > 0 {
                    Ok(Taking::Yes)
                } else {
                    Err(Taking::NotAsking { two_ways: false })
                }
            }
            Some(Kind::AirPlay { .. } | Kind::HomePod { .. } | Kind::HomePodPair) => Ok(Taking::CantTell),
            _ => Ok(Taking::Yes),
        };
        let broken = taking.is_err();
        c.taking = Some(taking);
        if broken {
            return c;
        }
        c.volume = app.current_volume().map(|v| if v == 0 { Err(Volume::Zero) } else { Ok(Volume::Level(v.min(100) as u8)) });
        c
    }

    fn discovery_check(&self) -> DiscoveryCheck {
        let app = &self.app;
        let mut lines = Vec::new();
        let st = app.discovery_net_status();
        let adapters = app.network_adapters();
        let found = app.speaker_view().speakers.iter().filter(|s| s.id != MA_OUTPUT_ID).count();
        match &st.scope {
            crate::discovery_net::Resolved::Paused { name } => lines.push(FindLine::AdapterOff { adapter: name.clone() }),
            crate::discovery_net::Resolved::Active { name, ip } => {
                lines.push(FindLine::LookingOn { place: NetworkPlace { adapter: name.clone(), network: None, address: subnet(&ip.to_string()) }, found })
            }
            crate::discovery_net::Resolved::All => {
                let usable: Vec<_> = adapters.iter().filter_map(|a| a.usable_ipv4().map(|ip| (a, ip))).collect();
                let vpn = |n: &str| {
                    let n = n.to_lowercase();
                    ["vpn", "tap-", "wireguard", "tailscale", "zerotier", "openvpn", "anyconnect"].iter().any(|w| n.contains(w))
                };
                let home: Vec<_> = usable.iter().filter(|(a, _)| !vpn(&a.name)).collect();
                for (i, (a, ip)) in home.iter().enumerate() {
                    lines.push(FindLine::LookingOn {
                        place: NetworkPlace { adapter: a.name.clone(), network: None, address: subnet(&ip.to_string()) },
                        found: if i == 0 { found } else { 0 },
                    });
                }
                if let (Some((v, vip)), Some((h, _))) = (usable.iter().find(|(a, _)| vpn(&a.name)), home.first()) {
                    lines.push(FindLine::Vpn {
                        vpn: NetworkPlace { adapter: v.name.clone(), network: None, address: subnet(&vip.to_string()) },
                        only: Adapter { key: h.key.clone(), name: h.name.clone() },
                    });
                }
            }
        }
        if st.error.is_some() {
            lines.push(FindLine::AirPlaySearchFailed);
        }
        DiscoveryCheck { lines }
    }

    fn wanted_dialog(&self) -> Option<Wanted> {
        let app = &self.app;
        if let Some((id, name)) = app.pending_pin_pairing() {
            return Some(Wanted::PairingCode(CodeAsk { id, name: clean_name(&name).0, stage: CodeStage::Showing, seconds_left: None }));
        }
        if let Some(p) = app.pending_sendspin_pairing() {
            return Some(Wanted::SendspinCode(SendspinAsk {
                id: p.id,
                name: clean_name(&p.name).0,
                live_code: p.kind != PairPromptKind::TokenOnly,
                seconds_left: None,
                identity_changed: p.identity_changed,
                lost: p.lost_credential,
                wrong: false,
            }));
        }
        if let Some((code, _)) = app.sendspin_source_code() {
            if self.inner.lock().unwrap().ma_code_closed.as_deref() != Some(code.as_str()) {
                return Some(Wanted::MaCode(MaCodeShow { code, this_pc: app.sendspin_source_name(), seconds_left: None }));
            }
        }
        None
    }

    fn probe(&self) -> Probe {
        self.inner.lock().unwrap().probe.clone()
    }

    fn tone(&self, id: &str) -> Option<ToneView> {
        let name = self.rows().into_iter().find(|(r, _)| r.id == id)?.0.name;
        let playing = self.playing_id().as_deref() == Some(id);
        Some(ToneView {
            name,
            eq: self.tone_of(id),
            headroom_db: playing.then(|| self.app.eq_report()).flatten().map(|r| r.auto_attenuation_db).filter(|d| *d < 0.0),
            can_undo: self.inner.lock().unwrap().tone_undo.get(id).is_some_and(|s| !s.is_empty()),
        })
    }

    fn report(&self, parts: ReportParts) -> String {
        let lang = self.lang();
        let mut out = Vec::new();
        if parts.versions {
            out.push(w!(lang, SetReportVersionLine, version = crate::display_version(), os = std::env::consts::OS));
        }
        if parts.last_hour {
            out.push(self.app.diagnostics_report());
        }
        if parts.speakers {
            for s in self.app.speaker_view_opts(true).speakers {
                out.push(format!("{} — {} ({})", s.friendly_name, s.ip, s.id));
            }
        }
        if parts.log {
            if let Some(text) = crate::log_dir().and_then(|d| newest_log(&d)).and_then(|p| std::fs::read_to_string(p).ok()) {
                // The end of the log: what happened most recently.
                let lines: Vec<&str> = text.lines().collect();
                let from = lines.len().saturating_sub(400);
                out.push(lines[from..].join("\n"));
            }
        }
        out.join("\n\n")
    }

    fn act(&self, act: Act) {
        let app = &self.app;
        let described = format!("{act:?}");
        match act {
            Act::LookOn(key) => {
                let choice = key
                    .and_then(|k| app.network_adapters().into_iter().find(|a| a.key == k))
                    .map(|a| crate::discovery_net::SavedAdapter { key: a.key, name: a.name });
                app.set_discovery_adapter(choice);
            }
            Act::LookOnlyOn(key) => self.act(Act::LookOn(Some(key))),
            Act::SearchAgain => app.trigger_rescan(),
            Act::ShowSpeaker(id) => self.set_hidden(&id, false),
            Act::HideSpeaker(id) => self.set_hidden(&id, true),
            Act::SendspinSpeakers(on) => app.set_sendspin_enabled(on),
            Act::PlayOn(id) => app.select_speaker_async(&id),
            Act::Delay(id, d) => {
                let r = match d {
                    DelayAct::Remove(n) => app.change_delay(Some(&id), Request::Remove(n)).map(|_| ()),
                    DelayAct::Add(n) => app.change_delay(Some(&id), Request::Add(n)).map(|_| ()),
                    DelayAct::BackToOwn => app.change_delay(Some(&id), Request::Set(0)).map(|_| ()),
                    DelayAct::Choose(c) => app.set_delay_choice(&id, c).map(|_| ()),
                };
                if let Err(e) = r {
                    log::info!("settings: delay of {id} not changed: {e:?}");
                }
            }
            Act::Tone(id, t) => {
                let mut eq = self.tone_of(&id);
                match t {
                    ToneAct::Band(i, db) => eq.bands_db[i.min(crate::equalizer::EQ_BANDS - 1)] = db,
                    ToneAct::Preamp(db) => eq.preamp_db = db,
                    ToneAct::Bass(b) => {
                        eq.bands_db[0] = b;
                        eq.bands_db[1] = b;
                        eq.bands_db[2] = (b / 2.0 * 2.0).round() / 2.0;
                    }
                    ToneAct::Treble(tr) => {
                        let n = crate::equalizer::EQ_BANDS;
                        eq.bands_db[n - 3] = tr;
                        eq.bands_db[n - 2] = tr;
                        eq.bands_db[n - 1] = tr;
                    }
                    ToneAct::Preset(i) => {
                        if let Some(p) = EQ_PRESETS.get(i) {
                            eq.bands_db = p.bands_db;
                        }
                    }
                    ToneAct::Flat => eq = crate::equalizer::EqSettings::default(),
                    ToneAct::Undo => {
                        let last = self.inner.lock().unwrap().tone_undo.get_mut(&id).and_then(|s| s.pop());
                        if let Some(prev) = last {
                            self.set_tone(&id, prev, false);
                        }
                        return;
                    }
                }
                self.set_tone(&id, eq, true);
            }
            Act::ComesBack(id, c) => {
                let mut s = app.speaker_settings(&id);
                s.auto_connect = match c {
                    ComesBack::WhenLastChosen => AutoConnect::Default,
                    ComesBack::WhenItAppears => AutoConnect::Always,
                    ComesBack::Never => AutoConnect::Never,
                };
                app.set_speaker_settings(&id, s);
            }
            Act::SongTitles(id, on) => {
                let mut s = app.speaker_settings(&id);
                s.song_titles = Some(on);
                app.set_speaker_settings(&id, s);
            }
            Act::PairAgain(id) => {
                app.forget_airplay_pairing(&id);
                app.select_speaker_async(&id);
            }
            Act::ForgetPairing(id) => app.forget_airplay_pairing(&id),
            Act::RemovePassword(id) => {
                let mut uc = app.user_config.lock().unwrap();
                uc.remove_airplay_password(&id);
                uc.save();
            }
            Act::Forget(id) => {
                // Its pairing, password, volume, delay changes, tone and
                // "comes back" choice go, and nothing else. If it plays,
                // the session ends (on a thread: stopping talks to it).
                let playing = self.playing_id().as_deref() == Some(id.as_str());
                {
                    let mut uc = app.user_config.lock().unwrap();
                    uc.speaker_settings.remove(&id);
                    uc.remove_airplay_password(&id);
                    uc.remove_airplay_pairing(&id);
                    if uc.last_speaker_id.as_deref() == Some(id.as_str()) {
                        uc.last_speaker_id = None;
                    }
                    uc.save();
                }
                self.inner.lock().unwrap().tone_undo.remove(&id);
                if playing {
                    let app = app.clone();
                    let _ = std::thread::Builder::new().name("stream-to-speaker-forget".into()).spawn(move || {
                        let _ = app.set_streaming_enabled(false);
                    });
                }
            }
            Act::Remove(id) => app.remove_manual_speaker(&id),
            Act::TestSound(id) => {
                // Where the sound is: at once. Another speaker is played by
                // the engine's own command from the window (it moves the
                // sound there first), never from here.
                if self.playing_id().as_deref() == Some(id.as_str()) {
                    app.play_test_sound();
                }
            }
            Act::VolumeLimit(id, limit) => {
                let mut s = app.speaker_settings(&id);
                s.volume_limit = (limit < 100).then_some(limit);
                app.set_speaker_settings(&id, s);
            }
            Act::ShowAs(id, name) => {
                let mut s = app.speaker_settings(&id);
                // An empty name gives the speaker its own name back.
                let name = name.trim().to_string();
                s.show_as = (!name.is_empty()).then_some(name);
                app.set_speaker_settings(&id, s);
            }
            Act::WhenStops(w) => app.set_auto_reconnect_on_drop(w == WhenStops::BringBack),
            Act::Allow { address, .. } => match address.parse::<IpAddr>() {
                Ok(ip) => app.allow_device(ip),
                Err(_) => log::warn!("settings: can't allow {address}: not an address"),
            },
            Act::StartAtSignIn(on) => {
                let r = crate::autostart::set_enabled(on);
                self.inner.lock().unwrap().sign_in_problem = r.err();
            }
            Act::GoBack(on) => app.set_auto_reconnect_on_launch(on),
            Act::OfferToMa(on) => {
                if let Err(e) = app.set_sendspin_source_enabled(on) {
                    log::warn!("settings: Music Assistant input: {e}");
                }
            }
            Act::Unpair(server) => {
                let mut uc = app.user_config.lock().unwrap();
                uc.sendspin.server_pairings.retain(|p| p.server_name.as_deref().unwrap_or("Music Assistant") != server);
                uc.save();
            }
            Act::AllowPairingAgain => app.allow_sendspin_pairing_again(),
            Act::AppearsAs(name) => {
                {
                    let mut uc = app.user_config.lock().unwrap();
                    uc.sendspin.source_name = Some(name);
                    uc.save();
                }
                // The new name is announced when the input starts again.
                if app.is_sendspin_source_enabled() {
                    let app = app.clone();
                    let _ = std::thread::Builder::new().name("stream-to-speaker-ma-rename".into()).spawn(move || {
                        let _ = app.set_sendspin_source_enabled(false);
                        let _ = app.set_sendspin_source_enabled(true);
                    });
                }
            }
            Act::CancelMaPairing => {
                self.inner.lock().unwrap().ma_code_closed = app.sendspin_source_code().map(|(c, _)| c);
            }
            Act::OnlyPlaying(on) => app.set_privacy_mode(on),
            Act::Phone(on) => {
                let r = app.set_web_ui_enabled(on);
                self.inner.lock().unwrap().phone_problem = r.err();
            }
            Act::NewLink => {
                if let Err(e) = app.new_web_link() {
                    self.inner.lock().unwrap().phone_problem = Some(e);
                }
            }
            Act::OpenLogFolder => {
                if let Some(d) = crate::log_dir() {
                    self.desktop.open_folder(&d);
                }
            }
            Act::ReadGuideOnline => self.desktop.open_url(&format!("{REPOSITORY}#readme")),
            Act::ShowWelcomeAgain => app.reset_onboarding(),
            Act::Reconnect | Act::TryAirPlay => {
                if app.is_speaker_bound() {
                    app.reconnect_speaker();
                } else if let Some(id) = app.saved_speaker_id() {
                    app.select_speaker_async(&id);
                }
            }
            Act::TrySteady => {
                if let Some(s) = app.delay_status() {
                    let _ = app.set_delay_choice(&s.id, crate::delay::DelayChoice::Steady);
                }
            }
            Act::SetVolume(v) => app.set_speaker_volume(v as u32),
            Act::DailyUpdateCheck(on) => app.set_check_for_updates(on),
            Act::CheckNow => app.check_for_updates_now(),
            Act::WhatsNew | Act::UpdateNow => {
                let url = app.update_banner().map(|r| r.url).unwrap_or_else(|| crate::update_check::RELEASES_PAGE.to_string());
                self.desktop.open_url(&url);
            }
            Act::SkipVersion => {
                if let Some(r) = app.update_banner() {
                    app.skip_update(&r.tag);
                }
            }
            Act::Donate => self.desktop.open_url(DONATE),
            Act::LicenceAndSource => self.desktop.open_url(REPOSITORY),
            Act::ShowLab(on) => {
                let mut uc = app.user_config.lock().unwrap();
                if uc.show_lab != on {
                    uc.show_lab = on;
                    uc.save();
                }
            }
            Act::Lab(l) => self.lab_act(l),
            Act::SubmitCode { code, .. } => {
                if !app.submit_pin(Some(code)) {
                    app.record_error(w!(self.lang(), SetPairingEnded));
                }
            }
            Act::NewCode(id) => {
                let _ = app.submit_pin(None);
                app.select_speaker_async(&id);
            }
            Act::CancelPairing(_) => {
                let _ = app.submit_pin(None);
            }
            Act::SubmitPassword { id, password, connect } => {
                let name = self.rows().into_iter().find(|(r, _)| r.id == id).map(|(r, _)| r.name).unwrap_or_else(|| id.clone());
                let dialog = app.password_dialog(&id, &name);
                let r = if connect { app.submit_password_dialog(&dialog, &password) } else { app.enter_airplay_password(&dialog, &password) };
                if let Err(e) = r {
                    app.record_error(e);
                }
            }
            Act::SubmitSendspin { code, .. } => app.submit_sendspin_pairing(Some(code)),
            Act::CancelSendspin(_) => app.submit_sendspin_pairing(None),
            Act::Find { address, name } => self.find(address, name),
            Act::AddFound { way, name } => {
                let address = match self.probe() {
                    Probe::Found { address, .. } => address,
                    _ => return,
                };
                match way {
                    Way::AirPlay => self.add(ManualKind::AirPlay, &name, &address, "7000"),
                    Way::Upnp => {
                        let host = match manual_speakers::resolve_ipv4(&address, 1400) {
                            Ok(ip) => ip.to_string(),
                            Err(_) => address.clone(),
                        };
                        self.add(ManualKind::Upnp, &name, &format!("http://{host}:1400/xml/device_description.xml"), "")
                    }
                }
            }
            Act::AddByHand { airplay, host, port, description, name } => {
                if airplay {
                    let port = port.map(|p| p.to_string()).unwrap_or_else(|| "7000".into());
                    self.add(ManualKind::AirPlay, &name, &host, &port);
                } else {
                    self.add(ManualKind::Upnp, &name, &description, "");
                }
            }
            Act::ResetProbe => {
                let mut inner = self.inner.lock().unwrap();
                inner.probe_seq += 1;
                inner.probe = Probe::Idle;
            }
            Act::SaveReport(text) => {
                if let Some(dir) = crate::log_dir() {
                    let name = format!("report-{}.txt", now_unix());
                    let path: PathBuf = dir.join(name);
                    match std::fs::write(&path, text) {
                        Ok(()) => self.desktop.open_folder(&dir),
                        Err(e) => app.record_error(format!("{e}")),
                    }
                }
            }
            Act::OpenIssue(_) => self.desktop.open_url(&format!("{REPOSITORY}/issues/new")),
            // Controls the real app's views leave out.
            | Act::ConnectWith { .. }
            | Act::WhatPlays(_)
            | Act::PlayCalls(_)
            | Act::Notifications(_)
            | Act::Shortcuts(_)
            | Act::ShortcutKeys(..)
            | Act::Language(_)
            | Act::OpenVolumeMixer
            | Act::OpenMusicAssistant
            | Act::Api(_)
            | Act::NewKey
            | Act::Unmute
            | Act::SendToUs
            | Act::OpenNetworkSettings
            | Act::AllowThroughFirewall
            | Act::OpenApiHelp
            | Act::Install
            | Act::CancelInstall
            | Act::RestartWindows
            | Act::CancelPassword(_) => log::debug!("settings: {described} isn't offered by this version"),
        }
    }
}

impl AppSettings {
    fn set_hidden(&self, id: &str, hidden: bool) {
        let mut s = self.app.speaker_settings(id);
        s.hidden = hidden;
        self.app.set_speaker_settings(id, s);
    }

    fn lab_act(&self, l: LabAct) {
        let app = &self.app;
        let set_config = |f: &dyn Fn(&mut crate::user_config::UserConfig)| {
            let mut uc = app.user_config.lock().unwrap();
            f(&mut uc);
            uc.save();
        };
        match l {
            LabAct::AtOnce(on) => {
                set_config(&|uc| uc.delay_at_once = on);
                app.delay.catch_up().set_at_once(on);
            }
            LabAct::CatchUpFrames(f) => app.set_latency_adjust_step_frames(f),
            LabAct::DriftPpm(p) => app.set_rate_fudge_ppm(p),
            LabAct::SilencePace(ms) => app.set_silence_pace_ms(ms as u64),
            LabAct::Realtime(on) => set_config(&|uc| uc.prefer_realtime_airplay = on),
            LabAct::Mfi(on) => set_config(&|uc| uc.airplay_mfi_encryption = on),
            LabAct::UncompressedAlac(on) => set_config(&|uc| uc.airplay_uncompressed_alac = on),
            LabAct::ResetAll => app.turn_off_lab(),
            LabAct::ResetCarriedOver => set_config(&|uc| {
                uc.airplay_latency_ms = crate::user_config::AIRPLAY_LATENCY_MS_DEFAULT;
                uc.airplay_homepod_latency_ms = crate::user_config::HOMEPOD_LATENCY_MS_DEFAULT;
                uc.airplay_homepod_buffered = false;
            }),
        }
    }
}

/// `192.168.1.23` → `192.168.1.x`: the network, not this PC.
fn subnet(ip: &str) -> String {
    match ip.rsplit_once('.') {
        Some((net, _)) => format!("{net}.x"),
        None => ip.to_string(),
    }
}

/// The newest file in the log folder.
fn newest_log(dir: &std::path::Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}
