use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, button, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Color, Element, Length, Subscription, Task, Theme, application, executor, time};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;

use crate::devices::{MidiDeviceDescriptor, MidiDeviceManager};
use crate::midi::sink::MidiTransport;
use crate::midi::{MidiLibrary, MidiPlayer, MidiSequence, PlayerEvent, SharedMidiSink};

const TICK_INTERVAL: Duration = Duration::from_millis(100);

type AsyncResult<T> = Result<T, String>;

#[derive(Debug, Clone)]
enum Message {
    LibraryLoaded(AsyncResult<MidiLibrary>),
    DevicesRefreshed(AsyncResult<Vec<MidiDeviceDescriptor>>),
    DeviceSelected(Uuid),
    SongSelected(Uuid),
    SearchChanged(String),
    PlayPressed,
    StopPressed,
    AddLocalFile,
    PlaybackPrepared(AsyncResult<PreparedPlayback>),
    RefreshDevices,
    Tick,
    DismissStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceChoice {
    id: Uuid,
    name: String,
    transport: MidiTransport,
}

impl DeviceChoice {
    fn from(descriptor: &MidiDeviceDescriptor) -> Self {
        Self {
            id: descriptor.info.id,
            name: descriptor.info.name.clone(),
            transport: descriptor.info.transport,
        }
    }
}

impl fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = match self.transport {
            MidiTransport::Usb => "USB",
            MidiTransport::Bluetooth => "BLE",
        };
        write!(f, "[{transport}] {}", self.name)
    }
}

pub struct MidiPianoApp {
    library: MidiLibrary,
    device_manager: Arc<Mutex<MidiDeviceManager>>,
    devices: Vec<DeviceChoice>,
    selected_device: Option<Uuid>,
    selected_song: Option<Uuid>,
    search_query: String,
    midi_player: MidiPlayer,
    player_events: UnboundedReceiver<PlayerEvent>,
    current_sink: Option<SharedMidiSink>,
    playback_phase: PlaybackPhase,
    playback_progress: Option<PlaybackProgress>,
    status_message: Option<String>,
    error_message: Option<String>,
    is_scanning_devices: bool,
    is_preparing_playback: bool,
}

