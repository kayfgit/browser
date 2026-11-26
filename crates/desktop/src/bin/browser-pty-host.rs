//! browser-pty-host — a tiny companion process that owns a Windows ConPTY (or
//! platform PTY) and bridges it to the browser over stdin/stdout. It exists so
//! the ConPTY lives OUT of the browser process: a live ConPTY leaves a thread
//! stuck in an uninterruptible kernel read that deadlocks process exit, so we
//! isolate it here. The browser force-kills this process (via a kill-on-close
//! job object) when the tab closes or the browser exits — this process never
//! needs to shut the ConPTY down gracefully.
//!
//! Protocol:
//!   * argv: `<cols> <rows> [--cwd <dir>] [--conpty <dir>] <program> [args...]`
//!     (`--cwd` starts the shell in `<dir>` — session restore lands terminals
//!     back in their saved working directory; `--conpty` names the folder holding
//!     the bundled `conpty.dll` + `OpenConsole.exe`, see `use_conpty`)
//!   * browser → our stdin: frames `[type:u8][len:u32 LE][payload]`;
//!     type 0 = input bytes to write to the PTY; type 1 = resize with payload
//!     `[cols:u16 LE][rows:u16 LE]`.
//!   * our stdout ← PTY: raw output bytes (the browser feeds them to its VT parser).
//!
//! Built as a Windows GUI-subsystem binary so launching it from the (GUI)
//! browser does not pop up a console window — and, crucially, WITHOUT spawning
//! it under `CREATE_NO_WINDOW`: a console-less host can fail to back its
//! pseudoconsole, so the shell starts but its output never flows back (a
//! terminal that "opens" to a blinking cursor and nothing else). The GUI
//! subsystem hides the window while still letting ConPTY create its own
//! (headless) conhost. Our stdio is the parent's redirected pipes, so it works
//! despite having no console of our own.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{Read, Write};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// The command line, parsed.
#[derive(Debug, PartialEq)]
struct Args<'a> {
    cols: u16,
    rows: u16,
    /// Start the shell here (session restore). A missing dir is ignored.
    cwd: Option<&'a str>,
    /// The folder holding the bundled ConPTY.
    conpty: Option<&'a str>,
    /// The shell and its arguments.
    command: &'a [String],
}

/// Parse `<cols> <rows> [--cwd <dir>] [--conpty <dir>] <program> [args...]`; the
/// options come before the program, in any order. `None` without a program.
fn parse_args(args: &[String]) -> Option<Args<'_>> {
    let mut parsed = Args {
        cols: args.get(1)?.parse().unwrap_or(80),
        rows: args.get(2)?.parse().unwrap_or(24),
        cwd: None,
        conpty: None,
        command: &[],
    };
    let mut at = 3;
    loop {
        match args.get(at).map(String::as_str) {
            Some("--cwd") if args.len() > at + 2 => parsed.cwd = Some(&args[at + 1]),
            Some("--conpty") if args.len() > at + 2 => parsed.conpty = Some(&args[at + 1]),
            _ => break,
        }
        at += 2;
    }
    parsed.command = args.get(at..).filter(|c| !c.is_empty())?;
    Some(parsed)
}

