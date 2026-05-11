//! Resolve paths to bundled audio/video tools (ffmpeg / ffprobe / rubberband) and
//! build `std::process::Command`s for them.
//!
//! Distribution archives place these binaries next to the `dubsync` binary, so we
//! look there first. If they aren't there (e.g. during local `cargo run` against a
//! Homebrew install), we fall back to a bare name and let the OS resolve it via PATH.
//!
//! Use the `*_command()` helpers instead of constructing `Command::new` directly —
//! they also stamp `CREATE_NO_WINDOW` on Windows so the GUI doesn't flash a cmd
//! console for every subprocess spawn.

use std::path::PathBuf;
use std::process::Command;

pub fn ffmpeg() -> PathBuf {
    locate("ffmpeg")
}

pub fn ffprobe() -> PathBuf {
    locate("ffprobe")
}

pub fn rubberband() -> PathBuf {
    locate("rubberband")
}

/// Build a `Command` for ffmpeg with the right resolved path and the no-flash
/// flag on Windows.
pub fn ffmpeg_command() -> Command {
    no_window(Command::new(ffmpeg()))
}

/// Build a `Command` for ffprobe with the right resolved path and the no-flash
/// flag on Windows.
pub fn ffprobe_command() -> Command {
    no_window(Command::new(ffprobe()))
}

/// Build a `Command` for rubberband — used by the optional gap-fill (`--smooth-gaps`)
/// path to time-stretch neighbour audio over silence gaps. Resolved next-to-exe first
/// (release archives bundle it alongside ffmpeg/ffprobe), then PATH (dev installs via
/// `brew install rubberband` / `apt install rubberband-cli`).
pub fn rubberband_command() -> Command {
    no_window(Command::new(rubberband()))
}

/// Stamp `CREATE_NO_WINDOW` on Windows so spawning a console-subsystem child
/// (which ffmpeg/ffprobe both are) from the windowed `dubsync-gui` binary
/// doesn't pop up a fresh `cmd.exe` window. No-op on non-Windows targets.
#[cfg(windows)]
fn no_window(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW = 0x08000000 (winbase.h). Hard-coded so we don't need
    // a Windows-only dep just for one constant.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn no_window(cmd: Command) -> Command {
    cmd
}

fn locate(name: &str) -> PathBuf {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&exe_name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}
