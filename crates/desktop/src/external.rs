//! Links from other apps: opening them in the running browser, and registering the
//! browser with Windows so it can be picked as the default.
//!
//! **One window.** When Windows (or anything else) starts `browser.exe <url>` while a
//! browser is already running, the new process hands the URL to the running one over a
//! named pipe and exits; the running one opens it as a new tab and comes to the front
//! (`UserEvent::OpenExternal`). The pipe's name comes from the executable's path, so a
//! debug build and an installed browser never take each other's links. Only addresses
//! cross the pipe — never `:` commands — so another program can't run `:te` through it.
//!
//! **Default browser.** Windows won't let a program make itself the default; it has to
//! be registered, then chosen in Settings. `:default` writes that registration (current
//! user only, no admin) and opens the Default apps page at it; `:default remove` undoes it.
use std::io::{Read, Write};

use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

/// The most a handed-off address may be; anything longer is dropped.
const MAX_HANDOFF: u64 = 8 * 1024;

/// The pipe a running browser listens on: one per executable path, and a separate one
/// for `--scratch` runs, which stay apart from the real browser in every way.
fn pipe_name(scratch: bool) -> String {
    use std::hash::{Hash, Hasher};
    let exe = std::env::current_exe().unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    exe.to_string_lossy().to_lowercase().hash(&mut h);
    let kind = if scratch { "-scratch" } else { "" };
    format!(r"\\.\pipe\browser-open{kind}-{:016x}", h.finish())
}

/// Whether `target` may be opened on another program's behalf: an address, not a
/// shell command.
pub(crate) fn acceptable(target: &str) -> bool {
    !target.trim_start().starts_with(':')
}

/// Give `target` (or, with none, just "come to the front") to an already running
/// browser. True when one took it — this process then has nothing left to do.
pub(crate) fn hand_off(target: Option<&str>, scratch: bool) -> bool {
    if target.is_some_and(|t| !acceptable(t)) {
        return false;
    }
    let name = pipe_name(scratch);
    for _ in 0..20 {
        match std::fs::OpenOptions::new().write(true).open(&name) {
            Ok(mut pipe) => {
                // Let the running browser take the foreground: Windows only allows
                // that to a process the foreground one (this one) hands it to.
                #[cfg(windows)]
                unsafe {
                    let _ =
                        windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX);
                }
                return pipe.write_all(target.unwrap_or("").as_bytes()).is_ok();
            }
            // ERROR_PIPE_BUSY: it's between two hand-offs; try again in a moment.
            Err(e) if e.raw_os_error() == Some(231) => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            // No browser is listening: this one becomes it.
            Err(_) => return false,
        }
    }
    false
}

/// Take hand-offs from later launches, for the rest of this process's life.
#[cfg(windows)]
pub(crate) fn listen(proxy: EventLoopProxy<UserEvent>, scratch: bool) {
    use std::os::windows::io::FromRawHandle;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let name = HSTRING::from(pipe_name(scratch));
    std::thread::spawn(move || loop {
        // FIRST_PIPE_INSTANCE: if another process already owns this name, don't share it.
        let pipe = unsafe {
            CreateNamedPipeW(
                &name,
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                4096,
                0,
                None,
            )
        };
        if pipe.is_invalid() {
            return;
        }
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        let connected = connected.is_ok()
            || windows::core::Error::from_thread().code() == ERROR_PIPE_CONNECTED.to_hresult();
        // Owning the handle as a File closes it (and the connection) when done.
        let file = unsafe { std::fs::File::from_raw_handle(pipe.0) };
        if !connected {
            continue;
        }
        let mut target = String::new();
        if file.take(MAX_HANDOFF).read_to_string(&mut target).is_ok()
            && proxy
                .send_event(UserEvent::OpenExternal(target.trim().to_string()))
                .is_err()
        {
            return; // the browser is shutting down
        }
    });
}

#[cfg(not(windows))]
pub(crate) fn listen(_proxy: EventLoopProxy<UserEvent>, _scratch: bool) {}

/// The registry names this build registers under. A debug build gets its own, so
/// registering it never replaces an installed browser's entry.
fn registration() -> (&'static str, &'static str) {
    if cfg!(debug_assertions) {
        ("kayf.browser.dev", "browser (dev)")
    } else {
        ("kayf.browser", "browser")
    }
}

