//! Diagnostics: a point-in-time snapshot of the counters the app keeps,
//! plus the copy of them taken just before the last fault (a dropped or
//! failed connection), so what led up to a failure survives the
//! reconnect that follows it. Rendered as plain text for the clipboard.

use std::fmt::Write as _;
use std::time::Duration;

/// How long without a packet from Windows before the audio is reported
/// as no longer arriving.
const SOURCE_STALE_AFTER: Duration = Duration::from_secs(1);

/// How long after the last packet that carried sound the audio still
/// counts as playing, so a short pause in the sound doesn't flip it.
const SOUND_HOLD: Duration = Duration::from_millis(500);

/// What the audio loop is getting from Windows. The driver's start/stop
/// events only say whether a stream is running on our device (and the
/// app starts out assuming one is), not whether anything is audible, so
/// this also looks at the packets themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowsAudio {
    /// Windows reported its stream stopped.
    Stopped,
    /// Waiting on Windows, and no packet has come since launch.
    #[default]
    NothingYet,
    /// Waiting on Windows, and the last packet is over a second old.
    NothingRecently,
    /// Packets are arriving, but none carried sound in the last half
    /// second.
    Silent,
    /// Packets are arriving and carried sound in the last half second.
    Playing,
}

impl WindowsAudio {
    /// `reading_source`: the audio loop takes packets from Windows rather
    /// than generating silence. `since_last_packet` / `since_last_sound`:
    /// age of the newest packet from Windows / of the newest one that
    /// carried sound, `None` if none came yet.
    pub fn classify(
        reading_source: bool,
        since_last_packet: Option<Duration>,
        since_last_sound: Option<Duration>,
    ) -> Self {
        if !reading_source {
            return WindowsAudio::Stopped;
        }
        match since_last_packet {
            None => WindowsAudio::NothingYet,
            Some(age) if age > SOURCE_STALE_AFTER => WindowsAudio::NothingRecently,
            Some(_) if since_last_sound.is_some_and(|age| age <= SOUND_HOLD) => WindowsAudio::Playing,
            Some(_) => WindowsAudio::Silent,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            WindowsAudio::Stopped => "stream stopped by Windows",
            WindowsAudio::NothingYet => "nothing received yet",
            WindowsAudio::NothingRecently => "nothing received for over a second",
            WindowsAudio::Silent => "receiving silence",
            WindowsAudio::Playing => "playing",
        }
    }
}

/// Counters and state at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Unix seconds when taken.
    pub taken_unix: u64,
    pub app_uptime_secs: u64,
    /// Bound speaker: name, id, address, transport label.
    pub speaker: Option<String>,
    pub speaker_id: Option<String>,
    pub speaker_addr: Option<String>,
    pub transport: Option<&'static str>,
    /// How long the current session has been up.
    pub session_secs: Option<u64>,
    /// Automatic reconnects since the user last picked a speaker.
    pub reconnects: u32,
    pub streaming_enabled: bool,
    pub windows_audio: WindowsAudio,
    pub connecting_to: Option<String>,
    /// 10 ms packets published to the senders / stream clients.
    pub packets_published: u64,
    /// Of those, silence generated while Windows was idle.
    pub idle_silence_packets: u64,
    /// Packets a slow consumer's queue had no room for (dropped for it).
    pub dropped_packets: u64,
    /// Consumers of the audio hub (AirPlay sender, HTTP stream clients).
    pub hub_consumers: usize,
    /// AirPlay retransmission: (requests received, packets re-sent).
    pub resend: Option<(u64, u64)>,
    /// Latency nudge still being applied, ms (+ trim / − pad).
    pub pending_latency_ms: i32,
    /// AirPlay buffer setting in effect for the bound speaker, ms.
    pub airplay_buffer_ms: Option<u32>,
    pub stream_format: &'static str,
    /// Discovery scope in words and how many speakers are listed.
    pub discovery: String,
    pub speakers_listed: usize,
}

/// A fault and the snapshot taken just before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub when_unix: u64,
    pub what: String,
    pub before: Snapshot,
}

impl Snapshot {
    /// `(label, value)` rows, shared by the UI and the text export.
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".to_string());
        let mut rows = vec![
            ("Speaker", opt(&self.speaker)),
            ("Speaker id", opt(&self.speaker_id)),
            ("Address", opt(&self.speaker_addr)),
            ("Transport", self.transport.unwrap_or("—").to_string()),
            (
                "Session uptime",
                self.session_secs.map(format_secs).unwrap_or_else(|| "—".to_string()),
            ),
            ("Reconnects (this playback)", self.reconnects.to_string()),
            ("Streaming enabled", yes_no(self.streaming_enabled)),
            ("Windows audio", self.windows_audio.describe().to_string()),
            ("Connecting to", opt(&self.connecting_to)),
            ("Packets sent", self.packets_published.to_string()),
            ("  of which idle silence", self.idle_silence_packets.to_string()),
            ("Packets dropped (slow consumer)", self.dropped_packets.to_string()),
            ("Audio consumers", self.hub_consumers.to_string()),
        ];
        rows.push((
            "AirPlay resend requests / packets",
            match self.resend {
                Some((req, pkts)) => format!("{} / {}", req, pkts),
                None => "—".to_string(),
            },
        ));
        rows.push(("Pending latency adjust", format!("{} ms", self.pending_latency_ms)));
        rows.push((
            "AirPlay buffer",
            self.airplay_buffer_ms
                .map(|ms| format!("{} ms", ms))
                .unwrap_or_else(|| "—".to_string()),
        ));
        rows.push(("Stream format", self.stream_format.to_string()));
        rows.push(("Discovery", self.discovery.clone()));
        rows.push(("Speakers listed", self.speakers_listed.to_string()));
        rows.push(("End-to-end latency", "not measured".to_string()));
        rows.push(("App uptime", format_secs(self.app_uptime_secs)));
        rows
    }
}

