//! Updates: check GitHub for a newer release and install it.
//!
//! The installer is per-user (no administrator rights), so updating is silent. In the
//! default `auto` mode the browser checks at launch and every few hours; a newer release
//! is downloaded and checked against its published SHA-256 in the background, then
//! installed by a hidden helper once the browser quits, so the next launch is the new
//! version. `:update` checks now and shows the release notes; `:update install` installs
//! right away and restarts. `notify` only shows a notice, `off` doesn't check.
//!
//! Copies installed per machine (into Program Files, by 0.3.0 and earlier) move to the
//! per-user install on their next `:update install`: Windows asks once to remove the old
//! copy. The web engine itself, WebView2, is kept up to date by Windows.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{App, UserEvent};

const RELEASES_API: &str = "https://api.github.com/repos/kayfgit/browser/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/kayfgit/browser/releases";
const MSI_ASSET: &str = "browser-x86_64-pc-windows-msvc.msi";
/// How often the browser checks while it runs (and at launch, if longer ago).
pub(crate) const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// The address of the tab that shows a new release's notes.
const UPDATE_URL: &str = "browser://update";

/// A release on GitHub, as far as updating needs it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Release {
    /// Without the tag's `v`.
    pub(crate) version: String,
    /// Its changelog section (markdown).
    pub(crate) notes: String,
    msi_url: String,
    checksum_url: String,
}

/// What the browser does about updates (`:update auto|notify|off`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Download in the background and install on quit.
    Auto,
    /// Only say that there's a new version.
    Notify,
    /// Don't check.
    Off,
}

/// An update downloaded and verified, waiting to be installed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Staged {
    pub(crate) version: String,
    msi: PathBuf,
}

/// The version this build is. A debug build can pretend to be another with
/// `BROWSER_UPDATE_AS`, to try the check against real releases.
fn current_version() -> String {
    if cfg!(debug_assertions) {
        if let Ok(v) = std::env::var("BROWSER_UPDATE_AS") {
            return v;
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// Where to look for the latest release. `BROWSER_UPDATE_FEED` points at another URL
/// answering in GitHub's format, to test updating against a local feed.
fn feed_url() -> String {
    std::env::var("BROWSER_UPDATE_FEED").unwrap_or_else(|_| RELEASES_API.to_string())
}

/// Read GitHub's "latest release" JSON.
fn parse_release(json: &serde_json::Value) -> Result<Release, String> {
    let tag = json["tag_name"].as_str().ok_or("the release has no tag")?;
    let asset_url = |name: &str| {
        json["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_string)
    };
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: json["body"].as_str().unwrap_or("").to_string(),
        msi_url: asset_url(MSI_ASSET).ok_or("the release has no installer")?,
        checksum_url: asset_url(&format!("{MSI_ASSET}.sha256"))
            .ok_or("the release has no installer checksum")?,
    })
}

/// Whether version `candidate` (`x.y.z`) is newer than `current`. A pre-release
/// suffix (`-rc.1`) is ignored.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(candidate) > parts(current)
}

/// The hash in a `.sha256` file (`<hex> *<name>`).
fn parse_checksum(text: &str) -> Option<String> {
    let hex = text.split_whitespace().next()?.to_ascii_lowercase();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(concat!("browser/", env!("CARGO_PKG_VERSION"))) // GitHub requires one
        .build()
        .map_err(|e| e.to_string())
}

fn fetch_latest() -> Result<Release, String> {
    let json: serde_json::Value = client(15)?
        .get(feed_url())
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?;
    parse_release(&json)
}

/// Download `release`'s installer into the temp folder and check its SHA-256.
fn download(release: &Release) -> Result<Staged, String> {
    let c = client(600)?;
    let get = |url: &str| {
        c.get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("download failed: {e}"))
    };
    let expected = parse_checksum(&String::from_utf8_lossy(&get(&release.checksum_url)?))
        .ok_or("the installer's checksum file is malformed")?;
    let msi = get(&release.msi_url)?;
    if sha256_hex(&msi) != expected {
        return Err(
            "the downloaded installer doesn't match its checksum — not installing it".into(),
        );
    }
    let dir = update_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("browser-{}.msi", release.version));
    std::fs::write(&path, &msi).map_err(|e| e.to_string())?;
    Ok(Staged {
        version: release.version.clone(),
        msi: path,
    })
}