/// `:default` — register this browser with Windows as a web browser (current user).
#[cfg(windows)]
pub(crate) fn register() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (key, name) = registration();
    let quoted = format!("\"{}\"", exe.display());
    let class = format!(r"Software\Classes\{key}.url");
    let client = format!(r"Software\Clients\StartMenuInternet\{key}");
    let caps = format!(r"{client}\Capabilities");
    let icon = format!("{quoted},0");
    for (sub, value, data) in [
        // The URL handler links open with.
        (class.clone(), None, format!("{name} URL")),
        (class.clone(), Some("URL Protocol"), String::new()),
        (format!(r"{class}\DefaultIcon"), None, icon.clone()),
        // The name and icon Settings shows for the handler (else just "browser.exe").
        (
            format!(r"{class}\Application"),
            Some("ApplicationName"),
            name.to_string(),
        ),
        (
            format!(r"{class}\Application"),
            Some("ApplicationIcon"),
            icon.clone(),
        ),
        (
            format!(r"{class}\shell\open\command"),
            None,
            format!("{quoted} \"%1\""),
        ),
        // The browser entry Settings lists.
        (client.clone(), None, name.to_string()),
        (format!(r"{client}\DefaultIcon"), None, icon.clone()),
        (
            format!(r"{client}\shell\open\command"),
            None,
            quoted.clone(),
        ),
        (caps.clone(), Some("ApplicationName"), name.to_string()),
        (
            caps.clone(),
            Some("ApplicationDescription"),
            "A keyboard-driven, modal browser".into(),
        ),
        (caps.clone(), Some("ApplicationIcon"), icon),
        (
            format!(r"{caps}\URLAssociations"),
            Some("http"),
            format!("{key}.url"),
        ),
        (
            format!(r"{caps}\URLAssociations"),
            Some("https"),
            format!("{key}.url"),
        ),
        (
            format!(r"{caps}\StartMenu"),
            Some("StartMenuInternet"),
            key.to_string(),
        ),
        (
            r"Software\RegisteredApplications".into(),
            Some(name),
            caps.clone(),
        ),
    ] {
        set_string(&sub, value, &data)?;
    }
    associations_changed();
    open_default_apps(name);
    Ok(format!(
        "registered {name} with Windows — pick it under Web browser in the Settings window"
    ))
}

/// `:default remove` — undo [`register`].
#[cfg(windows)]
pub(crate) fn unregister() -> Result<String, String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegDeleteTreeW, HKEY_CURRENT_USER};
    let (key, name) = registration();
    unsafe {
        let _ = RegDeleteTreeW(
            HKEY_CURRENT_USER,
            &HSTRING::from(format!(r"Software\Classes\{key}.url")),
        );
        let _ = RegDeleteTreeW(
            HKEY_CURRENT_USER,
            &HSTRING::from(format!(r"Software\Clients\StartMenuInternet\{key}")),
        );
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(r"Software\RegisteredApplications"),
            &HSTRING::from(name),
        );
    }
    associations_changed();
    Ok(format!("{name} is no longer registered as a browser"))
}

#[cfg(not(windows))]
pub(crate) fn register() -> Result<String, String> {
    Err("only Windows has a default-browser setting to register with".into())
}

#[cfg(not(windows))]
pub(crate) fn unregister() -> Result<String, String> {
    register()
}

/// Write a string value (`None` = the key's default value) under HKCU.
#[cfg(windows)]
fn set_string(sub: &str, value: Option<&str>, data: &str) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    let wide: Vec<u16> = data.encode_utf16().chain(Some(0)).collect();
    let value = value.map(HSTRING::from);
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(sub),
            value
                .as_ref()
                .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("couldn't write the registry ({sub}): {status:?}"))
    }
}

/// Tell Windows its link associations changed, so Settings shows the new entry.
#[cfg(windows)]
fn associations_changed() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Open Settings → Default apps at this browser's entry. Through the shell, not
/// `explorer.exe <uri>`: Explorer ignores this deep link and opens Documents instead.
#[cfg(windows)]
fn open_default_apps(name: &str) {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let uri = format!(
        "ms-settings:defaultapps?registeredAppUser={}",
        url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
    );
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(uri),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_never_cross_the_pipe() {
        assert!(acceptable("https://github.com/kayfgit/browser"));
        assert!(acceptable("youtube.com"));
        assert!(!acceptable(":te rm -rf"));
        assert!(!acceptable("  :ai hi"));
        assert!(
            !hand_off(Some(":te whoami"), true),
            "a command is never handed off"
        );
    }

    #[test]
    fn each_executable_has_its_own_pipe() {
        let name = pipe_name(false);
        assert!(name.starts_with(r"\\.\pipe\browser-open-"));
        assert_eq!(name, pipe_name(false), "stable within a process");
        assert_ne!(
            name,
            pipe_name(true),
            "scratch runs never take the real one's links"
        );
    }
}
