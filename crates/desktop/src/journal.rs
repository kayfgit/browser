//! The diagnostic log: a plain-text trail of what the browser did and when, one file
//! per launch, so a sudden crash or a lost session can be pieced together afterwards
//! (by you, or by an AI reading the file).
//!
//! **Early development: it records nearly everything** — every command you run, the
//! addresses tabs open and navigate to, session saves and loads, mode changes, clicks,
//! the browser's own keys, every message shown in the bar, update steps, panics, and
//! native crashes (with a minidump). What you type INTO pages is never recorded, so
//! passwords don't end up in a file. Once the browser matures this will be cut back.
//! (`README.md` and the user guide say so too.)
//!
//! Files live in `%LOCALAPPDATA%\browser\data\logs` as
//! `browser-<date>_<time>-pid<pid>.log`, with any crash dump beside it as `.dmp`.
//! Logs older than [`KEEP_DAYS`] go, and the folder stays under [`KEEP_BYTES`].
//!
//! Each line is `YYYY-MM-DD HH:MM:SS.mmm [area] message`, written straight to the
//! file (no buffering), so everything up to a crash is on disk. A launch whose log
//! doesn't end with `[exit] shutdown complete` crashed or was killed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Logs older than this many days are deleted at launch.
pub(crate) const KEEP_DAYS: u64 = 14;
/// The logs folder is kept under this many bytes (newest kept, crash dumps included).
pub(crate) const KEEP_BYTES: u64 = 200 * 1024 * 1024;

struct Journal {
    file: Mutex<std::fs::File>,
    path: PathBuf,
}

static JOURNAL: OnceLock<Journal> = OnceLock::new();

/// `%LOCALAPPDATA%\browser\data\logs`: machine-local, like the other bulky state.
pub(crate) fn dir() -> Option<PathBuf> {
    crate::session::local_data_dir().map(|d| d.join("logs"))
}

/// The log file this launch writes, once [`init`] has run.
pub(crate) fn current_path() -> Option<PathBuf> {
    JOURNAL.get().map(|j| j.path.clone())
}

/// Open this launch's log file, catch panics and native crashes into it, and prune
/// old logs in the background. Call once, first thing in `main`.
pub(crate) fn init() {
    let Some(dir) = dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let stem = format!(
        "browser-{}-pid{}",
        file_stamp(&local_now()),
        std::process::id()
    );
    let path = dir.join(format!("{stem}.log"));
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = JOURNAL.set(Journal {
        file: Mutex::new(file),
        path: path.clone(),
    });
    install_panic_hook();
    #[cfg(windows)]
    crash::install(dir.join(format!("{stem}.dmp")));
    std::thread::spawn(move || prune(&dir, &path));
}

/// Append one entry to the log. A no-op before [`init`] (and in unit tests).
pub(crate) fn log(area: &str, msg: impl AsRef<str>) {
    let Some(j) = JOURNAL.get() else { return };
    let line = format_line(&stamp(&local_now()), area, msg.as_ref());
    // A poisoned lock (a panic mid-write) still holds a usable file.
    let mut file = j.file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = file.write_all(line.as_bytes());
}

/// One log line: the time, the area in brackets, the message. A multi-line message
/// (a backtrace, an error chain) keeps its lines, indented with a tab, so every
/// entry still starts with a timestamp.
fn format_line(stamp: &str, area: &str, msg: &str) -> String {
    let mut out = format!("{stamp} [{area}] ");
    for (i, line) in msg.trim_end().lines().enumerate() {
        if i > 0 {
            out.push_str("\n\t");
        }
        out.push_str(line);
    }
    out.push('\n');
    out
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        let place = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let trace = std::backtrace::Backtrace::force_capture();
        log(
            "panic",
            format!("thread '{name}' panicked at {place}: {payload}\n{trace}"),
        );
        default(info);
    }));
}

/// Delete what [`prune_plan`] picks, never this launch's own log.
fn prune(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let files: Vec<(PathBuf, u64, u64)> = entries
        .flatten()
        .filter(|e| e.path() != current)
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let age = now
                .duration_since(meta.modified().ok()?)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some((e.path(), age, meta.len()))
        })
        .collect();
    let budget = KEEP_BYTES.saturating_sub(std::fs::metadata(current).map_or(0, |m| m.len()));
    for path in prune_plan(&files, KEEP_DAYS * 86_400, budget) {
        let _ = std::fs::remove_file(path);
    }
}

/// Which of `files` (path, age in seconds, size) to delete: everything older than
/// `max_age`, then the oldest of the rest until what's left fits in `max_bytes`.
fn prune_plan(files: &[(PathBuf, u64, u64)], max_age: u64, max_bytes: u64) -> Vec<PathBuf> {
    let mut doomed = Vec::new();
    let mut kept: Vec<&(PathBuf, u64, u64)> = Vec::new();
    for f in files {
        if f.1 > max_age {
            doomed.push(f.0.clone());
        } else {
            kept.push(f);
        }
    }
    // Newest first; keep adding until the budget is spent, delete the rest.
    kept.sort_by_key(|f| f.1);
    let mut total = 0u64;
    for f in kept {
        total = total.saturating_add(f.2);
        if total > max_bytes {
            doomed.push(f.0.clone());
        }
    }
    doomed
}

