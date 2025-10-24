mod app;
mod devices;
mod midi;

use app::MidiPianoApp;
use iced::Application;

fn main() -> iced::Result {
    if env_logger::try_init().is_err() {
        eprintln!("Logger already initialized");
    }
    MidiPianoApp::run(iced::Settings::default())
}