impl MidiPianoApp {
    fn init() -> (Self, Task<Message>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let device_manager = Arc::new(Mutex::new(MidiDeviceManager::new()));

        let app = MidiPianoApp {
            library: MidiLibrary::default(),
            device_manager: device_manager.clone(),
            devices: Vec::new(),
            selected_device: None,
            selected_song: None,
            search_query: String::new(),
            midi_player: MidiPlayer::new(event_tx),
            player_events: event_rx,
            current_sink: None,
            playback_phase: PlaybackPhase::Idle,
            playback_progress: None,
            status_message: None,
            error_message: None,
            is_scanning_devices: true,
            is_preparing_playback: false,
        };

        let task = Task::batch([
            Task::perform(load_library(), Message::LibraryLoaded),
            Task::perform(refresh_devices(device_manager), Message::DevicesRefreshed),
        ]);

        (app, task)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::LibraryLoaded(result) => {
                match result {
                    Ok(library) => {
                        self.library = library;
                        self.status_message = Some("Library loaded".into());
                    }
                    Err(err) => {
                        self.error_message = Some(format!("Failed to load MIDI library: {err}"));
                    }
                }
                Task::none()
            }
            Message::DevicesRefreshed(result) => {
                self.is_scanning_devices = false;
                match result {
                    Ok(descriptors) => {
                        self.devices = descriptors.iter().map(DeviceChoice::from).collect();
                        if let Some(selected) = self.selected_device {
                            if !self.devices.iter().any(|choice| choice.id == selected) {
                                self.selected_device = None;
                            }
                        }
                        self.status_message = Some("Devices updated".into());
                    }
                    Err(err) => {
                        self.error_message = Some(format!("Failed to refresh devices: {err}"));
                    }
                }
                Task::none()
            }
            Message::RefreshDevices => {
                self.is_scanning_devices = true;
                Task::perform(
                    refresh_devices(self.device_manager.clone()),
                    Message::DevicesRefreshed,
                )
            }
            Message::DeviceSelected(id) => {
                self.selected_device = Some(id);
                Task::none()
            }
            Message::SongSelected(id) => {
                self.selected_song = Some(id);
                Task::none()
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
                Task::none()
            }
            Message::PlayPressed => {
                if self.is_preparing_playback {
                    return Task::none();
                }

                let song_id = match self.selected_song {
                    Some(id) => id,
                    None => {
                        self.error_message = Some("Select a MIDI file to play".into());
                        return Task::none();
                    }
                };

                let device_id = match self.selected_device {
                    Some(id) => id,
                    None => {
                        self.error_message = Some("Select a MIDI output device first".into());
                        return Task::none();
                    }
                };

                let entry = match self.library.get(&song_id).cloned() {
                    Some(entry) => entry,
                    None => {
                        self.error_message =
                            Some("Selected MIDI file is no longer available".into());
                        return Task::none();
                    }
                };

                self.is_preparing_playback = true;
                self.playback_phase = PlaybackPhase::Preparing;
                self.status_message = Some(format!("Preparing {}", entry.name));

                let path = entry.path.clone();
                Task::perform(
                    prepare_playback(path, device_id, self.device_manager.clone()),
                    Message::PlaybackPrepared,
                )
            }
            Message::PlaybackPrepared(result) => {
                self.is_preparing_playback = false;
                match result {
                    Ok(prepared) => {
                        match self
                            .midi_player
                            .start_playback(prepared.sequence.clone(), prepared.sink.clone())
                        {
                            Ok(_) => {
                                self.current_sink = Some(prepared.sink);
                                self.playback_phase = PlaybackPhase::Playing;
                                self.playback_progress = Some(PlaybackProgress {
                                    elapsed: Duration::ZERO,
                                    total: prepared.sequence.duration,
                                });
                            }
                            Err(err) => {
                                self.error_message =
                                    Some(format!("Failed to start playback: {err:?}"));
                                self.playback_phase = PlaybackPhase::Idle;
                                self.playback_progress = None;
                            }
                        }
                    }
                    Err(err) => {
                        self.error_message = Some(format!("Failed to prepare playback: {err}"));
                        self.playback_phase = PlaybackPhase::Idle;
                        self.playback_progress = None;
                    }
                }
                Task::none()
            }
            Message::StopPressed => {
                self.midi_player.stop();
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.current_sink = None;
                Task::none()
            }
            Message::AddLocalFile => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("MIDI Files", &["mid", "midi"])
                    .pick_file()
                {
                    match self.library.add_local_file(path) {
                        Ok(entry) => {
                            self.selected_song = Some(entry.id);
                            self.status_message = Some(format!("Added {}", entry.name));
                        }
                        Err(err) => {
                            self.error_message = Some(format!("Failed to add MIDI file: {err:?}"));
                        }
                    }
                }
                Task::none()
            }
            Message::Tick => {
                while let Ok(event) = self.player_events.try_recv() {
                    self.handle_player_event(event);
                }
                Task::none()
            }
            Message::DismissStatus => {
                self.status_message = None;
                self.error_message = None;
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let content = column![
            self.device_section(),
            self.playback_controls(),
            self.library_view(),
            self.status_banner()
        ]
        .spacing(16)
        .padding(16);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Left)
            .align_y(Vertical::Top)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        time::every(TICK_INTERVAL).map(|_| Message::Tick)
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn handle_player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Started { total } => {
                self.playback_phase = PlaybackPhase::Playing;
                self.playback_progress = Some(PlaybackProgress {
                    elapsed: Duration::ZERO,
                    total,
                });
                self.status_message = Some("Playback started".into());
            }
            PlayerEvent::Progress { elapsed, total } => {
                self.playback_progress = Some(PlaybackProgress { elapsed, total });
            }
            PlayerEvent::Finished => {
                self.playback_phase = PlaybackPhase::Finished;
                self.status_message = Some("Playback finished".into());
                self.current_sink = None;
            }
            PlayerEvent::Stopped => {
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.status_message = Some("Playback stopped".into());
                self.current_sink = None;
            }
            PlayerEvent::Error(message) => {
                self.error_message = Some(message);
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.current_sink = None;
            }
        }
    }

    fn filtered_entries(&self) -> Vec<&crate::midi::MidiEntry> {
        if self.search_query.trim().is_empty() {
            return self.library.entries().iter().collect();
        }
        let query = self.search_query.to_lowercase();
        self.library
            .entries()
            .iter()
            .filter(|entry| entry.name.to_lowercase().contains(&query))
            .collect()
    }

    fn device_section(&self) -> Element<'_, Message> {
        let selected_choice = self
            .selected_device
            .and_then(|id| self.devices.iter().find(|choice| choice.id == id))
            .cloned();

        let pick_list = pick_list(
            self.devices.clone(),
            selected_choice,
            |choice: DeviceChoice| Message::DeviceSelected(choice.id),
        )
        .placeholder(if self.is_scanning_devices {
            "Scanning devices..."
        } else {
            "Select output device"
        });

        let refresh_button = button("Refresh").on_press(Message::RefreshDevices);
        let add_button = button("Add Local MIDI").on_press(Message::AddLocalFile);

        row![
            pick_list,
            refresh_button.style(iced::widget::button::secondary),
            add_button.style(iced::widget::button::secondary)
        ]
        .spacing(12)
        .into()
    }

    fn playback_controls(&self) -> Element<'_, Message> {
        let play_button = button("Play")
            .on_press(Message::PlayPressed)
            .style(iced::widget::button::primary);
        let stop_button = button("Stop")
            .on_press(Message::StopPressed)
            .style(iced::widget::button::secondary);

        let status_text = match self.playback_phase {
            PlaybackPhase::Idle => text("Ready"),
            PlaybackPhase::Preparing => text("Preparing playback..."),
            PlaybackPhase::Playing => {
                if let Some(progress) = &self.playback_progress {
                    text(format!(
                        "Playing ({}/{} )",
                        format_duration(progress.elapsed),
                        format_duration(progress.total)
                    ))
                } else {
                    text("Playing...")
                }
            }
            PlaybackPhase::Finished => text("Completed"),
        }
        .size(16)
        .width(Length::Fill);

        row![play_button, stop_button, status_text]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into()
    }

    fn library_view(&self) -> Element<'_, Message> {
        let search = text_input("Search MIDI files...", &self.search_query)
            .on_input(Message::SearchChanged)
            .padding(8);

        let mut list_column = Column::new().spacing(4);

        for entry in self.filtered_entries() {
            let is_selected = Some(entry.id) == self.selected_song;
            let label = if matches!(entry.origin, crate::midi::MidiOrigin::Local) {
                format!("{} (Local)", entry.name)
            } else {
                entry.name.clone()
            };

            let mut button = button(text(label)).on_press(Message::SongSelected(entry.id));
            if is_selected {
                button = button.style(iced::widget::button::success);
            }
            list_column = list_column.push(button);
        }

        let scroll = scrollable(list_column).height(Length::Fill);

        column![search, scroll]
            .spacing(12)
            .height(Length::Fill)
            .into()
    }

    fn status_banner(&self) -> Element<'_, Message> {
        if let Some(error) = &self.error_message {
            return row![
                text(error).size(16).color(Color::from_rgb(0.9, 0.4, 0.4)),
                button("Dismiss")
                    .on_press(Message::DismissStatus)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8)
            .into();
        }

        if let Some(status) = &self.status_message {
            return row![
                text(status).size(16).color(Color::from_rgb(0.4, 0.9, 0.4)),
                button("Dismiss")
                    .on_press(Message::DismissStatus)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8)
            .into();
        }

        column![].into()
    }
}

