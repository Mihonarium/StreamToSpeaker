//! Interactive speaker picker.
//!
//! When the user runs `stream-to-speaker` from a real terminal without
//! `--player`, we list the discovered renderers and let them pick one by
//! index — modeled on swyh-rs's button list, but text-mode.
//!
//! Non-interactive contexts (piped stdin, service host, CI) are never
//! asked, and nothing is picked for them.

use crate::ssdp::{DiscoveryState, Renderer};
use anyhow::{anyhow, Result};
use log::info;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Returns true when stdin and stdout are both attached to a terminal.
/// Used to gate the interactive prompt; non-interactive callers see a
/// machine-friendly list instead.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Pretty-print the renderer list to stdout. Returns the number printed.
pub fn print_speaker_list(renderers: &[Renderer]) -> usize {
    if renderers.is_empty() {
        println!("(no speakers discovered yet)");
        return 0;
    }
    println!();
    println!("Discovered speakers:");
    for (i, r) in renderers.iter().enumerate() {
        // display_name shows Sonos groups as "Living Room + Kitchen".
        println!("  [{}]  {}  ({})", i + 1, r.display_name(), r.ip);
    }
    println!();
    renderers.len()
}

/// Block (with a generous deadline) for at least one renderer to appear,
/// then return the current list. Useful both for the interactive picker
/// and for `--list-speakers`.
pub fn wait_for_first_discovery(
    state: &Arc<DiscoveryState>,
    max_wait: Duration,
) -> Vec<Renderer> {
    let start = Instant::now();
    while state.renderers().is_empty() && start.elapsed() < max_wait {
        thread::sleep(Duration::from_millis(200));
    }
    state.renderers()
}

/// Interactive picker. Returns Ok(Some(renderer)) on a successful choice,
/// Ok(None) if the user explicitly skipped (e.g. blank line), and Err on
/// I/O failure or no candidates after retrying.
///
/// Supported input:
///   - a number (1-based) → pick that speaker
///   - "r" / "refresh"   → re-run discovery and re-prompt
///   - "q" / blank line  → skip (return None — caller can decide what to do)
pub fn interactive_pick(state: &Arc<DiscoveryState>) -> Result<Option<Renderer>> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = String::new();

    loop {
        let renderers = wait_for_first_discovery(state, Duration::from_secs(5));
        let n = print_speaker_list(&renderers);

        if n == 0 {
            print!("No speakers discovered yet. [r]etry / [q]uit: ");
            stdout.flush().ok();
            line.clear();
            if stdin.lock().read_line(&mut line)? == 0 {
                return Ok(None);
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" | "q" | "quit" => return Ok(None),
                "r" | "refresh" => continue,
                _ => continue,
            }
        }

        print!(
            "Pick a speaker [1-{}] (Enter=first, r=refresh, q=skip): ",
            n
        );
        stdout.flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(None);
        }

        let trimmed = line.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "" => {
                info!("no choice given; using first speaker");
                return Ok(renderers.into_iter().next());
            }
            "q" | "quit" => return Ok(None),
            "r" | "refresh" => continue,
            other => match other.parse::<usize>() {
                Ok(idx) if idx >= 1 && idx <= n => {
                    return Ok(Some(renderers.into_iter().nth(idx - 1).unwrap()));
                }
                _ => {
                    println!("'{}' isn't a valid choice; try again.", trimmed);
                    continue;
                }
            },
        }
    }
}

/// The renderer `hint` names (substring or IP match), once discovery has
/// had a moment.
pub fn resolve(state: &Arc<DiscoveryState>, hint: &str) -> Result<Option<Renderer>> {
    let renderers = wait_for_first_discovery(state, Duration::from_secs(5));
    match state.find(hint) {
        Some(r) => Ok(Some(r)),
        None => Err(anyhow!(
            "no speaker matched {:?}; discovered: {:?}",
            hint,
            renderers.iter().map(|r| r.display_name()).collect::<Vec<_>>()
        )),
    }
}
