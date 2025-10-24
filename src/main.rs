mod app;
mod devices;
mod midi;

fn main() -> iced::Result {
    if env_logger::try_init().is_err() {
        eprintln!("Logger already initialized");
    }
    app::run()
}