struct PreparedPlayback {
    sequence: Arc<MidiSequence>,
    sink: SharedMidiSink,
}

impl fmt::Debug for PreparedPlayback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedPlayback")
            .field("duration", &self.sequence.duration)
            .finish()
    }
}

impl Clone for PreparedPlayback {
    fn clone(&self) -> Self {
        Self {
            sequence: Arc::clone(&self.sequence),
            sink: self.sink.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PlaybackPhase {
    Idle,
    Preparing,
    Playing,
    Finished,
}

#[derive(Debug, Clone)]
struct PlaybackProgress {
    elapsed: Duration,
    total: Duration,
}

async fn load_library() -> AsyncResult<MidiLibrary> {
    tokio::task::spawn_blocking(MidiLibrary::load_with_assets)
        .await
        .map_err(|err| format!("library loader task failed: {err:?}"))?
        .map_err(|err| format!("{err:?}"))
}

async fn refresh_devices(
    manager: Arc<Mutex<MidiDeviceManager>>,
) -> AsyncResult<Vec<MidiDeviceDescriptor>> {
    let mut guard = manager.lock().await;
    guard.refresh().await.map_err(|err| format!("{err:?}"))
}

async fn prepare_playback(
    path: PathBuf,
    device_id: Uuid,
    manager: Arc<Mutex<MidiDeviceManager>>,
) -> AsyncResult<PreparedPlayback> {
    let sequence = tokio::task::spawn_blocking(move || MidiSequence::from_file(&path))
        .await
        .map_err(|err| format!("sequence loader task failed: {err:?}"))?
        .map_err(|err| format!("{err:?}"))?;
    let sequence = Arc::new(sequence);

    let sink = {
        let guard = manager.lock().await;
        guard
            .connect(&device_id)
            .await
            .map_err(|err| format!("{err:?}"))?
    };

    Ok(PreparedPlayback { sequence, sink })
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn update(state: &mut MidiPianoApp, message: Message) -> Task<Message> {
    state.update(message)
}

fn view(state: &MidiPianoApp) -> Element<'_, Message> {
    state.view()
}

fn subscription(state: &MidiPianoApp) -> Subscription<Message> {
    state.subscription()
}

fn theme(state: &MidiPianoApp) -> Theme {
    state.theme()
}

pub fn run() -> iced::Result {
    application("MIDI Piano Player", update, view)
        .subscription(subscription)
        .theme(theme)
        .executor::<executor::Default>()
        .run_with(MidiPianoApp::init)
}
