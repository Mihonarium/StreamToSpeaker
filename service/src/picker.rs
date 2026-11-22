//! The speaker list in a terminal: what `--list-speakers` prints, and the
//! prompt a `--headless` run in a terminal asks with.
//!
//! Both show what the window lists: every kind of speaker found on the
//! network, and the ones added by address. Without a terminal nobody is
//! asked, and nothing is picked for them. Names come from devices on the
//! network, so anything in them a terminal would act on is replaced
//! before printing.

use crate::app::App;
use crate::http_server::SpeakerInfo;
use crate::ssdp::{DiscoveryState, Renderer};
use anyhow::Result;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a search may take before the list is shown anyway.
pub const SEARCH_WAIT: Duration = Duration::from_secs(15);

/// How long the prompt waits for an answer before taking the speaker used
/// last time, when that one is listed: an unattended start in a terminal
/// mustn't wait for ever.
pub const SAVED_SPEAKER_WAIT: Duration = Duration::from_secs(15);

/// `text` from a device, for a terminal: every control character (which a
/// terminal could take as a command: retitle the window, move the cursor,
/// recolour) replaced with `?`.
pub fn printable(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// [`printable`], keeping line breaks and tabs: for log lines.
pub fn printable_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c })
        .collect()
}

/// Returns true when stdin and stdout are both attached to a terminal.
/// Used to gate the interactive prompt.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Search the network now, as the window's Rescan does, and wait until
/// that search is done (or `max` has passed). AirPlay and Sendspin
/// speakers answer meanwhile.
pub fn search_and_wait(app: &Arc<App>, max: Duration) {
    app.trigger_rescan();
    let end = Instant::now() + max;
    while app.rescan_in_flight.load(Ordering::Acquire) && Instant::now() < end {
        thread::sleep(Duration::from_millis(100));
    }
}

/// The kind of speaker a row is, for rows whose name doesn't say it.
fn kind(speaker: &SpeakerInfo) -> Option<&'static str> {
    if speaker.manual || speaker.id == crate::sendspin_app::MA_OUTPUT_ID {
        // The name says it: "Den (AirPlay, added manually)", "Music Assistant".
        None
    } else if crate::sendspin_app::is_sendspin_id(&speaker.id) {
        Some("Sendspin")
    } else if speaker.id.starts_with("airplay:") {
        Some("AirPlay")
    } else {
        Some("UPnP")
    }
}

/// One numbered line per speaker: its name, then its address and kind,
/// and whether it is `last_used`.
pub fn speaker_lines(speakers: &[SpeakerInfo], last_used: Option<&str>) -> Vec<String> {
    speakers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let ip = printable(&s.ip);
            let mut about: Vec<&str> = Vec::new();
            if !ip.is_empty() {
                about.push(&ip);
            }
            about.extend(kind(s));
            if s.hidden {
                about.push("hidden");
            }
            if last_used == Some(s.id.as_str()) {
                about.push("used last time");
            }
            let name = printable(&s.friendly_name);
            if about.is_empty() {
                format!("  [{}]  {}", i + 1, name)
            } else {
                format!("  [{}]  {}  ({})", i + 1, name, about.join(", "))
            }
        })
        .collect()
}

/// Print the list to `out`. Errors (a closed pipe) are ignored: there is
/// nobody left to tell.
pub fn write_speaker_list(out: &mut dyn Write, speakers: &[SpeakerInfo], last_used: Option<&str>) {
    for line in speaker_lines(speakers, last_used) {
        let _ = writeln!(out, "{}", line);
    }
}

/// Block (with a deadline) for at least one UPnP renderer to appear, then
/// return the current list.
pub fn wait_for_first_discovery(state: &Arc<DiscoveryState>, max_wait: Duration) -> Vec<Renderer> {
    let start = Instant::now();
    while state.renderers().is_empty() && start.elapsed() < max_wait {
        thread::sleep(Duration::from_millis(200));
    }
    state.renderers()
}