/// The clipboard text: header, current snapshot, last error, last fault
/// with its before-snapshot.
pub fn report(
    product: &str,
    version: &str,
    now: &Snapshot,
    last_error: Option<&str>,
    fault: Option<&Fault>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{} v{} diagnostics", product, version);
    let _ = writeln!(s, "Taken at unix {}", now.taken_unix);
    let _ = writeln!(s);
    let _ = writeln!(s, "[now]");
    write_rows(&mut s, now);
    if let Some(e) = last_error {
        let _ = writeln!(s, "Last message: {}", e);
    }
    let _ = writeln!(s);
    match fault {
        Some(f) => {
            let _ = writeln!(
                s,
                "[last fault, {} s ago] {}",
                now.taken_unix.saturating_sub(f.when_unix),
                f.what
            );
            let _ = writeln!(s, "[statistics just before it]");
            write_rows(&mut s, &f.before);
        }
        None => {
            let _ = writeln!(s, "[last fault] none this run");
        }
    }
    s
}

fn write_rows(s: &mut String, snap: &Snapshot) {
    for (k, v) in snap.rows() {
        let _ = writeln!(s, "{}: {}", k, v);
    }
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}

/// `1h 02m 03s` / `2m 03s` / `3s`.
pub fn format_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}h {:02}m {:02}s", h, m, s)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_includes_fault_and_its_snapshot() {
        let now = Snapshot {
            taken_unix: 1_000,
            speaker: Some("Kitchen".into()),
            transport: Some("AirPlay 2"),
            reconnects: 1,
            resend: Some((3, 40)),
            stream_format: "L16",
            ..Default::default()
        };
        let before = Snapshot { packets_published: 777, ..now.clone() };
        let fault = Fault { when_unix: 990, what: "Lost connection to Kitchen".into(), before };
        let text = report("App", "1.2.3", &now, Some("oops"), Some(&fault));
        assert!(text.starts_with("App v1.2.3 diagnostics"));
        assert!(text.contains("Speaker: Kitchen"));
        assert!(text.contains("AirPlay resend requests / packets: 3 / 40"));
        assert!(text.contains("Last message: oops"));
        assert!(text.contains("[last fault, 10 s ago] Lost connection to Kitchen"));
        assert!(text.contains("Packets sent: 777"));
        assert!(text.contains("End-to-end latency: not measured"));
    }

    #[test]
    fn report_without_fault() {
        let text = report("App", "1", &Snapshot::default(), None, None);
        assert!(text.contains("[last fault] none this run"));
        assert!(text.contains("Speaker: —"));
    }

    #[test]
    fn windows_audio_says_only_what_is_known() {
        let fresh = Some(Duration::from_millis(30));
        let long_ago = Some(Duration::from_secs(3));
        // A stop from Windows wins over anything the packets said.
        assert_eq!(WindowsAudio::classify(false, fresh, fresh), WindowsAudio::Stopped);
        // Reading from Windows but nothing came: the state the app starts in.
        assert_eq!(WindowsAudio::classify(true, None, None), WindowsAudio::NothingYet);
        assert_eq!(WindowsAudio::classify(true, long_ago, long_ago), WindowsAudio::NothingRecently);
        // Packets, but none ever carried sound (a silent stream at launch).
        assert_eq!(WindowsAudio::classify(true, fresh, None), WindowsAudio::Silent);
        assert_eq!(WindowsAudio::classify(true, fresh, long_ago), WindowsAudio::Silent);
        assert_eq!(WindowsAudio::classify(true, fresh, fresh), WindowsAudio::Playing);
        // A short pause in the sound is still playing.
        assert_eq!(
            WindowsAudio::classify(true, fresh, Some(Duration::from_millis(400))),
            WindowsAudio::Playing
        );
    }

    #[test]
    fn only_audible_packets_read_as_playing() {
        let row = |w: WindowsAudio| {
            let snap = Snapshot { windows_audio: w, ..Default::default() };
            snap.rows().into_iter().find(|(k, _)| *k == "Windows audio").unwrap().1
        };
        assert_eq!(row(WindowsAudio::default()), "nothing received yet");
        for w in [
            WindowsAudio::Stopped,
            WindowsAudio::NothingYet,
            WindowsAudio::NothingRecently,
            WindowsAudio::Silent,
        ] {
            assert_ne!(row(w), "playing", "{:?}", w);
        }
        assert_eq!(row(WindowsAudio::Playing), "playing");
    }

    #[test]
    fn secs_format() {
        assert_eq!(format_secs(5), "5s");
        assert_eq!(format_secs(125), "2m 05s");
        assert_eq!(format_secs(3723), "1h 02m 03s");
    }
}