/// Local wall-clock time, broken down.
struct Clock {
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milli: u16,
}

#[cfg(windows)]
fn local_now() -> Clock {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    Clock {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        milli: st.wMilliseconds,
    }
}

/// Elsewhere: UTC (the logs are a Windows-first feature, like the browser).
#[cfg(not(windows))]
fn local_now() -> Clock {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = (secs / 86_400) as i64;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let s = secs % 86_400;
    Clock {
        year: year as u16,
        month: month as u16,
        day: day as u16,
        hour: (s / 3600) as u16,
        minute: (s % 3600 / 60) as u16,
        second: (s % 60) as u16,
        milli: d.subsec_millis() as u16,
    }
}

fn stamp(c: &Clock) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.milli
    )
}

fn file_stamp(c: &Clock) -> String {
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

/// Native crashes (an access violation in unsafe code, a fault inside a library):
/// log the exception, write a minidump beside the log, then let Windows end the
/// process as it would have.
#[cfg(windows)]
mod crash {
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Diagnostics::Debug::{
        MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo, MiniDumpWriteDump,
        SetUnhandledExceptionFilter, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
        MINIDUMP_TYPE,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
    };

    static DUMP_PATH: OnceLock<PathBuf> = OnceLock::new();

    pub(super) fn install(dump: PathBuf) {
        let _ = DUMP_PATH.set(dump);
        unsafe {
            SetUnhandledExceptionFilter(Some(on_crash));
        }
    }

    /// `EXCEPTION_CONTINUE_SEARCH`: let Windows Error Reporting carry on as usual.
    const CONTINUE_SEARCH: i32 = 0;

    unsafe extern "system" fn on_crash(info: *const EXCEPTION_POINTERS) -> i32 {
        let (code, addr) = unsafe {
            info.as_ref()
                .and_then(|p| p.ExceptionRecord.as_ref())
                .map(|r| (r.ExceptionCode.0 as u32, r.ExceptionAddress))
                .unwrap_or((0, std::ptr::null_mut()))
        };
        let dump = DUMP_PATH.get();
        super::log(
            "crash",
            format!(
                "unhandled exception 0x{code:08X} at {addr:?} on thread {} — minidump: {}",
                unsafe { GetCurrentThreadId() },
                dump.map(|p| p.display().to_string()).unwrap_or_default()
            ),
        );
        if let Some(path) = dump {
            if let Ok(file) = std::fs::File::create(path) {
                let exception = MINIDUMP_EXCEPTION_INFORMATION {
                    ThreadId: unsafe { GetCurrentThreadId() },
                    ExceptionPointers: info as *mut _,
                    ClientPointers: false.into(),
                };
                let kind = MINIDUMP_TYPE(
                    MiniDumpWithThreadInfo.0 | MiniDumpWithIndirectlyReferencedMemory.0,
                );
                let _ = unsafe {
                    MiniDumpWriteDump(
                        GetCurrentProcess(),
                        GetCurrentProcessId(),
                        HANDLE(file.as_raw_handle()),
                        kind,
                        Some(&exception),
                        None,
                        None,
                    )
                };
            }
        }
        CONTINUE_SEARCH
    }
}

#[cfg(test)]
mod tests {
    use super::{format_line, prune_plan};
    use std::path::PathBuf;

    #[test]
    fn a_line_starts_with_time_and_area_and_indents_continuations() {
        assert_eq!(
            format_line("2026-11-18 00:06:44.123", "cmd", ":open example.com"),
            "2026-11-18 00:06:44.123 [cmd] :open example.com\n"
        );
        assert_eq!(
            format_line("T", "panic", "boom\n  at a\n  at b\n"),
            "T [panic] boom\n\t  at a\n\t  at b\n"
        );
    }

    #[test]
    fn pruning_drops_old_logs_then_the_oldest_over_budget() {
        let day = 86_400;
        let f = |name: &str, age: u64, size: u64| (PathBuf::from(name), age, size);
        let files = [
            f("new.log", 10, 50),
            f("mid.log", 3 * day, 50),
            f("old.log", 20 * day, 1),
            f("older-but-young.log", 5 * day, 50),
        ];
        let mut doomed = prune_plan(&files, 14 * day, 100);
        doomed.sort();
        // Past 14 days: gone. Then newest first fits two 50-byte files in 100.
        assert_eq!(
            doomed,
            [
                PathBuf::from("old.log"),
                PathBuf::from("older-but-young.log")
            ]
        );
        // Plenty of room and all young: nothing goes.
        assert!(prune_plan(&files[..2], 14 * day, 1_000).is_empty());
    }
}