/// What the prompt reads next.
pub(crate) enum Answer {
    Line(String),
    /// Nothing within the time asked for.
    NoneInTime,
    /// The input ended.
    End,
}

/// Where the prompt's answers come from.
pub(crate) trait Answers {
    /// The next line typed, waiting at most `limit` (`None`: as long as it
    /// takes).
    fn next_line(&mut self, limit: Option<Duration>) -> io::Result<Answer>;
}

/// Lines from a reader, read on the caller's thread: no time limit (asking
/// for one waits anyway).
struct Blocking<R: BufRead>(R);

impl<R: BufRead> Answers for Blocking<R> {
    fn next_line(&mut self, _limit: Option<Duration>) -> io::Result<Answer> {
        let mut line = String::new();
        Ok(match self.0.read_line(&mut line)? {
            0 => Answer::End,
            _ => Answer::Line(line),
        })
    }
}

/// Lines a thread reads from the terminal, so a wait can have a time limit.
/// That thread ends at the end of input, or with the next line once nobody
/// waits any more; nothing else in the app reads the terminal, and it
/// doesn't keep the process from exiting.
pub(crate) struct Threaded(crossbeam_channel::Receiver<io::Result<String>>);

impl Threaded {
    fn from_stdin() -> io::Result<Self> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        thread::Builder::new().name("stream-to-speaker-prompt".into()).spawn(move || loop {
            let mut line = String::new();
            let got = io::stdin().read_line(&mut line).map(|n| if n == 0 { None } else { Some(line) });
            let end = !matches!(got, Ok(Some(_)));
            if tx.send(got.map(|l| l.unwrap_or_default())).is_err() || end {
                return;
            }
        })?;
        Ok(Self(rx))
    }

    #[cfg(test)]
    fn from(rx: crossbeam_channel::Receiver<io::Result<String>>) -> Self {
        Self(rx)
    }
}

impl Answers for Threaded {
    fn next_line(&mut self, limit: Option<Duration>) -> io::Result<Answer> {
        let got = match limit {
            Some(limit) => match self.0.recv_timeout(limit) {
                Ok(got) => got,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Ok(Answer::NoneInTime),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(Answer::End),
            },
            None => match self.0.recv() {
                Ok(got) => got,
                Err(_) => return Ok(Answer::End),
            },
        };
        Ok(match got? {
            line if line.is_empty() => Answer::End,
            line => Answer::Line(line),
        })
    }
}

/// Ask on this terminal which speaker to use: its id, or None if the
/// person skipped. `last_used`: the speaker used last time; when it is
/// listed it comes first, Enter takes it, and it is taken if nobody answers
/// within [`SAVED_SPEAKER_WAIT`].
pub fn interactive_pick(app: &Arc<App>, last_used: Option<&str>) -> Result<Option<String>> {
    let mut output = io::stdout();
    let list = || saved_first(app.speaker_view().speakers, last_used);
    let search = || search_and_wait(app, SEARCH_WAIT);
    let picked = match last_used {
        Some(_) => {
            let mut answers = Threaded::from_stdin()?;
            pick_from(&mut answers, &mut output, list, search, last_used, SAVED_SPEAKER_WAIT)?
        }
        None => {
            let stdin = io::stdin();
            let mut answers = Blocking(stdin.lock());
            pick_from(&mut answers, &mut output, list, search, None, SAVED_SPEAKER_WAIT)?
        }
    };
    Ok(picked)
}

/// `speakers` with `last_used`, if listed, moved to the front.
fn saved_first(mut speakers: Vec<SpeakerInfo>, last_used: Option<&str>) -> Vec<SpeakerInfo> {
    if let Some(at) = speakers.iter().position(|s| Some(s.id.as_str()) == last_used) {
        let saved = speakers.remove(at);
        speakers.insert(0, saved);
    }
    speakers
}

