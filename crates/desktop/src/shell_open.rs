//! Hand a file to the desktop: open it with its program, or show it selected in the
//! file manager. Explorer on Windows; `xdg-open` and the freedesktop FileManager1
//! interface on Linux.

use std::path::Path;
use std::process::{Command, Stdio};

/// Open `path` with its default program, or (`reveal`) show it selected in the file
/// manager. Failures are silent: there is nothing useful the shell could do about them.
pub(crate) fn open_path(path: &Path, reveal: bool) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("explorer.exe");
        if reveal {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(path.as_os_str());
            cmd.arg(arg);
        } else {
            cmd.arg(path);
        }
        let _ = cmd.spawn();
    }
    #[cfg(not(windows))]
    {
        if reveal && show_in_file_manager(path) {
            return;
        }
        // Without a FileManager1 service, open the folder that holds the file.
        let target = match (reveal, path.parent()) {
            (true, Some(dir)) => dir,
            _ => path,
        };
        let _ = quiet(Command::new("xdg-open").arg(target)).spawn();
    }
}

/// Open a text file in the desktop's editor for it.
pub(crate) fn edit_text_file(path: &Path) {
    let program = if cfg!(windows) { "notepad" } else { "xdg-open" };
    let _ = quiet(Command::new(program).arg(path)).spawn();
}

/// Ask the file manager (over D-Bus) to show `path` selected. False when no file
/// manager answers.
#[cfg(not(windows))]
fn show_in_file_manager(path: &Path) -> bool {
    let Ok(uri) = url::Url::from_file_path(path) else {
        return false;
    };
    quiet(Command::new("dbus-send").args([
        "--session",
        "--print-reply",
        "--dest=org.freedesktop.FileManager1",
        "--type=method_call",
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1.ShowItems",
    ]))
    .arg(format!("array:string:{uri}"))
    .arg("string:")
    .status()
    .is_ok_and(|s| s.success())
}

/// No inherited console streams: the helper's chatter must not reach ours.
fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
}
