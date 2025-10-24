use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, button, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Application, Color, Command, Element, Length, Subscription, Theme, executor, time};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;

use crate::devices::{MidiDeviceDescriptor, MidiDeviceManager};
use crate::midi::sink::MidiTransport;
use crate::midi::{MidiEntry, MidiLibrary, MidiPlayer, MidiSequence, PlayerEvent, SharedMidiSink};

const TICK_INTERVAL: Duration = Duration::from_millis(100);

type AsyncResult<T> = std::result::Result<T, String>;

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
    fn new_state() -> (Self, Command<Message>) {
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

        let initial_command = Command::batch(vec![
            Command::perform(load_library(), Message::LibraryLoaded),
            Command::perform(refresh_devices(device_manager), Message::DevicesRefreshed),
        ]);

        (app, initial_command)
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

    fn filtered_entries(&self) -> Vec<&MidiEntry> {
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
}

impl Application for MidiPianoApp {
    type Executor = executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Self::Message>) {
        MidiPianoApp::new_state()
    }

    fn title(&self) -> String {
        "MIDI Piano Player".to_string()
    }

    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
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
                Command::none()
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
                Command::none()
            }
            Message::RefreshDevices => {
                self.is_scanning_devices = true;
                Command::perform(
                    refresh_devices(self.device_manager.clone()),
                    Message::DevicesRefreshed,
                )
            }
            Message::DeviceSelected(id) => {
                self.selected_device = Some(id);
                Command::none()
            }
            Message::SongSelected(id) => {
                self.selected_song = Some(id);
                Command::none()
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
                Command::none()
            }
            Message::PlayPressed => {
                if self.is_preparing_playback {
                    return Command::none();
                }

                let song_id = match self.selected_song {
                    Some(id) => id,
                    None => {
                        self.error_message = Some("Select a MIDI file to play".into());
                        return Command::none();
                    }
                };

                let device_id = match self.selected_device {
                    Some(id) => id,
                    None => {
                        self.error_message = Some("Select a MIDI output device first".into());
                        return Command::none();
                    }
                };

                let entry = match self.library.get(&song_id).cloned() {
                    Some(entry) => entry,
                    None => {
                        self.error_message =
                            Some("Selected MIDI file is no longer available".into());
                        return Command::none();
                    }
                };

                self.is_preparing_playback = true;
                self.playback_phase = PlaybackPhase::Preparing;
                self.status_message = Some(format!("Preparing {}", entry.name));

                let path = entry.path.clone();

                Command::perform(
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
                Command::none()
            }
            Message::StopPressed => {
                self.midi_player.stop();
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.current_sink = None;
                Command::none()
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
                Command::none()
            }
            Message::Tick => {
                while let Ok(event) = self.player_events.try_recv() {
                    self.handle_player_event(event);
                }
                Command::none()
            }
            Message::DismissStatus => {
                self.status_message = None;
                self.error_message = None;
                Command::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let device_section = self.device_section();
        let playback_section = self.playback_controls();
        let library_view = self.library_view();

        let content = column![
            device_section,
            playback_section,
            library_view,
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

    fn theme(&self) -> Self::Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        time::every(TICK_INTERVAL).map(|_| Message::Tick)
    }
}

impl MidiPianoApp {
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

        row![pick_list, refresh_button, add_button]
            .spacing(12)
            .into()
    }

    fn playback_controls(&self) -> Element<'_, Message> {
        let play_button = button("Play")
            .on_press(Message::PlayPressed)
            .style(iced::theme::Button::Primary);
        let stop_button = button("Stop").on_press(Message::StopPressed);

        let mut status_text = match self.playback_phase {
            PlaybackPhase::Idle => text("Ready").size(16),
            PlaybackPhase::Preparing => text("Preparing playback...").size(16),
            PlaybackPhase::Playing => {
                if let Some(progress) = &self.playback_progress {
                    text(format!(
                        "Playing ({}/{} )",
                        format_duration(progress.elapsed),
                        format_duration(progress.total)
                    ))
                    .size(16)
                } else {
                    text("Playing...").size(16)
                }
            }
            PlaybackPhase::Finished => text("Completed").size(16),
        };

        status_text = status_text.width(Length::Fill);

        row![play_button, stop_button, status_text]
            .spacing(12)
            .align_items(iced::Alignment::Center)
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
            let mut row = button(text(label)).on_press(Message::SongSelected(entry.id));
            if is_selected {
                row = row.style(iced::theme::Button::Positive);
            }
            list_column = list_column.push(row);
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
                text(error)
                    .size(16)
                    .style(iced::theme::Text::Color(Color::from_rgb(0.9, 0.4, 0.4))),
                button("Dismiss").on_press(Message::DismissStatus)
            ]
            .spacing(8)
            .into();
        }

        if let Some(status) = &self.status_message {
            return row![
                text(status)
                    .size(16)
                    .style(iced::theme::Text::Color(Color::from_rgb(0.4, 0.9, 0.4))),
                button("Dismiss").on_press(Message::DismissStatus)
            ]
            .spacing(8)
            .into();
        }

        column![].into()
    }
}

#[derive(Debug, Clone)]
pub enum Message {
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
        DeviceChoice {
            id: descriptor.info.id,
            name: descriptor.info.name.clone(),
            transport: descriptor.info.transport,
        }
    }
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let transport = match self.transport {
            MidiTransport::Usb => "USB",
            MidiTransport::Bluetooth => "BLE",
        };
        write!(f, "[{transport}] {}", self.name)
    }
}

pub struct PreparedPlayback {
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
