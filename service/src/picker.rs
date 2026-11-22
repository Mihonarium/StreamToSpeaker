//! The speaker list in a terminal: what `--list-speakers` prints, and the
//! prompt a `--headless` run in a terminal asks with.
//!
//! Both show what the window lists: every kind of speaker found on the
//! network, and the ones added by address. Without a terminal nobody is
//! asked, and nothing is picked for them.

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

/// One numbered line per speaker: its name, then its address and kind.
pub fn speaker_lines(speakers: &[SpeakerInfo]) -> Vec<String> {
    speakers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut about: Vec<&str> = Vec::new();
            if !s.ip.is_empty() {
                about.push(&s.ip);
            }
            about.extend(kind(s));
            if s.hidden {
                about.push("hidden");
            }
            if about.is_empty() {
                format!("  [{}]  {}", i + 1, s.friendly_name)
            } else {
                format!("  [{}]  {}  ({})", i + 1, s.friendly_name, about.join(", "))
            }
        })
        .collect()
}

/// Print the list to `out`. Errors (a closed pipe) are ignored: there is
/// nobody left to tell.
pub fn write_speaker_list(out: &mut dyn Write, speakers: &[SpeakerInfo]) {
    for line in speaker_lines(speakers) {
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

/// Ask on this terminal which speaker to use: its id, or None if the
/// person skipped.
pub fn interactive_pick(app: &Arc<App>) -> Result<Option<String>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout();
    let picked = pick_from(
        &mut input,
        &mut output,
        || app.speaker_view().speakers,
        || search_and_wait(app, SEARCH_WAIT),
    )?;
    Ok(picked)
}

/// The prompt itself: searches first, lists, and reads a choice from
/// `input`. Accepts a number, Enter for the first speaker, "r" to search
/// again, "q" (or the end of input) to skip.
fn pick_from(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    mut list: impl FnMut() -> Vec<SpeakerInfo>,
    mut search: impl FnMut(),
) -> io::Result<Option<String>> {
    let mut line = String::new();
    let _ = writeln!(out, "Searching for speakers…");
    let _ = out.flush();
    search();
    loop {
        let speakers = list();
        let n = speakers.len();
        if n == 0 {
            let _ = write!(out, "No speakers found yet. [r] search again, [q] skip: ");
        } else {
            let _ = writeln!(out);
            write_speaker_list(out, &speakers);
            let _ = writeln!(out);
            let _ = write!(out, "Pick a speaker [1-{}] (Enter = the first, r = search again, q = skip): ", n);
        }
        let _ = out.flush();
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
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
                    let _ = writeln!(out, "\"{}\" isn't one of the choices.", answer);
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
            &mut typed.as_bytes(),
            &mut out,
            || shown.borrow().clone(),
            || {
                searches.set(searches.get() + 1);
                if searches.get() > 1 {
                    *shown.borrow_mut() = after.clone();
                }
            },
        )
        .unwrap();
        (picked, searches.get(), String::from_utf8(out).unwrap())
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
            speaker_lines(&rows),
            [
                "  [1]  Kitchen  (192.168.1.30, AirPlay)",
                "  [2]  Living Room  (192.168.1.31, UPnP)",
                "  [3]  Music Assistant",
                "  [4]  Den (AirPlay, added manually)  (192.168.1.40:7000, hidden)",
            ]
        );
    }
}