/// The prompt itself: searches first, lists, and reads a choice from
/// `answers`. Accepts a number, Enter for the first speaker, "r" to search
/// again, "q" (or the end of input) to skip. While `last_used` is the first
/// speaker listed and nobody has answered yet, it is taken after `wait`.
fn pick_from(
    answers: &mut dyn Answers,
    out: &mut dyn Write,
    mut list: impl FnMut() -> Vec<SpeakerInfo>,
    mut search: impl FnMut(),
    last_used: Option<&str>,
    wait: Duration,
) -> io::Result<Option<String>> {
    let _ = writeln!(out, "Searching for speakers…");
    let _ = out.flush();
    search();
    // Someone answered: from then on, nothing is taken for them.
    let mut answered = false;
    loop {
        let speakers = list();
        let n = speakers.len();
        let saved_first = speakers.first().filter(|s| Some(s.id.as_str()) == last_used);
        let limit = saved_first.filter(|_| !answered).map(|_| wait);
        if n == 0 {
            let _ = write!(out, "No speakers found yet. [r] search again, [q] skip: ");
        } else {
            let _ = writeln!(out);
            write_speaker_list(out, &speakers, last_used);
            let _ = writeln!(out);
            match (saved_first, limit) {
                (Some(saved), Some(wait)) => {
                    let _ = writeln!(out, "Pick a speaker [1-{}] (Enter = 1, r = search again, q = skip).", n);
                    let _ = write!(
                        out,
                        "{}, used last time, is taken if nobody answers within {} seconds: ",
                        printable(&saved.friendly_name),
                        wait.as_secs()
                    );
                }
                _ => {
                    let _ = write!(out, "Pick a speaker [1-{}] (Enter = the first, r = search again, q = skip): ", n);
                }
            }
        }
        let _ = out.flush();
        let line = match answers.next_line(limit)? {
            Answer::Line(line) => line,
            Answer::End => return Ok(None),
            Answer::NoneInTime => {
                let saved = saved_first.map(|s| s.id.clone());
                if let Some(s) = speakers.first() {
                    let _ = writeln!(out);
                    let _ = writeln!(out, "No answer: using {}.", printable(&s.friendly_name));
                }
                return Ok(saved);
            }
        };
        answered = true;
        let answer = line.trim();
        match answer.to_lowercase().as_str() {
            "" if n > 0 => return Ok(speakers.into_iter().next().map(|s| s.id)),
            "" | "q" | "quit" => return Ok(None),
            "r" | "refresh" => {
                let _ = writeln!(out, "Searching for speakers…");
                let _ = out.flush();
                search();
            }
            other => match other.parse::<usize>() {
                Ok(i) if (1..=n).contains(&i) => return Ok(speakers.into_iter().nth(i - 1).map(|s| s.id)),
                _ => {
                    let _ = writeln!(out, "\"{}\" isn't one of the choices.", printable(answer));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn speaker(id: &str, name: &str, ip: &str) -> SpeakerInfo {
        SpeakerInfo {
            id: id.into(),
            friendly_name: name.into(),
            ip: ip.into(),
            active: false,
            note: None,
            manual: false,
            hidden: false,
        }
    }

    fn two() -> Vec<SpeakerInfo> {
        vec![
            speaker("airplay:AABBCCDDEEFF", "Kitchen", "192.168.1.30"),
            speaker("uuid:RINCON_A", "Living Room", "192.168.1.31"),
        ]
    }

    /// The prompt over `typed`, with a list that a second search changes
    /// from `before` to `after`: what it returned, how often it searched,
    /// and what it printed.
    fn pick(typed: &str, before: Vec<SpeakerInfo>, after: Vec<SpeakerInfo>) -> (Option<String>, usize, String) {
        let searches = Cell::new(0);
        let shown = RefCell::new(before);
        let mut out = Vec::new();
        let picked = pick_from(
            &mut Blocking(typed.as_bytes()),
            &mut out,
            || shown.borrow().clone(),
            || {
                searches.set(searches.get() + 1);
                if searches.get() > 1 {
                    *shown.borrow_mut() = after.clone();
                }
            },
            None,
            SAVED_SPEAKER_WAIT,
        )
        .unwrap();
        (picked, searches.get(), String::from_utf8(out).unwrap())
    }

    /// Answers given in turn, recording the time limit each was asked with.
    struct Scripted {
        answers: Vec<Answer>,
        limits: Vec<Option<Duration>>,
    }

    impl Answers for Scripted {
        fn next_line(&mut self, limit: Option<Duration>) -> io::Result<Answer> {
            self.limits.push(limit);
            Ok(if self.answers.is_empty() { Answer::End } else { self.answers.remove(0) })
        }
    }

    /// The prompt with the speaker used last time listed first (as
    /// `interactive_pick` lists it): what it returned, the limits it asked
    /// with, and what it printed.
    fn pick_with_saved(answers: Vec<Answer>, saved: Option<&str>) -> (Option<String>, Vec<Option<Duration>>, String) {
        let mut rows = two();
        rows.reverse(); // the saved one, Living Room, first
        let mut scripted = Scripted { answers, limits: Vec::new() };
        let mut out = Vec::new();
        let picked =
            pick_from(&mut scripted, &mut out, || rows.clone(), || {}, saved, Duration::from_secs(15)).unwrap();
        (picked, scripted.limits, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_speaker_used_last_time_is_listed_first() {
        let ids = |v: Vec<SpeakerInfo>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(saved_first(two(), Some("uuid:RINCON_A"))), ["uuid:RINCON_A", "airplay:AABBCCDDEEFF"]);
        assert_eq!(ids(saved_first(two(), Some("uuid:GONE"))), ["airplay:AABBCCDDEEFF", "uuid:RINCON_A"]);
        assert_eq!(ids(saved_first(two(), None)), ["airplay:AABBCCDDEEFF", "uuid:RINCON_A"]);
    }

    #[test]
    fn the_speaker_used_last_time_comes_first_and_enter_takes_it() {
        let (picked, _, printed) = pick_with_saved(vec![Answer::Line("\n".into())], Some("uuid:RINCON_A"));
        assert_eq!(picked.as_deref(), Some("uuid:RINCON_A"));
        assert!(printed.contains("  [1]  Living Room  (192.168.1.31, UPnP, used last time)"), "{printed}");
    }

    #[test]
    fn nobody_answering_takes_the_speaker_used_last_time_after_the_wait() {
        let (picked, limits, printed) = pick_with_saved(vec![Answer::NoneInTime], Some("uuid:RINCON_A"));
        assert_eq!(picked.as_deref(), Some("uuid:RINCON_A"));
        assert_eq!(limits, [Some(Duration::from_secs(15))]);
        assert!(printed.contains("Living Room, used last time, is taken if nobody answers within 15 seconds"), "{printed}");
        assert!(printed.contains("No answer: using Living Room."), "{printed}");
    }

    #[test]
    fn once_someone_answers_or_with_nothing_saved_the_prompt_waits() {
        // Someone is there: after any answer, no time limit.
        let (picked, limits, _) =
            pick_with_saved(vec![Answer::Line("r\n".into()), Answer::Line("2\n".into())], Some("uuid:RINCON_A"));
        assert_eq!(picked.as_deref(), Some("airplay:AABBCCDDEEFF"));
        assert_eq!(limits, [Some(Duration::from_secs(15)), None]);
        // Nothing saved, or the saved speaker isn't listed: no limit at all.
        assert_eq!(pick_with_saved(vec![Answer::Line("1\n".into())], None).1, [None]);
        assert_eq!(pick_with_saved(vec![Answer::Line("1\n".into())], Some("uuid:GONE")).1, [None]);
    }

    #[test]
    fn a_wait_with_a_time_limit_ends_on_time_and_takes_an_earlier_answer() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut answers = Threaded::from(rx);
        let started = Instant::now();
        assert!(matches!(answers.next_line(Some(Duration::from_millis(30))).unwrap(), Answer::NoneInTime));
        assert!(started.elapsed() >= Duration::from_millis(30));
        tx.send(Ok("2\n".to_string())).unwrap();
        assert!(matches!(answers.next_line(Some(Duration::from_secs(30))).unwrap(), Answer::Line(l) if l == "2\n"));
        drop(tx);
        assert!(matches!(answers.next_line(None).unwrap(), Answer::End));
    }

    #[test]
    fn a_device_cant_send_the_terminal_commands_through_its_name() {
        let mut rows = two();
        rows[0].friendly_name = "Kitchen\u{1b}]0;owned\u{7}\u{1b}[2J\u{9b}31m".into();
        rows[0].ip = "192.168.1.30\u{1b}[A".into();
        let lines = speaker_lines(&rows, None);
        assert!(!lines[0].chars().any(|c| c.is_control()), "{:?}", lines[0]);
        assert!(lines[0].starts_with("  [1]  Kitchen?]0;owned??[2J?31m  (192.168.1.30?[A, AirPlay)"), "{:?}", lines[0]);
        assert_eq!(printable_text("line one\n\tline two\u{1b}[0m"), "line one\n\tline two?[0m");
    }

    #[test]
    fn a_number_picks_that_speaker() {
        let (picked, searches, _) = pick("2\n", two(), two());
        assert_eq!(picked.as_deref(), Some("uuid:RINCON_A"));
        assert_eq!(searches, 1, "searches once before the first list");
    }

    #[test]
    fn r_searches_again_and_shows_what_it_found() {
        let (picked, searches, printed) = pick("r\n1\n", Vec::new(), two());
        assert_eq!(searches, 2);
        assert_eq!(picked.as_deref(), Some("airplay:AABBCCDDEEFF"));
        assert!(printed.contains("No speakers found yet"), "{printed}");
        assert!(printed.contains("Kitchen"), "{printed}");
    }

    #[test]
    fn enter_takes_the_first_q_or_the_end_of_input_skips() {
        assert_eq!(pick("\n", two(), two()).0.as_deref(), Some("airplay:AABBCCDDEEFF"));
        assert_eq!(pick("q\n", two(), two()).0, None);
        assert_eq!(pick("", two(), two()).0, None);
        // With nothing listed, Enter skips too.
        assert_eq!(pick("\n", Vec::new(), Vec::new()).0, None);
    }

    #[test]
    fn a_wrong_answer_asks_again() {
        let (picked, _, printed) = pick("9\nkitchen\n1\n", two(), two());
        assert_eq!(picked.as_deref(), Some("airplay:AABBCCDDEEFF"));
        assert!(printed.contains("\"9\" isn't one of the choices."), "{printed}");
        assert!(printed.contains("\"kitchen\" isn't one of the choices."), "{printed}");
    }

    #[test]
    fn the_list_says_where_and_what_each_speaker_is() {
        let mut rows = two();
        rows.push(speaker("music-assistant:input", "Music Assistant", ""));
        let mut manual =
            speaker("manual:airplay:192.168.1.40:7000", "Den (AirPlay, added manually)", "192.168.1.40:7000");
        manual.manual = true;
        manual.hidden = true;
        rows.push(manual);
        assert_eq!(
            speaker_lines(&rows, None),
            [
                "  [1]  Kitchen  (192.168.1.30, AirPlay)",
                "  [2]  Living Room  (192.168.1.31, UPnP)",
                "  [3]  Music Assistant",
                "  [4]  Den (AirPlay, added manually)  (192.168.1.40:7000, hidden)",
            ]
        );
    }
}
