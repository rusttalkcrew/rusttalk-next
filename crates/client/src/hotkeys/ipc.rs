//! `rusttalk --toggle-mute` and the other shortcut commands, for desktops without the
//! global shortcuts portal: bind a key to the command in the desktop's own settings
//! (Hyprland: `bind = , F13, exec, rusttalk --toggle-mute`). The command hands the
//! action to the running app over a socket in the runtime folder that only this
//! person can open.

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::Once,
    thread,
};

use anyhow::{Context, Result};

use super::{command, deliver};

/// One socket per profile, so dev profiles don't take each other's commands.
fn path() -> Option<PathBuf> {
    let name = match env::var("RUSTTALK_PROFILE") {
        Ok(profile) => format!("rusttalk-dev-{profile}.sock"),
        Err(_) => "rusttalk.sock".into(),
    };
    Some(PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?).join(name))
}

/// Send a command (`toggle-mute`) to the running RustTalk.
pub fn send(message: &str) -> Result<()> {
    let path = path().context("XDG_RUNTIME_DIR is not set")?;
    let mut stream = UnixStream::connect(&path).context("RustTalk isn't running")?;
    stream.write_all(format!("{message}\n").as_bytes())?;
    Ok(())
}

/// Take commands for as long as the app runs. Once per process: the home screen may
/// be built again after signing out and in.
pub fn listen() {
    static LISTEN: Once = Once::new();
    LISTEN.call_once(|| {
        if let Err(error) = start() {
            tracing::warn!(error = %format_args!("{error:#}"), "shortcut commands unavailable");
        }
    });
}

fn start() -> Result<()> {
    let path = path().context("XDG_RUNTIME_DIR is not set")?;
    if UnixStream::connect(&path).is_ok() {
        anyhow::bail!("another RustTalk already takes them");
    }
    // Left behind by a RustTalk that didn't exit cleanly.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).context("open the socket")?;
    // The runtime folder is private already; this keeps the socket private elsewhere too.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).context("protect the socket")?;
    thread::Builder::new()
        .name("rusttalk-commands".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut line = String::new();
                // One short line per connection; anything longer is not a command.
                if BufReader::new(stream.take(64))
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                match command(line.trim()) {
                    Some((action, pressed)) => deliver(action, pressed),
                    None => tracing::warn!("unknown shortcut command"),
                }
            }
        })?;
    Ok(())
}