/// Where an update's installer and helper script are downloaded.
fn update_dir() -> PathBuf {
    std::env::temp_dir().join("browser-update")
}

/// Delete what an update left behind. By the time the browser starts again the
/// installer has finished; the helper script may still be closing, in which case it
/// goes on the next launch.
pub(crate) fn clean_leftovers() {
    remove_leftovers(&update_dir());
}

fn remove_leftovers(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        let _ = std::fs::remove_dir(dir);
    }
}

/// How this copy was installed, which decides how it updates.
#[derive(Debug, PartialEq)]
enum Install {
    /// A debug build, run from the source tree.
    Dev,
    /// The per-user installer: updates silently.
    User,
    /// The old per-machine installer, in Program Files: moves to a per-user install.
    Machine,
    /// Anything else (the zip, `install.ps1`): the installer would add a second copy.
    Other(PathBuf),
}

/// `user_dir` is the folder the per-user installer recorded (`InstallDir`).
fn install_kind(
    exe: &Path,
    user_dir: Option<&Path>,
    program_files: &[PathBuf],
    debug: bool,
) -> Install {
    if debug {
        return Install::Dev;
    }
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    let exe_l = lower(exe);
    if user_dir.is_some_and(|d| exe_l == lower(&d.join("bin").join("browser.exe"))) {
        Install::User
    } else if program_files
        .iter()
        .any(|pf| exe_l.starts_with(&(lower(pf) + "\\")))
    {
        Install::Machine
    } else {
        Install::Other(exe.to_path_buf())
    }
}

fn this_install() -> Install {
    let exe = std::env::current_exe().unwrap_or_default();
    let program_files: Vec<PathBuf> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    install_kind(
        &exe,
        user_install_dir().as_deref(),
        &program_files,
        cfg!(debug_assertions),
    )
}

/// The folder the per-user installer installed into, from the registry.
#[cfg(windows)]
fn user_install_dir() -> Option<PathBuf> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut buf = [0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\kayf\browser"),
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = (bytes as usize / 2).saturating_sub(1); // drop the terminating NUL
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

#[cfg(not(windows))]
fn user_install_dir() -> Option<PathBuf> {
    None
}

/// PowerShell's single-quoted string for `p`.
fn ps_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "''"))
}

/// The helper for a per-user install: wait for this process to exit, install the
/// update silently, then start the browser again if `relaunch`.
fn install_script(pid: u32, msi: &Path, relaunch: Option<&Path>) -> String {
    let mut s = format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue\r\n\
         $msi = {msi}\r\n\
         Start-Process msiexec.exe -ArgumentList ('/i \"' + $msi + '\" /qn') -Wait\r\n",
        msi = ps_quote(msi),
    );
    if let Some(exe) = relaunch {
        s += &format!("Start-Process {}\r\n", ps_quote(exe));
    }
    s
}

/// The helper that moves a per-machine install to the per-user one: wait for this
/// process to exit, remove the old copy (Windows asks for permission; if that's refused
/// the old copy just starts again), install the new one silently and start it.
fn migrate_script(pid: u32, msi: &Path, old_exe: &Path) -> String {
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue\r\n\
         $msi = {msi}\r\n\
         $old = Get-ItemProperty 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' -ErrorAction SilentlyContinue |\r\n\
         \x20   Where-Object {{ $_.DisplayName -eq 'browser' }} | Select-Object -First 1\r\n\
         if ($old) {{\r\n\
         \x20   try {{ $p = Start-Process msiexec.exe -ArgumentList ('/x ' + $old.PSChildName + ' /qn') -Verb RunAs -Wait -PassThru }} catch {{ $p = $null }}\r\n\
         \x20   if (-not $p -or $p.ExitCode -ne 0) {{ Start-Process {old}; exit }}\r\n\
         }}\r\n\
         Start-Process msiexec.exe -ArgumentList ('/i \"' + $msi + '\" /qn') -Wait\r\n\
         $dir = (Get-ItemProperty 'HKCU:\\Software\\kayf\\browser' -ErrorAction SilentlyContinue).InstallDir\r\n\
         if (-not $dir) {{ $dir = Join-Path $env:LOCALAPPDATA 'Programs\\browser' }}\r\n\
         Start-Process (Join-Path $dir 'bin\\browser.exe')\r\n",
        msi = ps_quote(msi),
        old = ps_quote(old_exe),
    )
}

