//! Read another process's live working directory — the no-setup fallback for
//! saving a terminal's cwd on `:w`/`:wq`. Shells that keep their physical cwd in
//! sync with `cd` (nushell, cmd — verified live; bash too) need no shell
//! integration at all: at save time we walk the pty-host's process tree to the
//! deepest live descendant (the innermost shell/program — a cmd nested inside nu
//! wins over nu) and read its `PEB → ProcessParameters → CurrentDirectory`.
//! PowerShell famously does NOT sync its process cwd on `Set-Location`, and WSL's
//! Linux-side cwd is invisible from Windows — both of those need the OSC 9;9
//! prompt hook instead (see `pty_term`), which always takes precedence.

/// The working directory of the deepest live process under (not including)
/// `host_pid` — the innermost shell/program of that terminal. ConPTY's own
/// conhost is skipped (its cwd is frozen at spawn). Candidates are tried
/// deepest-first, so a transient child that died between snapshot and read just
/// falls back to its parent. `None` when nothing readable is left.
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) fn shell_cwd(host_pid: u32) -> Option<String> {
    use std::collections::HashMap;

    // Snapshot all processes → (pid, parent, exe), then index children.
    let procs = snapshot();
    let mut children: HashMap<u32, Vec<(u32, &str)>> = HashMap::new();
    for (pid, parent, name) in &procs {
        children.entry(*parent).or_default().push((*pid, name.as_str()));
    }

    // Collect the host's descendants with their depth (a visited set guards
    // against parent-pid cycles from PID reuse).
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<(u32, u32)> = vec![(host_pid, 0)];
    let mut candidates: Vec<(u32, u32)> = Vec::new(); // (depth, pid)
    while let Some((pid, depth)) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        for (kid, name) in children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]) {
            stack.push((*kid, depth + 1));
            let n = name.to_ascii_lowercase();
            if n != "conhost.exe" && n != "openconsole.exe" {
                candidates.push((depth + 1, *kid));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0)); // deepest first
    candidates.iter().find_map(|&(_, pid)| process_cwd(pid))
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
pub(crate) fn shell_cwd(_host_pid: u32) -> Option<String> {
    None
}

/// Snapshot every process as `(pid, parent pid, exe name)`.
#[cfg(all(windows, target_pointer_width = "64"))]
fn snapshot() -> Vec<(u32, u32, String)> {
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut procs = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return procs;
        };
        let mut entry =
            PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end =
                    entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                procs.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    procs
}

/// `NtQueryInformationProcess(ProcessBasicInformation)` result, x64 layout (the
/// 4-byte NTSTATUS/KPRIORITY fields pad to pointer size).
#[cfg(all(windows, target_pointer_width = "64"))]
#[repr(C)]
struct Pbi {
    exit_status: isize,
    peb: usize,
    affinity: usize,
    priority: isize,
    pid: usize,
    ppid: usize,
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[link(name = "ntdll")]
extern "system" {
    /// Class 0 = ProcessBasicInformation. Declared by hand — the `windows` crate
    /// keeps this behind the separate Wdk feature set, which nothing else needs.
    fn NtQueryInformationProcess(
        h: isize,
        class: u32,
        info: *mut core::ffi::c_void,
        len: u32,
        ret: *mut u32,
    ) -> i32;
}

/// The live working directory of process `pid`, read from its PEB. The x64
/// offsets (`PEB+0x20` = ProcessParameters, `+0x38` = CurrentDirectory.DosPath)
/// are the long-stable documented layout every process tool relies on. `None`
/// for a dead/protected process or a 32-bit (WOW64) one, whose real parameters
/// live in its 32-bit PEB.
#[cfg(all(windows, target_pointer_width = "64"))]
fn process_cwd(pid: u32) -> Option<String> {
    use core::ffi::c_void;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
        let read = |addr: usize, buf: &mut [u8]| -> bool {
            let mut n = 0usize;
            ReadProcessMemory(
                h,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                Some(&mut n),
            )
            .is_ok()
                && n == buf.len()
        };
        let result = (|| {
            let mut pbi = std::mem::zeroed::<Pbi>();
            let st = NtQueryInformationProcess(
                h.0 as isize,
                0,
                &mut pbi as *mut _ as *mut c_void,
                std::mem::size_of::<Pbi>() as u32,
                std::ptr::null_mut(),
            );
            if st < 0 || pbi.peb == 0 {
                return None;
            }
            let mut p = [0u8; 8];
            if !read(pbi.peb + 0x20, &mut p) {
                return None;
            }
            let params = usize::from_le_bytes(p);
            // CurrentDirectory.DosPath is a UNICODE_STRING: len u16, cap u16,
            // 4 bytes padding, then the buffer pointer.
            let mut us = [0u8; 16];
            if !read(params + 0x38, &mut us) {
                return None;
            }
            let len = u16::from_le_bytes([us[0], us[1]]) as usize;
            let buf_ptr = usize::from_le_bytes(us[8..16].try_into().ok()?);
            if len == 0 || len > 0x8000 || buf_ptr == 0 {
                return None;
            }
            let mut raw = vec![0u8; len];
            if !read(buf_ptr, &mut raw) {
                return None;
            }
            let wide: Vec<u16> =
                raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let mut path = String::from_utf16_lossy(&wide);
            // The PEB stores "C:\dir\" with a trailing slash; drop it (but keep
            // a drive root's — "C:\" wouldn't be a valid dir without it).
            if path.len() > 3 && path.ends_with('\\') {
                path.pop();
            }
            (!path.is_empty()).then_some(path)
        })();
        let _ = CloseHandle(h);
        result
    }
}

#[cfg(all(test, windows, target_pointer_width = "64"))]
mod tests {
    use super::*;

    /// End-to-end over a real child: spawn cmd in a known directory and read it
    /// back through the snapshot walk + PEB machinery (cmd keeps its process cwd
    /// in sync, like nushell — the shells this fallback exists for).
    #[test]
    fn reads_a_live_child_shells_working_directory() {
        let dir = "C:\\Windows";
        let mut child = std::process::Command::new("cmd")
            .arg("/k")
            .arg("echo ready")
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn cmd");
        std::thread::sleep(std::time::Duration::from_millis(800));
        // The walk starts at OUR pid: cmd is our (non-conhost) descendant.
        let got = shell_cwd(std::process::id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got.as_deref().map(str::to_ascii_lowercase), Some(dir.to_ascii_lowercase()));
    }
}
