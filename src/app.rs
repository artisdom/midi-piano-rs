use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, button, column, container, pick_list, row, scrollable, text, text::Shaping, text_input,
};
use iced::{
    Color, Element, Font, Length, Subscription, Task, Theme, application, executor, time, window,
};
use rand::{
    rng,
    seq::{IndexedRandom, IteratorRandom, SliceRandom},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;

use crate::devices::{MidiDeviceDescriptor, MidiDeviceManager};
use crate::midi::sink::MidiTransport;
use crate::midi::{MidiLibrary, MidiPlayer, MidiSequence, PlayerEvent, SharedMidiSink};

const TICK_INTERVAL: Duration = Duration::from_millis(100);

type AsyncResult<T> = Result<T, String>;

const NOTO_SANS_SC: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-Regular.otf");
const DEFAULT_FONT: Font = Font::with_name("Noto Sans SC");
const USER_DATA_FILE: &str = "data/user_preferences.json";

#[derive(Debug, Clone)]
enum Message {
    LibraryLoaded(AsyncResult<MidiLibrary>),
    DevicesRefreshed(AsyncResult<Vec<MidiDeviceDescriptor>>),
    UserDataLoaded(AsyncResult<UserPreferences>),
    PreferencesSaved(AsyncResult<()>),
    TreeDataLoaded {
        request_id: u64,
        tree: LibraryNode,
        folders: HashMap<String, Vec<Uuid>>,
    },
    TreeDataFailed {
        request_id: u64,
        error: String,
    },
    DeviceSelected(Uuid),
    SongSelected(Uuid),
    SearchChanged(String),
    PlayPressed,
    StopPressed,
    AddLocalFile,
    PlaybackPrepared(AsyncResult<PreparedPlayback>),
    RefreshDevices,
    SetRating(Uuid, u8),
    ToggleFavorite(Uuid),
    SwitchTab(LibraryTab),
    ToggleFolder(String),
    SelectFolder(String),
    PlaylistDraftAdd(Uuid),
    PlaylistDraftRemove(usize),
    PlaylistDraftNameChanged(String),
    PlaylistDraftClear,
    PlaylistDraftSave,
    StartPlayback(Uuid),
    PlayFavorites {
        shuffle: bool,
    },
    PlayPlaylist {
        id: Uuid,
        shuffle: bool,
    },
    NextTrack,
    PrevTrack,
    PlaylistSelect(Option<Uuid>),
    PlaylistDelete(Uuid),
    PlaylistLoadToDraft(Uuid),
    GenerateRandomPlaylist,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaylistChoice {
    id: Uuid,
    name: String,
}

impl PlaylistChoice {
    fn from(playlist: &Playlist) -> Self {
        Self {
            id: playlist.id,
            name: playlist.name.clone(),
        }
    }
}

impl fmt::Display for PlaylistChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct UserPreferences {
    ratings: HashMap<Uuid, u8>,
    favorites: HashSet<Uuid>,
    playlists: Vec<Playlist>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Playlist {
    id: Uuid,
    name: String,
    tracks: Vec<Uuid>,
}

impl Playlist {
    fn new(name: impl Into<String>, tracks: Vec<Uuid>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            tracks,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PlaylistDraft {
    name: String,
    tracks: Vec<Uuid>,
}

#[derive(Debug, Clone)]
struct PlayQueue {
    tracks: Vec<Uuid>,
    index: usize,
    mode: QueueMode,
}

#[derive(Debug, Clone)]
enum QueueMode {
    Single,
    Favorites,
    Playlist(Uuid),
}

#[derive(Debug, Clone)]
struct LibraryNode {
    id: String,
    name: String,
    children: BTreeMap<String, LibraryNode>,
}

impl LibraryNode {
    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            children: BTreeMap::new(),
        }
    }

    fn ensure_child(&mut self, id: String, name: String) -> &mut LibraryNode {
        self.children
            .entry(id.clone())
            .or_insert_with(|| LibraryNode::new(id, name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Tree,
    Favorites,
}

#[derive(Debug, Clone)]
struct TreeItem {
    id: String,
    name: String,
    depth: usize,
    has_children: bool,
    is_expanded: bool,
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
    user_prefs: UserPreferences,
    active_tab: LibraryTab,
    library_tree: LibraryNode,
    folder_entries: HashMap<String, Vec<Uuid>>,
    expanded_folders: HashSet<String>,
    selected_folder: Option<String>,
    playlist_draft: PlaylistDraft,
    selected_playlist: Option<Uuid>,
    tree_cache: Vec<TreeItem>,
    tree_loading: bool,
    tree_request_id: u64,
    play_queue: Option<PlayQueue>,
}

impl MidiPianoApp {
    fn init() -> (Self, Task<Message>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let device_manager = Arc::new(Mutex::new(MidiDeviceManager::new()));
        let mut expanded_folders = HashSet::new();
        expanded_folders.insert("root".into());

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
            user_prefs: UserPreferences::default(),
            active_tab: LibraryTab::Tree,
            library_tree: LibraryNode::new("root".into(), "Library".into()),
            folder_entries: HashMap::new(),
            expanded_folders,
            selected_folder: None,
            playlist_draft: PlaylistDraft::default(),
            selected_playlist: None,
            tree_cache: Vec::new(),
            tree_loading: false,
            tree_request_id: 0,
            play_queue: None,
        };

        let mut app = app;
        app.refresh_tree_cache();

        let task = Task::batch([
            Task::perform(load_library(), Message::LibraryLoaded),
            Task::perform(refresh_devices(device_manager), Message::DevicesRefreshed),
            Task::perform(load_user_preferences(), Message::UserDataLoaded),
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
                        return self.schedule_tree_rebuild();
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
            Message::UserDataLoaded(result) => {
                match result {
                    Ok(prefs) => {
                        self.user_prefs = prefs;
                        self.status_message = Some("Preferences loaded".into());
                    }
                    Err(err) => {
                        self.error_message = Some(format!("Failed to load preferences: {err}"));
                    }
                }
                Task::none()
            }
            Message::PreferencesSaved(result) => {
                match result {
                    Ok(()) => {
                        self.status_message = Some("Preferences saved".into());
                    }
                    Err(err) => {
                        self.error_message = Some(format!("Failed to save preferences: {err}"));
                    }
                }
                Task::none()
            }
            Message::TreeDataLoaded {
                request_id,
                tree,
                folders,
            } => {
                if request_id == self.tree_request_id {
                    self.tree_loading = false;
                    self.apply_tree_data(tree, folders);
                }
                Task::none()
            }
            Message::TreeDataFailed { request_id, error } => {
                if request_id == self.tree_request_id {
                    self.tree_loading = false;
                    self.error_message = Some(format!("Failed to update library tree: {error}"));
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
            Message::SwitchTab(tab) => {
                if self.active_tab != tab {
                    self.active_tab = tab;
                    if matches!(self.active_tab, LibraryTab::Tree) {
                        if self.selected_folder.is_none() {
                            self.selected_folder = Some("root".into());
                        }
                        if self.tree_cache.is_empty() && !self.tree_loading {
                            return self.schedule_tree_rebuild();
                        }
                    }
                }
                Task::none()
            }
            Message::ToggleFolder(folder_id) => {
                self.selected_folder = Some(folder_id.clone());
                if !self.expanded_folders.remove(&folder_id) {
                    self.expanded_folders.insert(folder_id);
                }
                self.refresh_tree_cache();
                Task::none()
            }
            Message::SelectFolder(folder_id) => {
                if self.folder_entries.contains_key(&folder_id) {
                    self.selected_folder = Some(folder_id);
                }
                Task::none()
            }
            Message::SetRating(id, rating) => {
                if rating == 0 {
                    self.user_prefs.ratings.remove(&id);
                } else if rating <= 5 {
                    self.user_prefs.ratings.insert(id, rating);
                }
                self.status_message = Some("Rating updated".into());
                self.save_preferences_task()
            }
            Message::ToggleFavorite(id) => {
                if !self.user_prefs.favorites.remove(&id) {
                    self.user_prefs.favorites.insert(id);
                    self.status_message = Some("Added to favorites".into());
                } else {
                    self.status_message = Some("Removed from favorites".into());
                }
                self.save_preferences_task()
            }
            Message::PlaylistDraftAdd(id) => {
                if self.library.get(&id).is_none() {
                    self.error_message = Some("Selected track is not available".into());
                } else if !self.playlist_draft.tracks.contains(&id) {
                    self.playlist_draft.tracks.push(id);
                    self.status_message = Some("Track added to draft playlist".into());
                }
                Task::none()
            }
            Message::PlaylistDraftRemove(index) => {
                if index < self.playlist_draft.tracks.len() {
                    self.playlist_draft.tracks.remove(index);
                    self.status_message = Some("Track removed from draft playlist".into());
                }
                Task::none()
            }
            Message::PlaylistDraftNameChanged(name) => {
                self.playlist_draft.name = name;
                Task::none()
            }
            Message::PlaylistDraftClear => {
                self.playlist_draft = PlaylistDraft::default();
                self.status_message = Some("Playlist draft cleared".into());
                Task::none()
            }
            Message::PlaylistDraftSave => {
                if self.playlist_draft.tracks.is_empty() {
                    self.error_message =
                        Some("Add at least one track before saving a playlist".into());
                    return Task::none();
                }
                let name = if self.playlist_draft.name.trim().is_empty() {
                    format!("Playlist {}", self.user_prefs.playlists.len() + 1)
                } else {
                    self.playlist_draft.name.trim().to_owned()
                };
                let tracks = self.playlist_draft.tracks.clone();
                if let Some(active_id) = self.selected_playlist {
                    if let Some(existing) = self
                        .user_prefs
                        .playlists
                        .iter_mut()
                        .find(|playlist| playlist.id == active_id)
                    {
                        existing.name = name.clone();
                        existing.tracks = tracks.clone();
                        self.status_message = Some(format!("Playlist '{}' updated", existing.name));
                    } else {
                        let playlist = Playlist::new(name.clone(), tracks);
                        self.selected_playlist = Some(playlist.id);
                        self.user_prefs.playlists.push(playlist);
                        self.status_message = Some(format!("Playlist '{}' created", name));
                    }
                } else {
                    let playlist = Playlist::new(name.clone(), tracks);
                    self.selected_playlist = Some(playlist.id);
                    self.user_prefs.playlists.push(playlist);
                    self.status_message = Some(format!("Playlist '{}' created", name));
                }
                self.playlist_draft.name = name;
                self.save_preferences_task()
            }
            Message::PlaylistSelect(selection) => {
                self.selected_playlist = selection;
                Task::none()
            }
            Message::PlaylistDelete(id) => {
                let before = self.user_prefs.playlists.len();
                self.user_prefs
                    .playlists
                    .retain(|playlist| playlist.id != id);
                if before != self.user_prefs.playlists.len() {
                    if self.selected_playlist == Some(id) {
                        self.selected_playlist = None;
                    }
                    if let Some(queue) = &self.play_queue {
                        if matches!(queue.mode, QueueMode::Playlist(queue_id) if queue_id == id) {
                            self.play_queue = None;
                        }
                    }
                    self.status_message = Some("Playlist deleted".into());
                    self.save_preferences_task()
                } else {
                    Task::none()
                }
            }
            Message::PlaylistLoadToDraft(id) => {
                if let Some(playlist) = self
                    .user_prefs
                    .playlists
                    .iter()
                    .find(|playlist| playlist.id == id)
                    .cloned()
                {
                    self.playlist_draft.name = playlist.name.clone();
                    self.playlist_draft.tracks = playlist.tracks.clone();
                    self.selected_playlist = Some(id);
                    self.status_message = Some("Loaded playlist into draft".into());
                }
                Task::none()
            }
            Message::GenerateRandomPlaylist => {
                let mut rng = rand::rng();
                let selection: Vec<Uuid> = self
                    .library
                    .entries()
                    .iter()
                    .map(|entry| entry.id)
                    .choose_multiple(&mut rng, 50);
                self.playlist_draft.name = "Random 50".into();
                self.playlist_draft.tracks = selection;
                self.status_message = Some("Generated random playlist draft".into());
                Task::none()
            }
            Message::StartPlayback(id) => self.start_single_track(id),
            Message::PlayFavorites { shuffle } => self.play_favorites(shuffle),
            Message::PlayPlaylist { id, shuffle } => self.play_playlist(id, shuffle),
            Message::NextTrack => {
                if let Some(next_id) = self.advance_queue(true) {
                    self.play_track(next_id)
                } else {
                    Task::none()
                }
            }
            Message::PrevTrack => {
                if let Some(prev_id) = self.advance_queue(false) {
                    self.play_track(prev_id)
                } else {
                    Task::none()
                }
            }
            Message::PlayPressed => {
                if let Some(id) = self.selected_song {
                    self.start_single_track(id)
                } else {
                    self.error_message = Some("Select a MIDI file to play".into());
                    Task::none()
                }
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
                self.play_queue = None;
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
                            return self.schedule_tree_rebuild();
                        }
                        Err(err) => {
                            self.error_message = Some(format!("Failed to add MIDI file: {err:?}"));
                        }
                    }
                }
                Task::none()
            }
            Message::Tick => {
                let mut tasks = Vec::new();
                while let Ok(event) = self.player_events.try_recv() {
                    if let Some(task) = self.handle_player_event(event) {
                        tasks.push(task);
                    }
                }
                if tasks.is_empty() {
                    Task::none()
                } else {
                    Task::batch(tasks)
                }
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
            self.library_tabs(),
            self.library_view(),
            self.playlist_editor(),
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

    fn handle_player_event(&mut self, event: PlayerEvent) -> Option<Task<Message>> {
        match event {
            PlayerEvent::Started { total } => {
                self.playback_phase = PlaybackPhase::Playing;
                self.playback_progress = Some(PlaybackProgress {
                    elapsed: Duration::ZERO,
                    total,
                });
                self.status_message = Some("Playback started".into());
                None
            }
            PlayerEvent::Progress { elapsed, total } => {
                self.playback_progress = Some(PlaybackProgress { elapsed, total });
                None
            }
            PlayerEvent::Finished => {
                self.playback_phase = PlaybackPhase::Finished;
                self.current_sink = None;
                if let Some(next_id) = self.advance_queue(true) {
                    Some(self.play_track(next_id))
                } else {
                    self.status_message = Some("Playback finished".into());
                    None
                }
            }
            PlayerEvent::Stopped => {
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.status_message = Some("Playback stopped".into());
                self.current_sink = None;
                None
            }
            PlayerEvent::Error(message) => {
                self.error_message = Some(message);
                self.playback_phase = PlaybackPhase::Idle;
                self.playback_progress = None;
                self.current_sink = None;
                None
            }
        }
    }

    fn save_preferences_task(&self) -> Task<Message> {
        Task::perform(
            save_user_preferences(self.user_prefs.clone()),
            Message::PreferencesSaved,
        )
    }

    fn schedule_tree_rebuild(&mut self) -> Task<Message> {
        self.tree_loading = true;
        self.tree_cache.clear();
        self.folder_entries.clear();
        self.tree_request_id = self.tree_request_id.wrapping_add(1);
        let request_id = self.tree_request_id;
        let entries = self.library.entries().to_vec();
        Task::perform(compute_tree_data(entries), move |result| match result {
            Ok((tree, folders)) => Message::TreeDataLoaded {
                request_id,
                tree,
                folders,
            },
            Err(err) => Message::TreeDataFailed {
                request_id,
                error: err,
            },
        })
    }

    fn apply_tree_data(&mut self, tree: LibraryNode, folders: HashMap<String, Vec<Uuid>>) {
        self.library_tree = tree;
        self.folder_entries = folders;
        if self
            .selected_folder
            .as_ref()
            .map_or(true, |id| !self.folder_entries.contains_key(id))
        {
            self.selected_folder = Some("root".into());
        }
        self.refresh_tree_cache();
    }

    fn refresh_tree_cache(&mut self) {
        let mut items = Vec::new();
        collect_tree_items(&self.library_tree, 0, &self.expanded_folders, &mut items);
        self.tree_cache = items;
    }

    fn visible_entries(&self) -> Vec<&crate::midi::MidiEntry> {
        let query = self.search_query.trim().to_lowercase();

        let mut base: Vec<&crate::midi::MidiEntry> = match self.active_tab {
            LibraryTab::Tree => {
                if self.tree_loading {
                    Vec::new()
                } else {
                    let folder_id = self.selected_folder.as_deref().unwrap_or("root");
                    self.folder_entries
                        .get(folder_id)
                        .into_iter()
                        .flat_map(|ids| ids.iter())
                        .filter_map(|id| self.library.get(id))
                        .collect()
                }
            }
            LibraryTab::Favorites => self
                .user_prefs
                .favorites
                .iter()
                .filter_map(|id| self.library.get(id))
                .collect(),
        };

        if !query.is_empty() {
            base.retain(|entry| entry.name.to_lowercase().contains(&query));
        }

        base.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        base
    }

    fn start_single_track(&mut self, track_id: Uuid) -> Task<Message> {
        if self.library.get(&track_id).is_none() {
            self.error_message = Some("Selected track is not available".into());
            return Task::none();
        }
        self.queue_with_tracks(vec![track_id], track_id, QueueMode::Single, false);
        self.play_track(track_id)
    }

    fn play_favorites(&mut self, shuffle: bool) -> Task<Message> {
        let mut entries: Vec<_> = self
            .user_prefs
            .favorites
            .iter()
            .filter_map(|id| self.library.get(id))
            .collect();
        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let tracks: Vec<Uuid> = entries.iter().map(|entry| entry.id).collect();
        if tracks.is_empty() {
            self.error_message = Some("No favorites available to play".into());
            return Task::none();
        }
        let start_track = if shuffle {
            let mut rng = rng();
            *tracks.as_slice().choose(&mut rng).unwrap()
        } else {
            tracks[0]
        };
        if self.queue_with_tracks(tracks, start_track, QueueMode::Favorites, shuffle) {
            self.status_message = Some("Playing favorites".into());
            self.play_track(start_track)
        } else {
            Task::none()
        }
    }

    fn play_playlist(&mut self, playlist_id: Uuid, shuffle: bool) -> Task<Message> {
        let playlist = match self
            .user_prefs
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .cloned()
        {
            Some(playlist) => playlist,
            None => {
                self.error_message = Some("Playlist not found".into());
                return Task::none();
            }
        };

        let tracks: Vec<Uuid> = playlist
            .tracks
            .iter()
            .filter_map(|id| self.library.get(id).map(|entry| entry.id))
            .collect();

        if tracks.is_empty() {
            self.error_message = Some("Playlist has no playable tracks".into());
            return Task::none();
        }

        let start_track = if shuffle {
            let mut rng = rng();
            *tracks.as_slice().choose(&mut rng).unwrap()
        } else {
            tracks[0]
        };

        if self.queue_with_tracks(
            tracks,
            start_track,
            QueueMode::Playlist(playlist_id),
            shuffle,
        ) {
            self.status_message = Some(format!("Playing playlist '{}'", playlist.name));
            self.play_track(start_track)
        } else {
            Task::none()
        }
    }

    fn queue_with_tracks(
        &mut self,
        tracks: Vec<Uuid>,
        start_track: Uuid,
        mode: QueueMode,
        shuffle: bool,
    ) -> bool {
        if self.library.get(&start_track).is_none() {
            self.error_message = Some("Selected track is not available".into());
            return false;
        }

        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for id in tracks.into_iter() {
            if self.library.get(&id).is_some() && seen.insert(id) {
                ordered.push(id);
            }
        }

        if ordered.is_empty() {
            self.play_queue = None;
            return false;
        }

        if seen.insert(start_track) {
            ordered.insert(0, start_track);
        }

        if shuffle {
            let mut rng = rng();
            ordered.shuffle(&mut rng);
        }

        if let Some(pos) = ordered.iter().position(|id| *id == start_track) {
            ordered.swap(0, pos);
        } else {
            ordered.insert(0, start_track);
        }

        self.play_queue = Some(PlayQueue {
            tracks: ordered,
            index: 0,
            mode,
        });
        self.selected_song = Some(start_track);
        true
    }

    fn advance_queue(&mut self, forward: bool) -> Option<Uuid> {
        let queue = self.play_queue.as_mut()?;
        if queue.tracks.is_empty() {
            self.play_queue = None;
            return None;
        }
        if forward {
            if queue.index + 1 < queue.tracks.len() {
                queue.index += 1;
                let track = queue.tracks[queue.index];
                self.selected_song = Some(track);
                Some(track)
            } else {
                self.play_queue = None;
                self.status_message = Some("Queue finished".into());
                None
            }
        } else if queue.index > 0 {
            queue.index -= 1;
            let track = queue.tracks[queue.index];
            self.selected_song = Some(track);
            Some(track)
        } else {
            self.status_message = Some("Already at the beginning".into());
            None
        }
    }

    fn queue_label(&self, queue: &PlayQueue) -> String {
        let mode_label = match &queue.mode {
            QueueMode::Single => "Single".to_string(),
            QueueMode::Favorites => "Favorites".to_string(),
            QueueMode::Playlist(id) => self
                .user_prefs
                .playlists
                .iter()
                .find(|playlist| &playlist.id == id)
                .map(|playlist| playlist.name.clone())
                .unwrap_or_else(|| "Playlist".into()),
        };
        format!("{}: {}/{}", mode_label, queue.index + 1, queue.tracks.len())
    }

    fn current_track_label(&self) -> String {
        if let Some(id) = self.selected_song {
            if let Some(entry) = self.library.get(&id) {
                return format!("Now: {}", entry.name);
            }
        }
        "Now: --".into()
    }

    fn play_track(&mut self, track_id: Uuid) -> Task<Message> {
        if self.is_preparing_playback {
            self.status_message = Some("Already preparing a track".into());
            return Task::none();
        }

        let entry = match self.library.get(&track_id).cloned() {
            Some(entry) => entry,
            None => {
                self.error_message = Some("Track not available".into());
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

        self.is_preparing_playback = true;
        self.playback_phase = PlaybackPhase::Preparing;
        self.status_message = Some(format!("Preparing {}", entry.name));
        self.selected_song = Some(track_id);
        let path = entry.path.clone();

        Task::perform(
            prepare_playback(path, device_id, self.device_manager.clone()),
            Message::PlaybackPrepared,
        )
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

    fn library_tabs(&self) -> Element<'_, Message> {
        let mut tree_button = button(text("Tree").shaping(Shaping::Advanced));
        if self.active_tab == LibraryTab::Tree {
            tree_button = tree_button.style(iced::widget::button::primary);
        } else {
            tree_button = tree_button.style(iced::widget::button::secondary);
        }
        let tree_button = tree_button.on_press(Message::SwitchTab(LibraryTab::Tree));

        let mut favorites_button = button(text("Favorites").shaping(Shaping::Advanced));
        if self.active_tab == LibraryTab::Favorites {
            favorites_button = favorites_button.style(iced::widget::button::primary);
        } else {
            favorites_button = favorites_button.style(iced::widget::button::secondary);
        }
        let favorites_button = favorites_button.on_press(Message::SwitchTab(LibraryTab::Favorites));

        row![tree_button, favorites_button].spacing(12).into()
    }

    fn playback_controls(&self) -> Element<'_, Message> {
        let prev_button = button(text("⏮").shaping(Shaping::Advanced))
            .on_press(Message::PrevTrack)
            .style(iced::widget::button::secondary);

        let play_button = button("Play Selected")
            .on_press(Message::PlayPressed)
            .style(iced::widget::button::primary);

        let stop_button = button("Stop")
            .on_press(Message::StopPressed)
            .style(iced::widget::button::secondary);

        let next_button = button(text("⏭").shaping(Shaping::Advanced))
            .on_press(Message::NextTrack)
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
        .shaping(Shaping::Advanced)
        .size(16)
        .width(Length::Fill);

        let queue_text = if let Some(queue) = &self.play_queue {
            let label = self.queue_label(queue);
            text(label).shaping(Shaping::Advanced)
        } else {
            text("Queue: none").shaping(Shaping::Advanced)
        };

        let current_text = text(self.current_track_label()).shaping(Shaping::Advanced);

        row![
            prev_button,
            play_button,
            stop_button,
            next_button,
            status_text,
            queue_text,
            current_text
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .into()
    }

    fn library_view(&self) -> Element<'_, Message> {
        let search = text_input("Search MIDI files...", &self.search_query)
            .on_input(Message::SearchChanged)
            .padding(8);

        let entries = self.visible_entries();
        let list = scrollable(self.entry_column(entries)).height(Length::Fill);

        match self.active_tab {
            LibraryTab::Tree => {
                let tree = scrollable(self.tree_panel()).height(Length::Fill);
                column![
                    search,
                    row![
                        container(tree)
                            .width(Length::Fixed(260.0))
                            .height(Length::Fill),
                        container(list).width(Length::Fill).height(Length::Fill),
                    ]
                    .spacing(16)
                ]
                .spacing(12)
                .height(Length::Fill)
                .into()
            }
            LibraryTab::Favorites => {
                let play_row = row![
                    button("Play Favorites")
                        .on_press(Message::PlayFavorites { shuffle: false })
                        .style(iced::widget::button::primary),
                    button("Shuffle Favorites")
                        .on_press(Message::PlayFavorites { shuffle: true })
                        .style(iced::widget::button::secondary)
                ]
                .spacing(12);

                column![search, play_row, list]
                    .spacing(12)
                    .height(Length::Fill)
                    .into()
            }
        }
    }

    fn entry_column<'a>(&'a self, entries: Vec<&'a crate::midi::MidiEntry>) -> Column<'a, Message> {
        let mut column = Column::new().spacing(6);
        if entries.is_empty() {
            column = column
                .push(text("No MIDI files match the current filters").shaping(Shaping::Advanced));
        } else {
            for entry in entries {
                column = column.push(self.entry_row(entry));
            }
        }
        column
    }

    fn entry_row(&self, entry: &crate::midi::MidiEntry) -> Element<'_, Message> {
        let is_selected = Some(entry.id) == self.selected_song;
        let display_name = if matches!(entry.origin, crate::midi::MidiOrigin::Local) {
            format!("{} (Local)", entry.name)
        } else {
            entry.name.clone()
        };

        let mut select_button = button(text(display_name).shaping(Shaping::Advanced))
            .on_press(Message::SongSelected(entry.id));
        if is_selected {
            select_button = select_button.style(iced::widget::button::success);
        } else {
            select_button = select_button.style(iced::widget::button::secondary);
        }

        let play_button = button(text("▶").shaping(Shaping::Advanced))
            .style(iced::widget::button::primary)
            .on_press(Message::StartPlayback(entry.id));

        let current_rating = self.user_prefs.ratings.get(&entry.id).copied().unwrap_or(0);
        let mut stars_row = row![];
        for star in 1..=5u8 {
            let symbol = if current_rating >= star { "★" } else { "☆" };
            let target = if current_rating == star { 0 } else { star };
            let star_button = button(text(symbol).shaping(Shaping::Advanced))
                .style(iced::widget::button::secondary)
                .on_press(Message::SetRating(entry.id, target));
            stars_row = stars_row.push(star_button);
        }
        stars_row = stars_row.spacing(4);

        let favorite_symbol = if self.user_prefs.favorites.contains(&entry.id) {
            "♥"
        } else {
            "♡"
        };
        let favorite_button = button(text(favorite_symbol).shaping(Shaping::Advanced))
            .style(iced::widget::button::secondary)
            .on_press(Message::ToggleFavorite(entry.id));

        let add_button = button(text("＋").shaping(Shaping::Advanced))
            .style(iced::widget::button::secondary)
            .on_press(Message::PlaylistDraftAdd(entry.id));

        row![
            select_button,
            play_button,
            stars_row,
            favorite_button,
            add_button,
        ]
        .spacing(12)
        .into()
    }

    fn status_banner(&self) -> Element<'_, Message> {
        if let Some(error) = &self.error_message {
            return row![
                text(error)
                    .shaping(Shaping::Advanced)
                    .size(16)
                    .color(Color::from_rgb(0.9, 0.4, 0.4)),
                button("Dismiss")
                    .on_press(Message::DismissStatus)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8)
            .into();
        }

        if let Some(status) = &self.status_message {
            return row![
                text(status)
                    .shaping(Shaping::Advanced)
                    .size(16)
                    .color(Color::from_rgb(0.4, 0.9, 0.4)),
                button("Dismiss")
                    .on_press(Message::DismissStatus)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8)
            .into();
        }

        column![].into()
    }

    fn tree_panel(&self) -> Column<'_, Message> {
        let mut column = Column::new().spacing(4);

        if self.tree_loading && self.tree_cache.is_empty() {
            return column.push(text("Loading tree...").shaping(Shaping::Advanced));
        }

        for item in &self.tree_cache {
            let indent = "  ".repeat(item.depth);
            let indicator = if item.has_children {
                if item.is_expanded { "▼" } else { "▶" }
            } else {
                "•"
            };
            let label = format!("{indent}{indicator} {}", item.name);
            let mut button = button(text(label).shaping(Shaping::Advanced));
            if item.has_children {
                button = button.on_press(Message::ToggleFolder(item.id.clone()));
            } else {
                button = button.on_press(Message::SelectFolder(item.id.clone()));
            }
            if self.selected_folder.as_deref() == Some(item.id.as_str()) {
                button = button.style(iced::widget::button::success);
            } else {
                button = button.style(iced::widget::button::secondary);
            }
            column = column.push(button);
        }

        column
    }

    fn playlist_editor(&self) -> Element<'_, Message> {
        let name_input = text_input("Playlist name", &self.playlist_draft.name)
            .on_input(Message::PlaylistDraftNameChanged)
            .padding(8);

        let save_button = button("Save Playlist")
            .on_press(Message::PlaylistDraftSave)
            .style(iced::widget::button::primary);

        let clear_button = button("Clear Draft")
            .on_press(Message::PlaylistDraftClear)
            .style(iced::widget::button::secondary);

        let random_button = button("Random 50")
            .on_press(Message::GenerateRandomPlaylist)
            .style(iced::widget::button::secondary);

        let controls = row![name_input, save_button, clear_button, random_button].spacing(12);

        let playlist_choices: Vec<PlaylistChoice> = self
            .user_prefs
            .playlists
            .iter()
            .map(PlaylistChoice::from)
            .collect();

        let selected_choice = self.selected_playlist.and_then(|id| {
            playlist_choices
                .iter()
                .find(|choice| choice.id == id)
                .cloned()
        });

        let playlist_pick = pick_list(
            playlist_choices.clone(),
            selected_choice,
            |choice: PlaylistChoice| Message::PlaylistSelect(Some(choice.id)),
        )
        .placeholder("Choose playlist");

        let load_button = if let Some(id) = self.selected_playlist {
            button("Load into Draft")
                .on_press(Message::PlaylistLoadToDraft(id))
                .style(iced::widget::button::secondary)
        } else {
            button("Load into Draft").style(iced::widget::button::secondary)
        };

        let delete_button = if let Some(id) = self.selected_playlist {
            button("Delete Playlist")
                .on_press(Message::PlaylistDelete(id))
                .style(iced::widget::button::danger)
        } else {
            button("Delete Playlist").style(iced::widget::button::danger)
        };

        let clear_selection_button = if self.selected_playlist.is_some() {
            button("Clear Selection")
                .on_press(Message::PlaylistSelect(None))
                .style(iced::widget::button::secondary)
        } else {
            button("Clear Selection").style(iced::widget::button::secondary)
        };

        let selection_row = row![
            playlist_pick,
            load_button,
            delete_button,
            clear_selection_button,
        ]
        .spacing(12);

        let playlist_play_row: Element<'_, Message> = if let Some(id) = self.selected_playlist {
            row![
                button("Play Selected")
                    .on_press(Message::PlayPlaylist { id, shuffle: false })
                    .style(iced::widget::button::primary),
                button("Shuffle Selected")
                    .on_press(Message::PlayPlaylist { id, shuffle: true })
                    .style(iced::widget::button::secondary)
            ]
            .spacing(12)
            .into()
        } else {
            text("Select a playlist to play")
                .shaping(Shaping::Advanced)
                .into()
        };

        let mut tracks_column = Column::new().spacing(4);
        for (index, track_id) in self.playlist_draft.tracks.iter().cloned().enumerate() {
            if let Some(entry) = self.library.get(&track_id) {
                let label = text(entry.name.clone()).shaping(Shaping::Advanced);
                let remove_button = button("Remove")
                    .on_press(Message::PlaylistDraftRemove(index))
                    .style(iced::widget::button::secondary);
                tracks_column = tracks_column.push(row![label, remove_button].spacing(12));
            }
        }
        if self.playlist_draft.tracks.is_empty() {
            tracks_column =
                tracks_column.push(text("Playlist draft is empty").shaping(Shaping::Advanced));
        }

        let track_list = scrollable(tracks_column).height(Length::Fixed(200.0));

        column![controls, selection_row, playlist_play_row, track_list]
            .spacing(12)
            .into()
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

async fn load_user_preferences() -> AsyncResult<UserPreferences> {
    tokio::task::spawn_blocking(|| {
        let path = std::path::Path::new(USER_DATA_FILE);
        if !path.exists() {
            return Ok(UserPreferences::default());
        }
        let data = std::fs::read_to_string(path)
            .map_err(|err| format!("failed to read preferences: {err}"))?;
        serde_json::from_str(&data).map_err(|err| format!("failed to parse preferences: {err}"))
    })
    .await
    .map_err(|err| format!("failed to join preferences task: {err:?}"))?
}

async fn save_user_preferences(prefs: UserPreferences) -> AsyncResult<()> {
    tokio::task::spawn_blocking(move || {
        let path = std::path::Path::new(USER_DATA_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create data directory: {err}"))?;
        }
        let serialized = serde_json::to_string_pretty(&prefs)
            .map_err(|err| format!("failed to serialize preferences: {err}"))?;
        std::fs::write(path, serialized)
            .map_err(|err| format!("failed to write preferences: {err}"))
    })
    .await
    .map_err(|err| format!("failed to join save task: {err:?}"))?
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

fn collect_tree_items(
    node: &LibraryNode,
    depth: usize,
    expanded: &HashSet<String>,
    items: &mut Vec<TreeItem>,
) {
    for child in node.children.values() {
        collect_tree_items_inner(child, depth, expanded, items);
    }
}

fn collect_tree_items_inner(
    node: &LibraryNode,
    depth: usize,
    expanded: &HashSet<String>,
    items: &mut Vec<TreeItem>,
) {
    let has_children = !node.children.is_empty();
    let is_expanded = expanded.contains(&node.id);
    items.push(TreeItem {
        id: node.id.clone(),
        name: node.name.clone(),
        depth,
        has_children,
        is_expanded,
    });
    if has_children && is_expanded {
        for child in node.children.values() {
            collect_tree_items_inner(child, depth + 1, expanded, items);
        }
    }
}

fn build_window_icon() -> Option<window::Icon> {
    let size: u32 = 24;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let xf = x as f32 / size as f32;
            let yf = y as f32 / size as f32;
            let r = (200.0 - 80.0 * xf) as u8;
            let g = (120.0 + 100.0 * yf) as u8;
            let b = 220u8;
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    window::icon::from_rgba(rgba, size, size).ok()
}

pub fn run() -> iced::Result {
    let icon = build_window_icon();
    let window_settings = window::Settings {
        icon,
        ..window::Settings::default()
    };
    application("MIDI Piano Player", update, view)
        .subscription(subscription)
        .theme(theme)
        .window(window_settings)
        .font(NOTO_SANS_SC)
        .default_font(DEFAULT_FONT)
        .executor::<executor::Default>()
        .run_with(MidiPianoApp::init)
}

async fn compute_tree_data(
    entries: Vec<crate::midi::MidiEntry>,
) -> AsyncResult<(LibraryNode, HashMap<String, Vec<Uuid>>)> {
    tokio::task::spawn_blocking(move || build_tree_data_owned(entries))
        .await
        .map_err(|err| format!("tree rebuild task failed: {err:?}"))
}

fn build_tree_data_owned(
    entries: Vec<crate::midi::MidiEntry>,
) -> (LibraryNode, HashMap<String, Vec<Uuid>>) {
    let mut root = LibraryNode::new("root".into(), "Library".into());
    let mut folders: HashMap<String, Vec<Uuid>> = HashMap::new();
    folders.insert("root".into(), Vec::new());

    let mut local_ids: Vec<Uuid> = Vec::new();

    for entry in entries {
        match entry.origin {
            crate::midi::MidiOrigin::Asset => {
                folders
                    .entry("root".into())
                    .or_insert_with(Vec::new)
                    .push(entry.id);

                if let Some(segments) = entry.library_path.clone() {
                    if segments.is_empty() {
                        continue;
                    }

                    let mut node = &mut root;
                    let mut path_builder = String::new();
                    for (index, segment) in segments.iter().enumerate() {
                        if index > 0 {
                            path_builder.push('/');
                        }
                        path_builder.push_str(segment);
                        let node_id = format!("asset:{}", path_builder);
                        node = node.ensure_child(node_id.clone(), segment.clone());
                        folders.entry(node_id.clone()).or_insert_with(Vec::new);
                    }

                    let leaf_id = format!("asset:{}", path_builder);
                    folders
                        .entry(leaf_id)
                        .or_insert_with(Vec::new)
                        .push(entry.id);
                }
            }
            crate::midi::MidiOrigin::Local => {
                local_ids.push(entry.id);
            }
        }
    }

    if !local_ids.is_empty() {
        folders
            .entry("root".into())
            .or_insert_with(Vec::new)
            .extend(local_ids.iter().copied());
        let local_id = "local".to_string();
        root.ensure_child(local_id.clone(), "Local".into());
        folders
            .entry(local_id)
            .or_insert_with(Vec::new)
            .extend(local_ids);
    }

    (root, folders)
}
