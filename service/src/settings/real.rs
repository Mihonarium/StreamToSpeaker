//! The Settings port on the running app: [`AppSettings`].
//!
//! Every view is read from [`App`] and its settings file when a page asks;
//! every act changes the setting at once and saves it. What the app can't
//! do yet is `None` in the views, so the page leaves that control out.
//! Work that touches the network (connecting, probing an address) runs on
//! a thread of its own; nothing here waits for a speaker.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::{App, UpdateOutcome};
use crate::delay::Kind;
use crate::discovery_net::{NetAdapter, Resolved};
use crate::equalizer::EQ_PRESETS;
use crate::manual_speakers::{self, ManualKind};
use crate::model::facts::NetworkSeen;
use crate::model::situation::ConnKind;
use crate::model::text::{Lang, LocalTime};
use crate::sendspin_app::{is_sendspin_id, PairPromptKind, MA_OUTPUT_ID};

use super::port::*;
use super::words::w;

/// What the app asks of the desktop around it: opening a page in the
/// browser or a folder, and the time zone. Behind a trait so tests can see
/// what was opened.
pub trait Desktop: Send + Sync {
    fn open_url(&self, url: &str);
    fn open_folder(&self, dir: &std::path::Path) -> std::io::Result<()>;
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

    fn open_folder(&self, dir: &std::path::Path) -> std::io::Result<()> {
        #[cfg(windows)]
        { crate::shell_open(dir.as_os_str()) }
        #[cfg(not(windows))]
        { Err(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("cannot open {}", dir.display()))) }
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

/// Whether the check says that an AirPlay speaker hasn't reached this PC:
/// of a speaker that stays silent until it has, and of any other on a
/// network Windows treats as public, where Windows Firewall may be why. A
/// speaker of the second kind may play without ever reaching this PC.
pub fn says_not_reached(reached: Option<crate::airplay::Reached>, public: bool) -> bool {
    matches!(reached, Some(crate::airplay::Reached::No { silent }) if silent || public)
}

/// The day `unix` fell on, in the zone `offset` seconds from UTC, unless
/// that is `now_unix`'s own day.
pub fn earlier_day(unix: u64, now_unix: u64, offset: i64) -> Option<Date> {
    let day = (unix as i64 + offset).div_euclid(86_400);
    ((now_unix as i64 + offset).div_euclid(86_400) != day).then(|| {
        let (_, month, d) = civil_from_days(day);
        Date { day: d, month }
    })
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

    /// This PC's network connections as Windows describes them.
    fn networks(&self) -> Vec<NetworkSeen> {
        self.app.engine_view().map(|view| view.scene.networks.clone()).unwrap_or_default()
    }

    /// A device that asked for the sound during the present connection to
    /// `speaker` and that privacy still refuses.
    fn refused_now(&self, speaker: &crate::model::SpeakerId) -> Option<crate::app::RefusedDevice> {
        let app = &self.app;
        if !app.is_privacy_mode() {
            return None;
        }
        let session = app.session_number();
        app.recently_refused().into_iter().find(|r| {
            r.session == session && r.speaker.as_deref().is_some_and(|id| crate::engine::place_driver::speaker_for_pick(app, id) == *speaker)
        })
    }

    /// Whether `address` asked for the sound and was refused while the app
    /// was connected to `speaker` (to no speaker, for an empty one).
    fn was_refused(&self, address: IpAddr, speaker: &str) -> bool {
        let app = &self.app;
        let address = crate::stream_gate::canonical_ip(address);
        app.recently_refused().iter().any(|r| {
            let beside = r.speaker.as_deref().map(|id| crate::engine::place_driver::speaker_for_pick(app, id));
            r.address == address && beside.as_ref().map_or("", |id| id.as_str()) == speaker
        })
    }

    /// The network this PC reaches the connected speaker over, by the name
    /// the search check gives it, where Windows treats it as public.
    fn public_network_toward_speaker(&self) -> Option<String> {
        let app = &self.app;
        let IpAddr::V4(ours) = app.local_ip_toward(app.selected_speaker()?.ip).ok()? else {
            return None;
        };
        let adapters = app.network_adapters();
        let adapter = adapters.iter().find(|a| a.ipv4.contains(&ours))?;
        let (network, private) = profile_of(&self.networks(), adapter)?;
        (!private).then_some(network)
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
                    name: stored.show_as.clone().filter(|n| !n.trim().is_empty()).unwrap_or(name),
                    kind: kind.clone(),
                    presence,
                    added_by_address: s.manual,
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

    /// The tone `id` plays with: its own, or the app-wide curve.
    fn tone_of(&self, id: &str) -> crate::equalizer::EqSettings {
        let uc = self.app.user_config.lock().unwrap();
        uc.speaker_settings.get(id).and_then(|s| s.tone.clone()).unwrap_or_else(|| uc.equalizer.clone())
    }

    /// Use the panel's identity check before applying a speaker setting to both ways.
    fn airplay_twin(&self, id: &str) -> Option<crate::airplay::AirPlayRenderer> {
        let native = self.app.discovery.as_ref()?.find_by_id(id)?;
        self.app.airplay_discovery.as_ref()?.renderers().into_iter().find(|r| {
            r.ip == native.ip && crate::engine::place_driver::speaker_for_pick(&self.app, &r.stable_id()).as_str() == id
        })
    }

    fn transport_ids(&self, id: &str) -> Vec<String> {
        let mut ids = vec![id.to_owned()];
        if let Some(twin) = self.airplay_twin(id) { ids.push(twin.stable_id()); }
        ids
    }

    /// Keep edits to both ways together with other fields the engine may have saved.
    fn change_speaker(&self, id: &str, edit: impl Fn(&mut crate::user_config::SpeakerSettings)) -> Vec<String> {
        let ids = self.transport_ids(id);
        let mut cfg = self.app.user_config.lock().unwrap();
        for id in &ids {
            if crate::airplay::discovery::is_instance_id(id) { continue; }
            let entry = cfg.speaker_settings.entry(id.clone()).or_default();
            edit(entry);
            if entry.is_default() { cfg.speaker_settings.remove(id); }
        }
        cfg.save();
        ids
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
    fn accepts_shortcuts(&self, keys: &crate::platform::DelayKeys) -> bool {
        self.app.delay_shortcuts.lock().unwrap().upgrade().is_some_and(|s| s.lock().unwrap().accepts(keys))
    }

    #[cfg(windows)]
    fn windows_key_down(&self) -> bool {
        crate::platform::win::windows_key_down()
    }

    fn speakers(&self) -> SpeakersView {
        let app = &self.app;
        let (hidden, shown): (Vec<_>, Vec<_>) = self.rows().into_iter().partition(|(_, h)| *h);
        let networks = app.discovery.is_some().then(|| {
            let st = app.discovery_net_status();
            let adapters = app.network_adapters();
            let networks = self.networks();
            let looking = match &st.scope {
                Resolved::All => Looking::On(
                    adapters
                        .iter()
                        .filter_map(|a| {
                            a.usable_ipv4().map(|ip| NetworkPlace { adapter: a.name.clone(), network: network_name(&networks, a), address: ip.to_string() })
                        })
                        .collect(),
                ),
                Resolved::Active { name, ip } => {
                    let network = adapters.iter().find(|a| a.ipv4.contains(ip)).and_then(|a| network_name(&networks, a));
                    Looking::On(vec![NetworkPlace { adapter: name.clone(), network, address: ip.to_string() }])
                }
                Resolved::Paused { name } => Looking::AdapterOff { adapter: name.clone() },
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
        let row = self.rows().into_iter().find(|(r, _)| r.id == id).map(|(r, _)| r).or_else(|| {
            let cfg = app.user_config.lock().unwrap();
            let stored = cfg.speaker_settings.get(id);
            let remembered = cfg.sound_place.as_ref().is_some_and(|p| p.named.iter().chain(&p.place).any(|k| k.as_str() == id));
            if stored.is_none() && !remembered { return None; }
            let name = stored.and_then(|s| s.show_as.clone()).unwrap_or_else(|| id.into());
            drop(cfg);
            Some(SpeakerRow { id: id.into(), kind: self.kind_of(id, &name), name, presence: Presence::NotFound { last_seen: None },
                added_by_address: false, cant_play: None })
        })?;
        let shown_name = row.name.clone();
        let stored = app.speaker_settings(id);
        let twin = self.airplay_twin(id);
        let airplay = app.find_airplay_renderer(id).or(twin.clone());
        let model = app.discovery.as_ref().and_then(|d| d.find_by_id(id)).and_then(|r| r.model_name)
            .or_else(|| airplay.as_ref().and_then(|r| r.model.clone()));
        // Song titles reach AirPlay 1 receivers and Sendspin speakers.
        let titles = matches!(row.kind, SpeakerKind::Sendspin) || airplay.as_ref().map_or(
            matches!(row.kind, SpeakerKind::AirPlay | SpeakerKind::AppleTv),
            |r| r.transport() == Some(crate::airplay::Transport::RaopLegacy),
        );
        let song_titles = titles.then(|| stored.song_titles.unwrap_or(app.is_forward_now_playing()));
        let tone = self.tone_of(id);
        let connection = match &row.kind {
            SpeakerKind::Sonos | SpeakerKind::SonosGroup if twin.is_some() => "Sonos / AirPlay",
            SpeakerKind::Sonos | SpeakerKind::SonosGroup => "UPnP, Sonos",
            SpeakerKind::Other { .. } => "UPnP",
            SpeakerKind::Sendspin => "Sendspin",
            SpeakerKind::HomePodPair => "AirPlay 2",
            _ if airplay.as_ref().is_some_and(|r| r.requires_airplay2()) => "AirPlay 2",
            _ => "AirPlay",
        };
        let address = app.speaker_view_opts(true).speakers.into_iter().find(|s| s.id == id).map(|s| s.ip);
        Some(SpeakerView {
            details: Details {
                model: model.clone(),
                address,
                connection: Some(connection.to_string()),
            },
            row,
            volume_limit: Some(stored.volume_limit.unwrap_or(100).min(100)),
            tone: Some((tone.bands_db[0], tone.bands_db[crate::equalizer::EQ_BANDS - 1])),
            song_titles,
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
        let cfg = app.user_config.lock().unwrap();
        GeneralView {
            what_plays: Some(if cfg.only_some_apps { WhatPlays::OnlySentApps } else { WhatPlays::Everything }),
            play_calls: Some(cfg.play_calls),
            switch_back_after_quiet: cfg.switch_back_after_quiet,
            notes_by_clock: cfg.notes_by_clock,
            start_at_sign_in: Some(start),
            shortcuts: Some(Shortcuts {
                on: cfg.delay_shortcuts,
                remove: cfg.delay_keys.0[0].clone(),
                add: cfg.delay_keys.0[1].clone(),
                larger: "Shift".into(),
                status: app.delay_shortcuts.lock().unwrap().upgrade()
                    .map_or([crate::platform::KeyStatus::Rejected; 3], |keys| keys.lock().unwrap().status()),
            }),
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
        // The last step stays done across restarts: the time it last
        // played is saved. Today's is said without its date.
        let played_at = app.user_config.lock().unwrap().sendspin.source_played_unix;
        let played = if streaming {
            Some(None)
        } else {
            played_at.map(|t| earlier_day(t, now_unix(), self.desktop.utc_offset_secs()))
        };
        // Music Assistant has to reach this PC. While it hasn't, a network
        // the app looks on that Windows treats as public is worth saying.
        let public_network = (found_by.is_none() && failed.is_none())
            .then(|| find_lines(&app.discovery_net_status().scope, &app.network_adapters(), &self.networks(), 0))
            .into_iter()
            .flatten()
            .find_map(|l| match l {
                FindLine::Profile { network, private: false } => Some(network),
                _ => None,
            });
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
            public_network,
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
                        let at = self.seen(unix);
                        let address = r.address.to_string();
                        let group_of = r.speaker.map(|id| {
                            let id = crate::engine::place_driver::speaker_for_pick(app, &id).as_str().to_owned();
                            let name = app.speaker_name(&id).unwrap_or_else(|| id.clone());
                            (id, name)
                        });
                        Refused { name: (r.name != address).then_some(r.name), address, at, group_of }
                    })
                    .collect(),
            ),
            phone: Some(Phone { on, link: on.then(|| app.web_link()).flatten(), problem: self.inner.lock().unwrap().phone_problem.clone() }),
            api: Some({
                let cfg = app.user_config.lock().unwrap();
                Api { on: cfg.scripts_enabled, key: cfg.scripts_key.clone() }
            }),
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
            show_lab: app.user_config.lock().unwrap().show_lab,
            updates: Updates { daily: (!dev).then(|| app.is_check_for_updates()), state, installs_here: false },
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
        use crate::model::situation::{Doing, Standing};
        let app = &self.app;
        let view = app.engine_view();
        let place = view.as_ref().and_then(|view| view.scene.situation.place_speaker().map(|id| (&view.scene, id)));
        let mut c = PlaybackCheck {
            speaker: place.map(|(scene, id)| scene.speaker_name(id)),
            playing: None,
            windows: None,
            connected: None,
            taking: None,
            turned_away: None,
            public_network: None,
            not_reached: false,
            volume: None,
            untested: false,
            apple_tv: false,
            on_short: false,
            server_away: false,
        };
        let Some((scene, id)) = place else {
            return c;
        };
        c.apple_tv = scene.speaker(id).is_some_and(|row| row.kind == SpeakerKind::AppleTv);
        // Each link is checked only while the ones before it hold. Whether
        // something plays, an app on another output and the volume come
        // from the state the panel shows; that the app can't read the
        // sound, and the links about the connection, come from the app.
        //
        // 1. Something plays: sound that goes to the place, or an app that
        // stayed on another output of this PC, which is then named.
        let elsewhere = scene.situation.standing.iter().find_map(|s| match s {
            Standing::AppElsewhere { app, output } => {
                let output = scene.outputs.iter().find(|o| o.id == *output).map_or_else(|| output.as_str().to_string(), |o| scene.output_label(o));
                Some(WindowsSends::AppElsewhere { app: app.clone(), output })
            }
            _ => None,
        });
        let sent = matches!(scene.situation.doing, Doing::Playing | Doing::Connecting | Doing::Trying);
        c.playing = Some(match &elsewhere {
            _ if sent => Ok(None),
            Some(WindowsSends::AppElsewhere { app, .. }) => Ok(Some(app.clone())),
            _ => Err(None),
        });
        // 2. Windows sends it to the app's output, and the app reads it
        // there. An app that stayed on another output ends the check only
        // while nothing goes to the place: the speaker may be playing the
        // rest, and the links after this one say whether it does.
        let lost = sent && app.sound_input_lost();
        c.windows = Some(match elsewhere {
            _ if lost => Err(WindowsSends::InputLost),
            Some(found) if sent => Ok(found),
            Some(found) => Err(found),
            None if sent => Ok(WindowsSends::ToUs),
            None => return c,
        });
        if matches!(c.windows, Some(Err(_))) {
            return c;
        }
        let level = scene.volume.place.as_ref().map(|l| (l.value, l.muted));
        // 3. Connected. Music Assistant's server connects to the app:
        // choosing Music Assistant binds a session with or without it.
        c.server_away = id.is_music_assistant()
            && !matches!(app.sendspin_source_state(), Some(crate::sendspin::source::SourceState::Connected { .. }));
        let bound = app.is_speaker_bound() && !app.selected_session_dead() && !c.server_away;
        c.connected = Some(if bound { Ok(()) } else { Err(()) });
        if !bound {
            return c;
        }
        let status = app.delay_status();
        let kind = status.as_ref().map(|s| s.kind);
        c.untested = kind == Some(Kind::HomePodPair);
        c.on_short = matches!(kind, Some(Kind::AirPlay { .. } | Kind::HomePod { .. }))
            && status.as_ref().and_then(|s| s.choice) == Some(crate::delay::DelayChoice::Short);
        // A pause pressed on the speaker that left sound going, sent and
        // not heard: the panel's note says the same.
        let paused = scene.situation.standing.iter().any(|s| matches!(s, Standing::PausedAtSpeaker { .. }));
        let taking = match kind {
            _ if paused => Err(Taking::Paused),
            Some(Kind::Upnp) => {
                if app.subscriber_count() > 0 {
                    Ok(Taking::Yes)
                } else {
                    // Nothing fetches the sound. A device privacy turned
                    // away is said beside that; whether it plays the
                    // speaker's sound, the app can't tell.
                    c.turned_away = self.refused_now(id).map(|refused| {
                        let address = refused.address.to_string();
                        TurnedAway { device: (refused.name != address).then_some(refused.name), address }
                    });
                    // With no device turned away either, Windows Firewall
                    // may be what keeps the speaker out: it is stricter on
                    // a network Windows treats as public.
                    if c.turned_away.is_none() {
                        c.public_network = self.public_network_toward_speaker();
                    }
                    // The speaker can be reached by AirPlay as well.
                    Err(Taking::NotAsking { two_ways: scene.speaker(id).is_some_and(|row| row.airplay_way().is_some()) })
                }
            }
            Some(Kind::AirPlay { .. } | Kind::HomePod { .. } | Kind::HomePodPair) => {
                // An AirPlay speaker keeps time with this PC and has to
                // reach it for that. Whether one that hasn't plays, the
                // app still can't tell.
                let reached = app.speaker_reached_this_pc();
                if matches!(reached, Some(crate::airplay::Reached::No { .. })) {
                    let public = self.public_network_toward_speaker();
                    if says_not_reached(reached, public.is_some()) {
                        c.not_reached = true;
                        c.public_network = public;
                    }
                }
                Ok(Taking::CantTell)
            }
            // Music Assistant's server is connected and hasn't started this
            // PC's sound: the panel's note says the same.
            _ if scene.situation.standing.contains(&Standing::MusicAssistantNotStarted) => Err(Taking::NotAsking { two_ways: false }),
            _ => Ok(Taking::Yes),
        };
        let broken = taking.is_err();
        c.taking = Some(taking);
        if broken {
            return c;
        }
        c.volume = level.map(|(level, muted)| volume_found(level, muted));
        c
    }

    fn discovery_check(&self) -> DiscoveryCheck {
        let app = &self.app;
        let st = app.discovery_net_status();
        let found = app.speaker_view().speakers.iter().filter(|s| s.id != MA_OUTPUT_ID).count();
        let mut lines = find_lines(&st.scope, &app.network_adapters(), &self.networks(), found);
        if app.airplay_discovery_error().is_some() {
            lines.push(FindLine::AirPlaySearchFailed);
        }
        if app.sendspin_discovery_error().is_some() {
            lines.push(FindLine::SendspinSearchFailed);
        }
        DiscoveryCheck { lines }
    }

    fn wanted_dialog(&self) -> Option<Wanted> {
        let app = &self.app;
        if let Some(mut ask) = app.pairing_code_ask() {
            ask.name = clean_name(&ask.name).0;
            return Some(Wanted::PairingCode(ask));
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
        let name = self.app.speaker_name(id).unwrap_or_else(|| id.into());
        let playing = self.playing_id().as_deref() == Some(id);
        Some(ToneView {
            name,
            eq: self.tone_of(id),
            headroom_db: playing.then(|| self.app.eq_report()).flatten().map(|r| r.auto_attenuation_db).filter(|d| *d < 0.0),
        })
    }

    fn report(&self, parts: ReportParts) -> String {
        let lang = self.lang();
        let mut out = Vec::new();
        if parts.versions {
            out.push(w!(lang, SetReportVersionLine, version = crate::display_version(), os = std::env::consts::OS));
        }
        if parts.diagnostics {
            out.push(self.app.diagnostics_report(parts.speakers, parts.versions));
        }
        if parts.speakers {
            for s in self.app.speaker_view_opts(true).speakers {
                out.push(format!("{} — {} ({})", s.friendly_name, s.ip, s.id));
            }
        }
        if parts.log && parts.speakers {
            if let Some(text) = crate::log_dir().and_then(|d| newest_log(&d)).and_then(|p| std::fs::read_to_string(p).ok()) {
                // The end of the log: what happened most recently.
                let lines: Vec<&str> = text.lines().collect();
                let from = lines.len().saturating_sub(400);
                out.push(lines[from..].join("\n"));
            }
        }
        out.join("\n\n")
    }

    fn save_report(&self, text: &str) -> ReportSave {
        self.save_report_in(crate::log_dir().as_deref(), text)
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
                }
                let eq = crate::equalizer::EqSettings { enabled: true, ..eq }.sanitized();
                self.change_speaker(&id, |s| s.tone = Some(eq.clone()));
                app.tone_follows_speaker();
            }
            Act::SongTitles(id, on) => {
                self.change_speaker(&id, |s| s.song_titles = Some(on));
            }
            Act::PairAgain(id) => {
                app.forget_airplay_pairing(&id);
                app.select_speaker_async(&id);
            }
            Act::ClearSpeakerCredentials(id) => {
                // The place command clears both transports' settings. Clear their
                // credentials too, before a manual speaker disappears from discovery.
                let ids = self.transport_ids(&id);
                let mut cfg = app.user_config.lock().unwrap();
                for key in ids {
                    cfg.remove_airplay_password(&key);
                    cfg.remove_airplay_pairing(&key);
                    if let Some(stored) = cfg.speaker_settings.get_mut(&key) { stored.tone = None; }
                    if let Some(instance) = key.strip_prefix("sendspin:") {
                        if let Some(client) = cfg.sendspin.pinned_client_id(instance) { cfg.sendspin.player_pairings.remove(&client); }
                    }
                }
                cfg.save();
                drop(cfg);
                app.tone_follows_speaker();
            }
            Act::VolumeLimit(id, limit) => {
                let ids = self.change_speaker(&id, |s| s.volume_limit = (limit < 100).then_some(limit));
                app.apply_bound_volume_limit(&ids);
            }
            Act::ShowAs(id, name) => {
                let name = name.trim();
                self.change_speaker(&id, |s| s.show_as = (!name.is_empty()).then(|| name.to_owned()));
            }
            Act::WhatPlays(mode) => {
                let mut cfg = app.user_config.lock().unwrap();
                cfg.only_some_apps = mode == WhatPlays::OnlySentApps;
                cfg.save();
            }
            Act::RecordShortcuts(recording) => {
                let cfg = app.user_config.lock().unwrap();
                if let Some(keys) = app.delay_shortcuts.lock().unwrap().upgrade() {
                    keys.lock().unwrap().record(recording, &cfg);
                }
            }
            Act::ShortcutKeys(keys) => {
                let mut cfg = app.user_config.lock().unwrap();
                cfg.delay_keys = keys;
                cfg.save();
            }
            Act::SwitchBackAfterQuiet(on) | Act::NotesByClock(on) | Act::ShowLab(on)
            | Act::PlayCalls(on) | Act::Shortcuts(on) => {
                let mut cfg = app.user_config.lock().unwrap();
                match act {
                    Act::SwitchBackAfterQuiet(_) => cfg.switch_back_after_quiet = on,
                    Act::NotesByClock(_) => cfg.notes_by_clock = on,
                    Act::PlayCalls(_) => cfg.play_calls = on,
                    Act::Shortcuts(_) => cfg.delay_shortcuts = on,
                    _ => cfg.show_lab = on,
                }
                cfg.save();
            }
            Act::Allow { address, speaker } => match address.parse::<IpAddr>() {
                // Only a device that asked for the sound and was refused
                // is allowed, from the page that lists it: its speaker's,
                // or Privacy for one that asked beside no speaker.
                Ok(ip) if self.was_refused(ip, &speaker) => app.allow_device(ip),
                Ok(_) => log::warn!("settings: {address} wasn't refused beside {speaker:?}: not allowed"),
                Err(_) => log::warn!("settings: can't allow {address}: not an address"),
            },
            Act::StartAtSignIn(on) => {
                let r = crate::autostart::set_enabled(on);
                self.inner.lock().unwrap().sign_in_problem = r.err();
            }
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
                if r.is_ok() {
                    let mut cfg = app.user_config.lock().unwrap();
                    cfg.phone_enabled = on;
                    cfg.save();
                }
                self.inner.lock().unwrap().phone_problem = r.err();
            }
            Act::Api(on) => {
                if let Err(e) = app.set_scripts_access(on, false) { app.record_error(e); }
            }
            Act::NewKey => {
                if let Err(e) = app.set_scripts_access(true, true) { app.record_error(e); }
            }
            Act::OpenApiHelp => self.desktop.open_url(&format!("{REPOSITORY}/blob/main/TECHNICAL.md#http-api")),
            Act::NewLink => {
                if let Err(e) = app.new_web_link() {
                    self.inner.lock().unwrap().phone_problem = Some(e);
                }
            }
            Act::OpenLogFolder => {
                if let Some(d) = crate::log_dir() {
                    if let Err(e) = self.desktop.open_folder(&d) { app.record_error(e.to_string()); }
                }
            }
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
            Act::Unmute => app.set_speaker_muted(false),
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
            Act::Lab(l) => self.lab_act(l),
            Act::SubmitCode { id, code } => {
                if !app.submit_pin(&id, Some(code)) {
                    app.record_error(w!(self.lang(), SetPairingEnded));
                }
            }
            Act::NewCode(id) => {
                if !app.ask_for_new_code(&id) {
                    app.record_error(w!(self.lang(), SetPairingEnded));
                }
            }
            Act::CancelPairing(id) => {
                let _ = app.submit_pin(&id, None);
            }
            Act::SubmitPassword { id, password, connect } => {
                let name = self.rows().into_iter().find(|(r, _)| r.id == id).map(|(r, _)| r.name).unwrap_or_else(|| id.clone());
                let dialog = app.password_dialog(&id, &name);
                let r = if connect { app.submit_password_dialog(&dialog, &password) } else { app.enter_airplay_password(&dialog, &password) };
                if let Err(e) = r {
                    app.record_error(e);
                }
            }
            Act::SubmitSendspin { id, code } => app.submit_sendspin_pairing(&id, Some(code)),
            Act::CancelSendspin(id) => app.submit_sendspin_pairing(&id, None),
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
            Act::OpenIssue(_) => self.desktop.open_url(&format!("{REPOSITORY}/issues/new")),
            // Controls the real app's views leave out.
            Act::Language(_)
            | Act::OpenMusicAssistant
            | Act::Install
            | Act::CancelInstall
            | Act::CancelPassword(_) => log::debug!("settings: {described} isn't offered by this version"),
        }
    }
}

/// The check's last step. Mute comes first: a muted speaker is silent at
/// any level, and the level is looked at again once it is unmuted.
pub(crate) fn volume_found(level: u8, muted: bool) -> Result<Volume, Volume> {
    if muted {
        Err(Volume::Muted)
    } else if level == 0 {
        Err(Volume::Zero)
    } else {
        Ok(Volume::Level(level.min(100)))
    }
}

impl AppSettings {
    pub(super) fn save_report_in(&self, dir: Option<&std::path::Path>, text: &str) -> ReportSave {
        let Some(dir) = dir else { return ReportSave::Failed };
        let path = dir.join(format!("report-{}.txt", now_unix()));
        if let Err(e) = std::fs::write(&path, text) {
            log::warn!("couldn't save the report: {e}");
            return ReportSave::Failed;
        }
        match self.desktop.open_folder(dir) {
            Ok(()) => ReportSave::Opened,
            Err(e) => {
                log::warn!("couldn't open the report folder: {e}");
                ReportSave::SavedOnly
            }
        }
    }

    fn set_hidden(&self, id: &str, hidden: bool) {
        self.change_speaker(id, |s| s.hidden = hidden);
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

/// What Windows says of the network an adapter is on. Windows writes the
/// adapter's id with or without braces, in either case.
fn network_on<'a>(networks: &'a [NetworkSeen], adapter: &NetAdapter) -> Option<&'a NetworkSeen> {
    let plain = |id: &str| id.trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase();
    let key = plain(&adapter.key);
    networks.iter().find(|n| n.adapter.as_deref().is_some_and(|id| plain(id) == key))
}

/// The name of the network an adapter is on, for the lines that say where
/// the app looks. A VPN goes by its adapter's name alone, and so does a
/// network Windows named after its adapter.
fn network_name(networks: &[NetworkSeen], adapter: &NetAdapter) -> Option<String> {
    let network = network_on(networks, adapter).filter(|n| n.kind != ConnKind::Other)?;
    network.name.clone().filter(|name| !name.eq_ignore_ascii_case(&adapter.name))
}

/// What Windows treats an adapter's network as, where it says, with the
/// name the pages give that network. A VPN's isn't a finding about the
/// network the speakers are on.
fn profile_of(networks: &[NetworkSeen], adapter: &NetAdapter) -> Option<(String, bool)> {
    let network = network_on(networks, adapter).filter(|n| n.kind != ConnKind::Other)?;
    Some((network.name.clone().unwrap_or_else(|| adapter.name.clone()), network.private?))
}

/// The lines of *Check step by step* about where the app looks: each
/// network, a VPN beside them, and what Windows treats each network as.
pub(super) fn find_lines(scope: &Resolved, adapters: &[NetAdapter], networks: &[NetworkSeen], found: usize) -> Vec<FindLine> {
    let mut lines = Vec::new();
    let place = |a: &NetAdapter, ip: Ipv4Addr| NetworkPlace { adapter: a.name.clone(), network: network_name(networks, a), address: subnet(&ip.to_string()) };
    let profile = |a: &NetAdapter| profile_of(networks, a).map(|(network, private)| FindLine::Profile { network, private });
    match scope {
        Resolved::Paused { name } => lines.push(FindLine::AdapterOff { adapter: name.clone() }),
        Resolved::Active { name, ip } => {
            let adapter = adapters.iter().find(|a| a.ipv4.contains(ip));
            let network = adapter.and_then(|a| network_name(networks, a));
            lines.push(FindLine::LookingOn { place: NetworkPlace { adapter: name.clone(), network, address: subnet(&ip.to_string()) }, found });
            lines.extend(adapter.and_then(profile));
        }
        Resolved::All => {
            let usable: Vec<_> = adapters.iter().filter_map(|a| a.usable_ipv4().map(|ip| (a, ip))).collect();
            let vpn = |n: &str| {
                let n = n.to_lowercase();
                ["vpn", "tap-", "wireguard", "tailscale", "zerotier", "openvpn", "anyconnect"].iter().any(|w| n.contains(w))
            };
            let home: Vec<_> = usable.iter().filter(|(a, _)| !vpn(&a.name)).collect();
            for (i, (a, ip)) in home.iter().enumerate() {
                lines.push(FindLine::LookingOn { place: place(a, *ip), found: if i == 0 { found } else { 0 } });
            }
            if let (Some((v, vip)), Some((h, _))) = (usable.iter().find(|(a, _)| vpn(&a.name)), home.first()) {
                lines.push(FindLine::Vpn {
                    vpn: NetworkPlace { adapter: v.name.clone(), network: None, address: subnet(&vip.to_string()) },
                    only: Adapter { key: h.key.clone(), name: h.name.clone() },
                });
            }
            lines.extend(home.iter().filter_map(|(a, _)| profile(a)));
        }
    }
    lines
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
