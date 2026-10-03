//! Manual capture probe. Its output includes physical key usages; do not save sensitive typing.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use glide_platform::{InputBackend, InputSink};
    use glide_platform_win::{enable_per_monitor_v2, WindowsInput};
    enable_per_monitor_v2()?;
    let input = WindowsInput::new()?;
    for monitor in input.monitors()? {
        println!("{monitor:?}");
    }
    println!("Permissions: {:?}", input.permissions());
    let (sink, events) = InputSink::bounded(4096).map_err(|_| "invalid capture capacity")?;
    input.start_capture(sink)?;
    println!("Local capture active. Ctrl+C exits. Injected events are labelled and must never be forwarded.");
    for event in events {
        println!("{event:?}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This probe requires Windows.");
}
