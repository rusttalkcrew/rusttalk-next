#[cfg(target_os = "linux")]
mod linux;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    match linux::run() {
        Ok(()) => return std::process::ExitCode::SUCCESS,
        Err(error) => eprintln!("PipeWire app audio: {error:#}"),
    }
    #[cfg(not(target_os = "linux"))]
    eprintln!("PipeWire app audio is only supported on Linux");
    std::process::ExitCode::FAILURE
}
