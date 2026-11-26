//! Serializes output changes and feeds captured sound at its original rate.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use log::{debug, warn};

use super::tap::Tap;
use crate::platform::{PassThrough, PcOutputId};
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

const BLOCK_MS: u64 = 100;
const BLOCK_SAMPLES: usize =
    (WIRE_SAMPLE_RATE as u64 * BLOCK_MS / 1000) as usize * WIRE_CHANNELS as usize;

pub(super) enum Message {
    PassTo(Option<PcOutputId>),
    Replay {
        on: PcOutputId,
        samples: Vec<i16>,
        resume: Option<PcOutputId>,
    },
    Stop,
}

struct Request {
    message: Message,
    // Dropped after the worker handles or supersedes this request.
    #[cfg(test)]
    _done: Sender<()>,
}

pub(super) struct Worker {
    tx: Sender<Request>,
    thread: JoinHandle<()>,
    #[cfg(test)]
    last_done: Option<Receiver<()>>,
}

impl Worker {
    pub(super) fn start(tap: Tap, pass: Arc<dyn PassThrough>) -> Option<Self> {
        let (tx, rx) = unbounded();
        match thread::Builder::new()
            .name("stream-to-speaker-pc-sound".into())
            .spawn(move || run(&tap, &pass, rx))
        {
            Ok(thread) => Some(Self {
                tx,
                thread,
                #[cfg(test)]
                last_done: None,
            }),
            Err(e) => {
                warn!("couldn't start playback on this PC: {e}");
                None
            }
        }
    }

    pub(super) fn send(&mut self, message: Message) {
        let _ = self.tx.send(Request {
            message,
            #[cfg(test)]
            _done: {
                let (tx, rx) = crossbeam_channel::bounded(0);
                self.last_done = Some(rx);
                tx
            },
        });
    }

    #[cfg(test)]
    pub(super) fn settle(&self, timeout: Duration) {
        if let Some(done) = &self.last_done {
            assert_eq!(
                done.recv_timeout(timeout),
                Err(RecvTimeoutError::Disconnected)
            );
        }
    }

    pub(super) fn stop(mut self) {
        self.send(Message::Stop);
        let _ = self.thread.join();
    }
}

fn run(tap: &Tap, pass: &Arc<dyn PassThrough>, rx: Receiver<Request>) {
    let mut pending = None;
    'requests: while let Some(request) = pending.take().or_else(|| rx.recv().ok()) {
        match request.message {
            Message::PassTo(output) => pass_to(tap, pass, output.as_ref()),
            Message::Stop => break,
            Message::Replay {
                on,
                samples,
                resume,
            } => {
                tap.pass(None);
                match pass.open(&on) {
                    Ok(()) => {
                        let start = Instant::now();
                        for (i, block) in samples.chunks(BLOCK_SAMPLES).enumerate() {
                            // A queued command wins even at the first deadline. Opening
                            // its output replaces the replay without draining it first.
                            let due = start + Duration::from_millis(i as u64 * BLOCK_MS);
                            match rx.recv_deadline(due) {
                                Ok(newer) => {
                                    pending = Some(newer);
                                    continue 'requests;
                                }
                                Err(RecvTimeoutError::Disconnected) => break 'requests,
                                Err(RecvTimeoutError::Timeout) => {
                                    pass.write(block, WIRE_SAMPLE_RATE, WIRE_CHANNELS);
                                }
                            }
                        }
                    }
                    Err(e) => debug!("couldn't replay on this PC: {e}"),
                }
                pass.close();
                if let Some(output) = resume {
                    // A request may have arrived while close drained the last block.
                    match rx.try_recv() {
                        Ok(newer) => pending = Some(newer),
                        Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => pass_to(tap, pass, Some(&output)),
                    }
                }
            }
        }
    }
    pass_to(tap, pass, None);
}

fn pass_to(tap: &Tap, pass: &Arc<dyn PassThrough>, output: Option<&PcOutputId>) {
    tap.pass(None);
    if let Some(id) = output {
        match pass.open(id) {
            Ok(()) => tap.pass(Some(pass.clone())),
            Err(e) => {
                pass.close();
                debug!("couldn't play through this PC: {e}");
            }
        }
    } else {
        pass.close();
    }
}
