//! The speaker's own buttons, heard over DACP (the remote-control channel
//! of AirPlay 1): a receiver whose play or pause button is pressed sends
//! `GET /ctrl-int/1/<command>` to the sender that announced
//! `iTunes_Ctrl_<DACP-ID>._dacp._tcp`, with the `Active-Remote` the sender
//! gave it in its requests (which the RTSP client sends already).
//!
//! Off: [`LISTEN_FOR_SPEAKER_BUTTONS`]. Until AirPlay speakers have been
//! tried with it, the app assumes nothing reaches it from their buttons:
//! nothing is announced and nothing listens.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use log::{debug, info};

use crate::engine::signals::SpeakerSignal;

/// Listen for the receiver's buttons. Off until AirPlay speakers have been
/// tried with it.
pub const LISTEN_FOR_SPEAKER_BUTTONS: bool = false;

/// The mDNS service a receiver looks for.
pub const DACP_SERVICE: &str = "_dacp._tcp.local.";

/// The longest request head read.
const MAX_HEAD: usize = 8 * 1024;

/// What a DACP request says, for the sender whose `Active-Remote` is
/// `active_remote`: None for another sender's, or a command that says
/// nothing of playing.
pub fn command_of(head: &str, active_remote: &str) -> Option<SpeakerSignal> {
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    if first.next()? != "GET" {
        return None;
    }
    let path = first.next()?;
    let theirs = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("active-remote"))
        .map(|(_, v)| v.trim())?;
    if theirs != active_remote {
        return None;
    }
    let command = path.strip_prefix("/ctrl-int/1/")?.split('?').next()?;
    match command {
        "pause" => Some(SpeakerSignal::PausedAtSpeaker),
        // Both play commands decode as a press of the play button.
        "playpause" | "play" => Some(SpeakerSignal::PlayPressed),
        "stop" => Some(SpeakerSignal::StoppedAtSpeaker),
        _ => None,
    }
}

/// A DACP listener for one session: answers every request `204`, and
/// tells `heard` what the receiver's buttons said.
pub struct Listener {
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Listen on `bind`, on a port of the system's choosing.
    pub fn start(
        bind: std::net::IpAddr,
        active_remote: String,
        heard: Arc<dyn Fn(SpeakerSignal) + Send + Sync>,
    ) -> std::io::Result<Self> {
        let socket = TcpListener::bind((bind, 0))?;
        socket.set_nonblocking(true)?;
        let port = socket.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new().name("stream-to-speaker-dacp".into()).spawn(move || {
            while !flag.load(Ordering::Acquire) {
                match socket.accept() {
                    Ok((conn, _)) => serve(conn, &active_remote, heard.as_ref()),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                    Err(e) => {
                        debug!("DACP: accept failed: {}", e);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        })?;
        info!("DACP: listening for the speaker's buttons on port {}", port);
        Ok(Listener { port, stop, thread: Some(thread) })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Answer one connection's requests until the receiver closes it.
fn serve(mut conn: TcpStream, active_remote: &str, heard: &(dyn Fn(SpeakerSignal) + Send + Sync)) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            buf.drain(..end + 4);
            if let Some(signal) = command_of(&head, active_remote) {
                heard(signal);
            }
            if conn.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").is_err() {
                return;
            }
        }
        if buf.len() > MAX_HEAD {
            return;
        }
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Announce the listener as `iTunes_Ctrl_<dacp_id>` on `daemon`, so the
/// receiver finds it; the announcement ends when the daemon unregisters
/// it (see [`unannounce`]).
pub fn announce(daemon: &mdns_sd::ServiceDaemon, dacp_id: &str, host: &str, port: u16) -> Result<String, String> {
    let instance = format!("iTunes_Ctrl_{dacp_id}");
    let info = mdns_sd::ServiceInfo::new(DACP_SERVICE, &instance, host, "", port, &[("txtvers", "1"), ("Ver", "131075")][..])
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| e.to_string())?;
    Ok(fullname)
}

/// End the announcement made by [`announce`].
pub fn unannounce(daemon: &mdns_sd::ServiceDaemon, fullname: &str) {
    let _ = daemon.unregister(fullname);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_receivers_buttons_read_as_pause_play_and_stop() {
        let req = |path: &str, remote: &str| format!("GET {path} HTTP/1.1\r\nHost: x\r\nActive-Remote: {remote}\r\n");
        assert_eq!(command_of(&req("/ctrl-int/1/pause", "42"), "42"), Some(SpeakerSignal::PausedAtSpeaker));
        assert_eq!(command_of(&req("/ctrl-int/1/playpause", "42"), "42"), Some(SpeakerSignal::PlayPressed));
        assert_eq!(command_of(&req("/ctrl-int/1/play", "42"), "42"), Some(SpeakerSignal::PlayPressed));
        assert_eq!(command_of(&req("/ctrl-int/1/stop", "42"), "42"), Some(SpeakerSignal::StoppedAtSpeaker));
        // Volume and track keys say nothing of playing.
        assert_eq!(command_of(&req("/ctrl-int/1/volumeup", "42"), "42"), None);
        assert_eq!(command_of(&req("/ctrl-int/1/nextitem", "42"), "42"), None);
        // Another sender's remote, or none.
        assert_eq!(command_of(&req("/ctrl-int/1/pause", "7"), "42"), None);
        assert_eq!(command_of("GET /ctrl-int/1/pause HTTP/1.1\r\nHost: x\r\n", "42"), None);
        assert_eq!(command_of("POST /ctrl-int/1/pause HTTP/1.1\r\nActive-Remote: 42\r\n", "42"), None);
    }

    #[test]
    fn the_listener_answers_and_tells_what_it_heard() {
        let heard: Arc<Mutex<Vec<SpeakerSignal>>> = Arc::new(Mutex::new(Vec::new()));
        let into = heard.clone();
        let l = Listener::start("127.0.0.1".parse().unwrap(), "42".into(), Arc::new(move |s| into.lock().unwrap().push(s))).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", l.port)).unwrap();
        c.write_all(b"GET /ctrl-int/1/pause HTTP/1.1\r\nActive-Remote: 42\r\n\r\nGET /ctrl-int/1/play HTTP/1.1\r\nActive-Remote: 42\r\n\r\n")
            .unwrap();
        let mut answer = [0u8; 256];
        let mut got = Vec::new();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        while got.windows(3).filter(|w| *w == b"204").count() < 2 {
            let n = c.read(&mut answer).unwrap();
            assert!(n > 0, "closed early");
            got.extend_from_slice(&answer[..n]);
        }
        assert_eq!(*heard.lock().unwrap(), [SpeakerSignal::PausedAtSpeaker, SpeakerSignal::PlayPressed]);
        drop(c);
        drop(l);
    }
}