/// Decide which ConPTY `portable-pty` loads, before it first does. It asks for
/// `conpty.dll` by bare name and prefers it over Windows' own, so the normal DLL
/// search could pick up any `conpty.dll` on PATH or in the current folder (an old
/// unsigned copy from another terminal, say). Searching only the default folders
/// (this exe's, System32, and ones added here) closes that: with `dir` (the bundled
/// Microsoft ConPTY, used only when both of its files are there) it loads that one,
/// and without, Windows' own.
#[cfg(windows)]
fn use_conpty(dir: Option<&str>) {
    use windows::core::HSTRING;
    use windows::Win32::System::LibraryLoader::{
        AddDllDirectory, SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    };
    // SAFETY: plain Win32 calls; the string outlives the call.
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
        if let Some(dir) = dir {
            let dir = std::path::Path::new(dir);
            // Without OpenConsole.exe beside it, conpty.dll would quietly fall back
            // to conhost; use Windows' own openly instead.
            if ["conpty.dll", "OpenConsole.exe"]
                .iter()
                .all(|f| dir.join(f).is_file())
            {
                AddDllDirectory(&HSTRING::from(dir.as_os_str()));
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(Args {
        cols,
        rows,
        cwd,
        conpty,
        command,
    }) = parse_args(&args)
    else {
        eprintln!(
            "usage: browser-pty-host <cols> <rows> [--cwd <dir>] [--conpty <dir>] <program> [args...]"
        );
        std::process::exit(2);
    };
    #[cfg(windows)]
    use_conpty(conpty);
    #[cfg(not(windows))]
    let _ = conpty;
    let program = &command[0];
    let prog_args = &command[1..];

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(program);
    for a in prog_args {
        cmd.arg(a);
    }
    if let Some(dir) = cwd {
        if std::path::Path::new(dir).is_dir() {
            cmd.cwd(dir);
        }
    }
    let mut child = pair.slave.spawn_command(cmd).expect("spawn shell");
    drop(pair.slave); // close our copy of the slave so EOF propagates correctly

    // Watch the shell directly: when it exits (e.g. the user typed `exit`), tear
    // the whole host down so our stdout closes and the browser, seeing that EOF,
    // closes the tab — just like pressing `x`. We CANNOT rely on the PTY master
    // read returning EOF here: ConPTY keeps the master open after the child exits
    // (it only unblocks on ClosePseudoConsole), so without this watcher `exit`
    // would hang at a dead prompt forever.
    std::thread::spawn(move || {
        let _ = child.wait();
        std::process::exit(0);
    });

    // PTY output → our stdout.
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        // Big reads: a flood of output crosses to the browser in fewer, larger writes.
        let mut buf = vec![0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
            }
        }
    });

    // Our stdin (framed) → PTY input / resize.
    let mut writer = pair.master.take_writer().expect("pty writer");
    let master = pair.master;
    let mut stdin = std::io::stdin();
    let mut header = [0u8; 5];
    while stdin.read_exact(&mut header).is_ok() {
        let kind = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if stdin.read_exact(&mut payload).is_err() {
            break;
        }
        match kind {
            0 => {
                let _ = writer.write_all(&payload);
                let _ = writer.flush();
            }
            1 if payload.len() == 4 => {
                let cols = u16::from_le_bytes([payload[0], payload[1]]);
                let rows = u16::from_le_bytes([payload[2], payload[3]]);
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            _ => {}
        }
    }

    // The browser closed our stdin (it's gone). Exit; the child watcher and the
    // browser's kill-on-close job object both also reap the shell + conhost, so
    // graceful ConPTY teardown is never required.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn options_come_before_the_program_in_any_order() {
        let a = args("host 100 30 --cwd C:\\x --conpty D:\\c pwsh -NoLogo");
        let p = parse_args(&a).unwrap();
        assert_eq!((p.cols, p.rows), (100, 30));
        assert_eq!(p.cwd, Some("C:\\x"));
        assert_eq!(p.conpty, Some("D:\\c"));
        assert_eq!(p.command, &a[7..]);

        let a = args("host 80 24 --conpty D:\\c --cwd C:\\x cmd");
        let p = parse_args(&a).unwrap();
        assert_eq!((p.cwd, p.conpty), (Some("C:\\x"), Some("D:\\c")));
        assert_eq!(p.command, &a[7..]);

        let a = args("host 80 24 nu");
        let p = parse_args(&a).unwrap();
        assert_eq!((p.cwd, p.conpty), (None, None));
        assert_eq!(p.command, &a[3..]);
        // A program named like an option still runs when nothing follows it.
        assert_eq!(
            parse_args(&args("host 80 24 --cwd")).unwrap().command,
            &["--cwd".to_string()]
        );
        assert_eq!(parse_args(&args("host 80 24")), None);
    }
}
