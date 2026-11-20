#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod screen;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    match linux::run() {
        Ok(()) => return std::process::ExitCode::SUCCESS,
        Err(error) => eprintln!("PipeWire capture: {error:#}"),
    }
    #[cfg(not(target_os = "linux"))]
    eprintln!("PipeWire capture is only supported on Linux");
    std::process::ExitCode::FAILURE
}
