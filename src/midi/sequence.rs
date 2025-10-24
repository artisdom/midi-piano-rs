use std::cmp::Ordering;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use midly::num::u4;
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

#[derive(Clone, Debug)]
pub struct PlaybackEvent {
    pub at: Duration,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct MidiSequence {
    pub events: Vec<PlaybackEvent>,
    pub duration: Duration,
}

impl MidiSequence {
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = fs::read(path)
            .with_context(|| format!("failed to read MIDI file {}", path.display()))?;
        let smf = Smf::parse(&contents)
            .with_context(|| format!("failed to parse MIDI file {}", path.display()))?;
        MidiSequence::from_smf(&smf)
    }

    fn from_smf(smf: &Smf<'_>) -> Result<Self> {
        let ppq = match smf.header.timing {
            Timing::Metrical(t) => t.as_int() as u32,
            Timing::Timecode(_fps, _subframe) => {
                bail!("timecode-based MIDI timing is not supported");
            }
        };

        if smf.header.format == midly::Format::Parallel && smf.tracks.len() < 2 {
            log::warn!("SMF declares format 1 but contains less than 2 tracks");
        }

        if smf.header.format == midly::Format::Sequential {
            bail!("SMF format 2 files are not supported");
        }

        let tempo_map = TempoMap::from_smf(smf, ppq)?;

        let mut raw_events: Vec<RawEvent> = Vec::new();
        for track in &smf.tracks {
            let mut tick_accumulator: u64 = 0;
            for event in track {
                tick_accumulator += event.delta.as_int() as u64;
                match &event.kind {
                    TrackEventKind::Meta(MetaMessage::Tempo(_)) => {
                        // handled in tempo map pass
                    }
                    TrackEventKind::Midi { channel, message } => {
                        if let Some(data) = encode_midi_message(*channel, message) {
                            raw_events.push(RawEvent {
                                tick: tick_accumulator,
                                data,
                            });
                        }
                    }
                    TrackEventKind::SysEx(data) => {
                        // Prepend 0xF0 and append 0xF7 if needed for completeness.
                        let mut payload = Vec::with_capacity(data.len() + 2);
                        payload.push(0xF0);
                        payload.extend_from_slice(data);
                        if !payload.ends_with(&[0xF7]) {
                            payload.push(0xF7);
                        }
                        raw_events.push(RawEvent {
                            tick: tick_accumulator,
                            data: payload,
                        });
                    }
                    TrackEventKind::Escape(data) => {
                        let mut payload = Vec::with_capacity(data.len() + 1);
                        payload.push(0xF7);
                        payload.extend_from_slice(data);
                        raw_events.push(RawEvent {
                            tick: tick_accumulator,
                            data: payload,
                        });
                    }
                    _ => {}
                }
            }
        }

        raw_events.sort_by(|a, b| {
            let ord = a.tick.cmp(&b.tick);
            if ord == Ordering::Equal {
                a.data.len().cmp(&b.data.len())
            } else {
                ord
            }
        });

        let mut events = Vec::with_capacity(raw_events.len());
        let mut total_duration = Duration::ZERO;
        for raw in raw_events {
            let at = tempo_map.ticks_to_duration(raw.tick);
            events.push(PlaybackEvent { at, data: raw.data });
            if at > total_duration {
                total_duration = at;
            }
        }

        Ok(MidiSequence {
            events,
            duration: total_duration,
        })
    }
}

#[derive(Debug, Clone)]
struct RawEvent {
    tick: u64,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TempoEntry {
    tick: u64,
    micros_per_quarter: u32,
}

#[derive(Debug, Clone)]
struct TempoMap {
    entries: Vec<TempoEntry>,
    ppq: u32,
}

impl TempoMap {
    fn from_smf(smf: &Smf<'_>, ppq: u32) -> Result<Self> {
        let mut entries = vec![TempoEntry {
            tick: 0,
            micros_per_quarter: 500_000,
        }];

        for track in &smf.tracks {
            let mut tick_accumulator: u64 = 0;
            for event in track {
                tick_accumulator += event.delta.as_int() as u64;
                if let TrackEventKind::Meta(MetaMessage::Tempo(tempo)) = event.kind {
                    let value = tempo.as_int();
                    entries.push(TempoEntry {
                        tick: tick_accumulator,
                        micros_per_quarter: value,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.tick.cmp(&b.tick));
        entries.dedup_by(|a, b| {
            if a.tick == b.tick {
                a.micros_per_quarter = b.micros_per_quarter;
                true
            } else {
                false
            }
        });

        Ok(TempoMap { entries, ppq })
    }

    fn ticks_to_duration(&self, tick: u64) -> Duration {
        let mut total_micros: u128 = 0;
        let mut last_tick: u64 = 0;
        let mut last_tempo = self
            .entries
            .first()
            .map(|entry| entry.micros_per_quarter)
            .unwrap_or(500_000);

        for entry in self.entries.iter().skip(1) {
            if entry.tick > tick {
                break;
            }
            total_micros += segment_duration(last_tempo, entry.tick - last_tick, self.ppq);
            last_tick = entry.tick;
            last_tempo = entry.micros_per_quarter;
        }

        total_micros += segment_duration(last_tempo, tick.saturating_sub(last_tick), self.ppq);
        Duration::from_micros(total_micros as u64)
    }
}

fn segment_duration(micros_per_quarter: u32, delta_ticks: u64, ppq: u32) -> u128 {
    if delta_ticks == 0 {
        return 0;
    }
    let numerator = micros_per_quarter as u128 * delta_ticks as u128;
    numerator / ppq as u128
}

fn encode_midi_message(channel: u4, message: &MidiMessage) -> Option<Vec<u8>> {
    let channel_value = channel.as_int();

    let status_base = match message {
        MidiMessage::NoteOff { .. } => 0x80,
        MidiMessage::NoteOn { .. } => 0x90,
        MidiMessage::Aftertouch { .. } => 0xA0,
        MidiMessage::Controller { .. } => 0xB0,
        MidiMessage::ProgramChange { .. } => 0xC0,
        MidiMessage::ChannelAftertouch { .. } => 0xD0,
        MidiMessage::PitchBend { .. } => 0xE0,
    };

    let status = status_base | channel_value;

    match message {
        MidiMessage::NoteOff { key, vel } => Some(vec![status, key.as_int(), vel.as_int()]),
        MidiMessage::NoteOn { key, vel } => Some(vec![status, key.as_int(), vel.as_int()]),
        MidiMessage::Aftertouch { key, vel } => Some(vec![status, key.as_int(), vel.as_int()]),
        MidiMessage::Controller { controller, value } => {
            Some(vec![status, controller.as_int(), value.as_int()])
        }
        MidiMessage::ProgramChange { program } => Some(vec![status, program.as_int()]),
        MidiMessage::ChannelAftertouch { vel } => Some(vec![status, vel.as_int()]),
        MidiMessage::PitchBend { bend } => {
            let raw: u16 = bend.0.as_int();
            let lsb = (raw & 0x7F) as u8;
            let msb = ((raw >> 7) & 0x7F) as u8;
            Some(vec![status, lsb, msb])
        }
    }
}
