//! `:update` — check GitHub for a newer release, show its notes, and install it.
//!
//! Nothing updates by itself. A check runs on `:update` and, unless turned off with
//! `:update off`, at most once a day at launch, which only puts a notice in the status
//! bar. `:update install` downloads the MSI, checks it against the SHA-256 the release
//! publishes, saves the session, and hands over to a hidden helper that waits for the
//! browser to exit, runs the installer (Windows asks for permission: it installs for
//! all users) and starts the browser again. The web engine itself, WebView2, is kept up
//! to date by Windows.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{App, UserEvent};

const RELEASES_API: &str = "https://api.github.com/repos/kayfgit/browser/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/kayfgit/browser/releases";
const MSI_ASSET: &str = "browser-x86_64-pc-windows-msvc.msi";
/// The launch check runs at most this often.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
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
        .get(RELEASES_API)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?;
    parse_release(&json)
}

/// Download `release`'s installer into the temp folder and check its SHA-256.
fn download(release: &Release) -> Result<PathBuf, String> {
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
    Ok(path)
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

/// How this copy was installed, which decides whether `:update install` can replace it.
#[derive(Debug, PartialEq)]
enum Install {
    /// A debug build, run from the source tree.
    Dev,
    /// Installed by the MSI, into Program Files.
    Msi,
    /// Anything else (the zip, `install.ps1`): the MSI would add a second copy.
    Other(PathBuf),
}

fn install_kind(exe: &Path, program_files: &[PathBuf], debug: bool) -> Install {
    if debug {
        return Install::Dev;
    }
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    let exe_l = lower(exe);
    if program_files
        .iter()
        .any(|pf| exe_l.starts_with(&(lower(pf) + "\\")))
    {
        Install::Msi
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
    install_kind(&exe, &program_files, cfg!(debug_assertions))
}

/// The helper script: wait for this process to exit, run the installer, start the
/// browser again (also when the install was cancelled, so it comes back either way).
fn helper_script(pid: u32, msi: &Path, exe: &Path) -> String {
    let quote = |p: &Path| p.to_string_lossy().replace('\'', "''");
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue\r\n\
         $msi = '{msi}'\r\n\
         Start-Process msiexec.exe -ArgumentList ('/i \"' + $msi + '\" /passive') -Wait\r\n\
         Start-Process '{exe}'\r\n",
        msi = quote(msi),
        exe = quote(exe),
    )
}

impl App {
    /// `:update [check|install|on|off]`.
    pub(crate) fn update_command(&mut self, arg: &str) {
        match arg.trim() {
            "" | "check" => self.check_for_update(true),
            "install" => self.install_update(),
            "on" => {
                self.config.check_updates = None;
                crate::config::save(&self.config);
                self.set_status("checking for updates once a day at launch");
            }
            "off" => {
                self.config.check_updates = Some(false);
                crate::config::save(&self.config);
                self.set_status("no automatic update checks — :update checks now");
            }
            other => self.set_error(format!(
                "unknown :update {other} — try :update, :update install, :update on|off"
            )),
        }
    }

    /// Check for a newer release in the background. `manual` checks report back
    /// either way; the launch check only speaks up when there's one.
    pub(crate) fn check_for_update(&mut self, manual: bool) {
        if manual {
            self.set_status("checking for updates…");
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let release = fetch_latest();
            let _ = proxy.send_event(UserEvent::UpdateChecked { release, manual });
        });
    }

    /// The launch check: once a day at most, unless turned off.
    pub(crate) fn check_for_update_at_launch(&mut self) {
        let now = crate::app::now_epoch();
        let due = self
            .config
            .update_checked_at
            .is_none_or(|at| now.saturating_sub(at) >= CHECK_INTERVAL_SECS);
        if self.config.check_updates != Some(false) && due {
            self.config.update_checked_at = Some(now);
            crate::config::save(&self.config);
            self.check_for_update(false);
        }
    }

    pub(crate) fn on_update_checked(&mut self, release: Result<Release, String>, manual: bool) {
        let current = current_version();
        match release {
            Ok(r) if is_newer(&r.version, &current) => {
                if manual {
                    self.show_release_notes(&r, &current);
                    self.set_status(format!(
                        "browser {} is available (you have {current}) — :update install",
                        r.version
                    ));
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
            "browser {} is available — you have {current}. `:update install` installs it \
             (Windows asks for permission), then the browser restarts with your tabs.\n\n{}",
            r.version, r.notes
        );
        let mut doc = crate::markdown::to_document(&text, UPDATE_URL);
        doc.title = format!("browser {}", r.version);
        self.show_read_document(doc, false, false);
    }

    /// `:update install` — download, verify, then hand over to the installer.
    fn install_update(&mut self) {
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
            Install::Msi => {}
        }
        self.set_status("downloading the update…");
        let proxy = self.proxy.clone();
        let current = current_version();
        std::thread::spawn(move || {
            let result = fetch_latest().and_then(|r| {
                if is_newer(&r.version, &current) {
                    download(&r).map(|path| (r.version, path))
                } else {
                    Err(format!("browser {current} is already the latest version"))
                }
            });
            let _ = proxy.send_event(UserEvent::UpdateDownloaded(result));
        });
    }

    /// The installer is downloaded and verified: save the session, start the helper
    /// that installs it once this process has exited, and quit.
    pub(crate) fn on_update_downloaded(&mut self, result: Result<(String, PathBuf), String>) {
        let (version, msi) = match result {
            Ok(done) => done,
            Err(error) => {
                self.set_error(format!("update failed: {error}"));
                return;
            }
        };
        let exe = std::env::current_exe().unwrap_or_default();
        let script = msi.with_file_name("install.ps1");
        let script_text = helper_script(std::process::id(), &msi, &exe);
        if let Err(error) = std::fs::write(&script, script_text) {
            self.set_error(format!("update failed: {error}"));
            return;
        }
        let mut helper = std::process::Command::new("powershell.exe");
        helper.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
        ]);
        helper
            .args(["-WindowStyle", "Hidden", "-File"])
            .arg(&script);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            helper.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Err(error) = helper.spawn() {
            self.set_error(format!("couldn't start the installer: {error}"));
            return;
        }
        self.save_session();
        self.set_status(format!("installing browser {version} — back in a moment"));
        self.quit = true;
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
    fn only_an_msi_install_can_be_replaced() {
        let pf = [PathBuf::from(r"C:\Program Files")];
        let exe = |p: &str| PathBuf::from(p);
        assert_eq!(
            install_kind(&exe(r"C:\Program Files\browser\browser.exe"), &pf, false),
            Install::Msi
        );
        assert_eq!(
            install_kind(&exe(r"c:\program files\browser\browser.exe"), &pf, false),
            Install::Msi
        );
        assert!(matches!(
            install_kind(
                &exe(r"C:\Users\a\AppData\Local\Programs\browser\browser.exe"),
                &pf,
                false
            ),
            Install::Other(_)
        ));
        assert!(matches!(
            install_kind(&exe(r"C:\Program Files Extra\browser.exe"), &pf, false),
            Install::Other(_)
        ));
        assert_eq!(
            install_kind(&exe(r"C:\Program Files\browser\browser.exe"), &pf, true),
            Install::Dev
        );
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
        let path = download(&release).expect("the installer matches its checksum");
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000_000);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_helper_waits_installs_and_restarts() {
        let s = helper_script(
            42,
            Path::new(r"C:\Users\O'Neil\AppData\Local\Temp\browser-update\browser-0.4.0.msi"),
            Path::new(r"C:\Program Files\browser\browser.exe"),
        );
        assert!(s.starts_with("Wait-Process -Id 42"));
        assert!(s.contains(r"$msi = 'C:\Users\O''Neil\AppData"));
        assert!(s.contains("/passive"));
        assert!(s.contains(r"Start-Process 'C:\Program Files\browser\browser.exe'"));
    }
}