/// Write `script` next to the installer and run it, hidden. It outlives the browser.
fn spawn_helper(script: &str, msi: &Path) -> Result<(), String> {
    let path = msi.with_file_name("install.ps1");
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    let mut helper = std::process::Command::new("powershell.exe");
    helper.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
    ]);
    helper.args(["-WindowStyle", "Hidden", "-File"]).arg(&path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        helper.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    helper.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Checks every [`CHECK_INTERVAL_SECS`] while the browser runs. The thread only
/// sleeps; whether to check is decided when its event arrives.
pub(crate) fn start_periodic_checks(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(CHECK_INTERVAL_SECS));
        if proxy.send_event(UserEvent::UpdateCheckDue).is_err() {
            return;
        }
    });
}

impl App {
    pub(crate) fn update_mode(&self) -> Mode {
        match self.config.update_mode.as_deref() {
            Some("off") => Mode::Off,
            Some("notify") => Mode::Notify,
            // Before modes, `:update off` was check_updates = false.
            None if self.config.check_updates == Some(false) => Mode::Off,
            _ => Mode::Auto,
        }
    }

    /// `:update [check|install|auto|notify|off]`.
    pub(crate) fn update_command(&mut self, arg: &str) {
        let set_mode = |app: &mut App, mode: Option<&str>, msg: &str| {
            app.config.update_mode = mode.map(str::to_string);
            app.config.check_updates = None;
            crate::config::save(&app.config);
            app.set_status(msg.to_string());
        };
        match arg.trim() {
            "" | "check" => self.check_for_update(true),
            "install" => self.install_update_now(),
            "auto" | "on" => set_mode(
                self,
                None,
                "updates download in the background and install when you quit",
            ),
            "notify" => set_mode(
                self,
                Some("notify"),
                "updates are only announced — :update install installs one",
            ),
            "off" => set_mode(self, Some("off"), "no update checks — :update checks now"),
            other => self.set_error(format!(
                "unknown :update {other} — try :update, :update install, :update auto|notify|off"
            )),
        }
    }

