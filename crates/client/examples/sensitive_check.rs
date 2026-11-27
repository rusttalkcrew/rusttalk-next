//! `make sensitive-check DIR=<folder>`: prints the picture check's scores for each
//! picture in a local folder. Dev only; it reads the folder and sends nothing.

fn main() -> std::process::ExitCode {
    rusttalk_client::sensitive_check(std::env::args_os().nth(1))
}
