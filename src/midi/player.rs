use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant as TokioInstant};

use super::sequence::MidiSequence;
use super::sink::SharedMidiSink;

const PROGRESS_UPDATE_STEP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Started { total: Duration },
    Progress { elapsed: Duration, total: Duration },
    Finished,
    Stopped,
    Error(String),
}

struct PlaybackHandle {
    cancel: Arc<Notify>,
    join: JoinHandle<()>,
}

impl PlaybackHandle {
    fn new(cancel: Arc<Notify>, join: JoinHandle<()>) -> Self {
        Self { cancel, join }
    }
}

pub struct MidiPlayer {
    event_sender: mpsc::UnboundedSender<PlayerEvent>,
    playback: Option<PlaybackHandle>,
    active_sequence: Option<Arc<MidiSequence>>,
}

impl MidiPlayer {
    pub fn new(event_sender: mpsc::UnboundedSender<PlayerEvent>) -> Self {
        Self {
            event_sender,
            playback: None,
            active_sequence: None,
        }
    }

    pub fn start_playback(
        &mut self,
        sequence: Arc<MidiSequence>,
        sink: SharedMidiSink,
    ) -> Result<()> {
        if sequence.events.is_empty() {
            return Err(anyhow!(
                "selected MIDI file does not contain playable events"
            ));
        }

        self.stop_internal();
        self.active_sequence = Some(sequence.clone());

        let cancel = Arc::new(Notify::new());
        let cancel_clone = cancel.clone();
        let sender = self.event_sender.clone();
        let total_duration = sequence.duration;

        let join = tokio::spawn(async move {
            let _ = sender.send(PlayerEvent::Started {
                total: total_duration,
            });
            let _ = sender.send(PlayerEvent::Progress {
                elapsed: Duration::ZERO,
                total: total_duration,
            });

            let start = TokioInstant::now();
            let mut last_reported = Duration::ZERO;

            let mut index = 0;
            let total_events = sequence.events.len();
            while index < total_events {
                let event_at = sequence.events[index].at;
                let target = start + event_at;
                let wait_result = tokio::select! {
                    _ = time::sleep_until(target) => WaitOutcome::Completed,
                    _ = cancel_clone.notified() => WaitOutcome::Cancelled,
                };

                if let WaitOutcome::Cancelled = wait_result {
                    return;
                }

                let mut batch: Vec<Vec<u8>> = Vec::new();
                while index < total_events && sequence.events[index].at == event_at {
                    batch.push(sequence.events[index].data.clone());
                    index += 1;
                }

                if let Err(err) = sink.send_batch(&batch).await {
                    let _ = sender.send(PlayerEvent::Error(err.to_string()));
                    return;
                }

                if event_at >= last_reported + PROGRESS_UPDATE_STEP || event_at >= total_duration {
                    last_reported = event_at;
                    let _ = sender.send(PlayerEvent::Progress {
                        elapsed: event_at,
                        total: total_duration,
                    });
                }
            }

            let _ = sender.send(PlayerEvent::Progress {
                elapsed: total_duration,
                total: total_duration,
            });
            let _ = sender.send(PlayerEvent::Finished);
        });

        self.playback = Some(PlaybackHandle::new(cancel, join));

        Ok(())
    }

    pub fn stop(&mut self) {
        self.stop_internal();
    }

    fn stop_internal(&mut self) {
        if let Some(handle) = self.playback.take() {
            handle.cancel.notify_waiters();
            let _ = self.event_sender.send(PlayerEvent::Stopped);

            let join = handle.join;
            tokio::spawn(async move {
                let _ = join.await;
            });
        }
        self.active_sequence = None;
    }
}

enum WaitOutcome {
    Completed,
    Cancelled,
}