    /// Check for a newer release in the background. `manual` checks report back
    /// either way; the others only show a notice (or, in `auto`, download it).
    pub(crate) fn check_for_update(&mut self, manual: bool) {
        if manual {
            self.set_status("checking for updates…");
        }
        self.config.update_checked_at = Some(crate::app::now_epoch());
        // A throwaway --scratch run must not write the real config.
        if !self.cli_scratch {
            crate::config::save(&self.config);
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let release = fetch_latest();
            let _ = proxy.send_event(UserEvent::UpdateChecked { release, manual });
        });
    }

    /// The launch check: when the last one was over [`CHECK_INTERVAL_SECS`] ago.
    pub(crate) fn check_for_update_at_launch(&mut self) {
        let now = crate::app::now_epoch();
        let due = self
            .config
            .update_checked_at
            .is_none_or(|at| now.saturating_sub(at) >= CHECK_INTERVAL_SECS);
        if self.update_mode() != Mode::Off && due {
            self.check_for_update(false);
        }
    }

    /// The periodic check fired.
    pub(crate) fn on_update_check_due(&mut self) {
        if self.update_mode() != Mode::Off {
            self.check_for_update(false);
        }
    }

    pub(crate) fn on_update_checked(&mut self, release: Result<Release, String>, manual: bool) {
        let current = current_version();
        match release {
            Ok(r) if is_newer(&r.version, &current) => {
                let staged = self
                    .staged_update
                    .as_ref()
                    .is_some_and(|s| s.version == r.version);
                let stage =
                    !staged && self.update_mode() == Mode::Auto && this_install() == Install::User;
                if manual {
                    self.show_release_notes(&r, &current);
                    self.set_status(if staged || stage {
                        format!(
                            "browser {} installs when you quit (you have {current}) — :update install installs it now",
                            r.version
                        )
                    } else {
                        format!(
                            "browser {} is available (you have {current}) — :update install",
                            r.version
                        )
                    });
                }
                if stage {
                    self.download_update(r.clone(), false);
                }
                self.update_available = Some(r);
            }
            Ok(_) => {
                self.update_available = None;
                if manual {
                    self.set_status(format!("browser {current} is the latest version"));
                }
            }
            Err(error) if manual => self.set_error(format!("update check failed: {error}")),
            Err(_) => {}
        }
        self.window.request_redraw();
    }

    fn show_release_notes(&mut self, r: &Release, current: &str) {
        let text = format!(
            "browser {} is available — you have {current}. It installs when you quit, or now \
             with `:update install` (the browser restarts with your tabs).\n\n{}",
            r.version, r.notes
        );
        let mut doc = crate::markdown::to_document(&text, UPDATE_URL);
        doc.title = format!("browser {}", r.version);
        self.show_read_document(doc, false, false);
    }

    /// Download `release` in the background: to install it now, or to stage it for quit.
    fn download_update(&mut self, release: Release, now: bool) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = download(&release);
            let _ = proxy.send_event(UserEvent::UpdateDownloaded { result, now });
        });
    }

    /// `:update install` — install the latest release now and restart.
    fn install_update_now(&mut self) {
        match this_install() {
            Install::Dev => {
                self.set_status(
                    "this is a development build — update it with git pull and cargo build (or install.ps1)",
                );
                return;
            }
            Install::Other(path) => {
                self.set_status(format!(
                    "this copy ({}) wasn't installed with the installer — get the new version from {RELEASES_PAGE}",
                    path.display()
                ));
                return;
            }
            Install::User | Install::Machine => {}
        }
        if let Some(staged) = self.staged_update.take() {
            return self.on_update_downloaded(Ok(staged), true);
        }
        self.set_status("downloading the update…");
        let proxy = self.proxy.clone();
        let current = current_version();
        std::thread::spawn(move || {
            let result = fetch_latest().and_then(|r| {
                if is_newer(&r.version, &current) {
                    download(&r)
                } else {
                    Err(format!("browser {current} is already the latest version"))
                }
            });
            let _ = proxy.send_event(UserEvent::UpdateDownloaded { result, now: true });
        });
    }

    /// A release was downloaded and verified. To install `now`: start the helper,
    /// save the session and quit. Otherwise keep it for when the browser quits.
    pub(crate) fn on_update_downloaded(&mut self, result: Result<Staged, String>, now: bool) {
        let staged = match result {
            Ok(staged) => staged,
            Err(error) => {
                if now {
                    self.set_error(format!("update failed: {error}"));
                }
                return;
            }
        };
        if !now {
            self.staged_update = Some(staged);
            self.window.request_redraw();
            return;
        }
        let exe = std::env::current_exe().unwrap_or_default();
        let pid = std::process::id();
        let script = match this_install() {
            Install::Machine => migrate_script(pid, &staged.msi, &exe),
            _ => install_script(pid, &staged.msi, Some(&exe)),
        };
        if let Err(error) = spawn_helper(&script, &staged.msi) {
            self.set_error(format!("couldn't start the installer: {error}"));
            return;
        }
        self.save_session();
        self.set_status(format!(
            "installing browser {} — back in a moment",
            staged.version
        ));
        self.quit = true;
    }

    /// On quit: install a staged update, silently, once the browser has exited.
    pub(crate) fn install_staged_update(&mut self) {
        if let Some(staged) = self.staged_update.take() {
            let script = install_script(std::process::id(), &staged.msi, None);
            let _ = spawn_helper(&script, &staged.msi);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("0.3.0", "0.2.1"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("0.3.0-rc.1", "0.3.0"));
    }

    #[test]
    fn a_github_release_gives_its_version_notes_and_installer() {
        let json = serde_json::json!({
            "tag_name": "v0.4.0",
            "body": "### Features\n* something",
            "assets": [
                { "name": "browser-x86_64-pc-windows-msvc.zip", "browser_download_url": "https://x/zip" },
                { "name": "browser-x86_64-pc-windows-msvc.msi", "browser_download_url": "https://x/msi" },
                { "name": "browser-x86_64-pc-windows-msvc.msi.sha256", "browser_download_url": "https://x/sha" }
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, "0.4.0");
        assert_eq!(r.msi_url, "https://x/msi");
        assert_eq!(r.checksum_url, "https://x/sha");
        assert!(r.notes.contains("something"));
        assert!(parse_release(&serde_json::json!({ "tag_name": "v1", "assets": [] })).is_err());
    }

    #[test]
    fn checksum_files_are_read_and_compared() {
        let hash = sha256_hex(b"abc");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            parse_checksum(&format!("{hash} *browser.msi\n")),
            Some(hash)
        );
        assert_eq!(parse_checksum("not a hash"), None);
    }

    #[test]
    fn the_install_kind_decides_how_a_copy_updates() {
        let pf = [PathBuf::from(r"C:\Program Files")];
        let user = PathBuf::from(r"C:\Users\a\AppData\Local\Programs\browser\");
        let kind = |exe: &str, debug| install_kind(Path::new(exe), Some(&user), &pf, debug);
        assert_eq!(
            kind(
                r"C:\Users\a\AppData\Local\Programs\browser\bin\browser.exe",
                false
            ),
            Install::User
        );
        assert_eq!(
            kind(
                r"c:\users\a\appdata\local\programs\browser\bin\browser.exe",
                false
            ),
            Install::User
        );
        // install.ps1 puts browser.exe straight into the same folder: not the installer's.
        assert!(matches!(
            kind(
                r"C:\Users\a\AppData\Local\Programs\browser\browser.exe",
                false
            ),
            Install::Other(_)
        ));
        assert_eq!(
            kind(r"C:\Program Files\browser\bin\browser.exe", false),
            Install::Machine
        );
        assert!(matches!(
            kind(r"C:\Program Files Extra\browser.exe", false),
            Install::Other(_)
        ));
        assert_eq!(
            kind(r"C:\Program Files\browser\bin\browser.exe", true),
            Install::Dev
        );
    }

    #[test]
    fn the_install_helper_waits_installs_silently_and_maybe_restarts() {
        let msi = Path::new(r"C:\Users\O'Neil\AppData\Local\Temp\browser-update\browser-0.4.0.msi");
        let exe = Path::new(r"C:\Users\O'Neil\AppData\Local\Programs\browser\bin\browser.exe");
        let s = install_script(42, msi, Some(exe));
        assert!(s.starts_with("Wait-Process -Id 42"));
        assert!(s.contains(r"$msi = 'C:\Users\O''Neil\AppData"));
        assert!(s.contains("/qn"));
        assert!(s.contains(
            r"Start-Process 'C:\Users\O''Neil\AppData\Local\Programs\browser\bin\browser.exe'"
        ));
        assert!(!install_script(42, msi, None).contains("Start-Process 'C:"));
    }

    #[test]
    fn the_migration_helper_removes_the_old_copy_first_and_falls_back_to_it() {
        let s = migrate_script(
            7,
            Path::new(r"C:\Temp\browser-0.4.0.msi"),
            Path::new(r"C:\Program Files\browser\bin\browser.exe"),
        );
        let uninstall = s.find("'/x '").unwrap();
        let install = s.find("'/i \"'").unwrap();
        assert!(uninstall < install, "the old copy goes first");
        assert!(s.contains("-Verb RunAs"));
        assert!(s.contains(r"Start-Process 'C:\Program Files\browser\bin\browser.exe'; exit"));
        assert!(s.contains(r"Join-Path $dir 'bin\browser.exe'"));
    }

    #[test]
    fn leftovers_from_an_update_are_removed() {
        let dir = std::env::temp_dir().join(format!("browser-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("browser-0.4.0.msi"), b"msi").unwrap();
        std::fs::write(dir.join("install.ps1"), b"script").unwrap();
        remove_leftovers(&dir);
        assert!(!dir.exists());
        remove_leftovers(&dir); // nothing there: no error
    }

    /// Against the real latest release: `cargo test -p browser -- --ignored update`.
    #[test]
    #[ignore = "downloads the latest release from GitHub"]
    fn the_latest_release_downloads_and_verifies() {
        let release = fetch_latest().expect("GitHub answers");
        let staged = download(&release).expect("the installer matches its checksum");
        assert!(std::fs::metadata(&staged.msi).unwrap().len() > 1_000_000);
        std::fs::remove_file(staged.msi).unwrap();
    }
}
