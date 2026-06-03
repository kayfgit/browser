//! browser-pty-host — a tiny companion process that owns a Windows ConPTY (or
//! platform PTY) and bridges it to the browser over stdin/stdout. It exists so
//! the ConPTY lives OUT of the browser process: a live ConPTY leaves a thread
//! stuck in an uninterruptible kernel read that deadlocks process exit, so we
//! isolate it here. The browser force-kills this process (via a kill-on-close
//! job object) when the tab closes or the browser exits — this process never
//! needs to shut the ConPTY down gracefully.
//!
//! Protocol:
//!   * argv: `<cols> <rows> <program> [args...]`
//!   * browser → our stdin: frames `[type:u8][len:u32 LE][payload]`;
//!     type 0 = input bytes to write to the PTY; type 1 = resize with payload
//!     `[cols:u16 LE][rows:u16 LE]`.
//!   * our stdout ← PTY: raw output bytes (the browser base64s and feeds xterm).
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: browser-pty-host <cols> <rows> <program> [args...]");
        std::process::exit(2);
    }
    let cols: u16 = args[1].parse().unwrap_or(80);
    let rows: u16 = args[2].parse().unwrap_or(24);
    let program = &args[3];
    let prog_args = &args[4..];

    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(program);
    for a in prog_args {
        cmd.arg(a);
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
        let mut buf = [0u8; 8192];
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
                let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            }
            _ => {}
        }
    }

    // The browser closed our stdin (it's gone). Exit; the child watcher and the
    // browser's kill-on-close job object both also reap the shell + conhost, so
    // graceful ConPTY teardown is never required.
    std::process::exit(0);
}
