//! Total the browser's real resource footprint across its WHOLE process tree —
//! `browser.exe` plus its WebView2 engine processes (broker / renderer / GPU /
//! utility / crashpad) and any `browser-pty-host` companions.
//!
//! Task Manager won't sum these for us: the Edge WebView2 runtime gives its broker
//! its own application identity, so Win11 Task Manager files the engine processes
//! under a separate "WebView2 Manager" group instead of nesting them under us
//! (even though they ARE our child processes). The `:res` command uses this to
//! show the true per-process breakdown + grand total, refreshed live.

/// One process in the browser's tree at a moment in time. Memory is an absolute
/// reading; `cpu_100ns` and `io_bytes` are cumulative counters, so CPU% and disk
/// rate are derived from their delta between two samples.
pub struct ProcSample {
    pub name: String,
    pub pid: u32,
    /// Working-set size in bytes (resident physical memory).
    pub working_set: u64,
    /// Total CPU time used so far (kernel + user), in 100-nanosecond units.
    pub cpu_100ns: u64,
    /// Cumulative I/O transferred (read + write), in bytes.
    pub io_bytes: u64,
}

/// Number of logical processors, for turning CPU-time deltas into a percentage of
/// total machine CPU (matching Task Manager's column).
pub fn cpu_count() -> u64 {
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

/// Sample every process in this process's tree (self + all descendants), sorted
/// by memory (largest first). Empty if the snapshot fails.
#[cfg(windows)]
pub fn tree_sample() -> Vec<ProcSample> {
    use std::collections::HashMap;
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // Snapshot all processes → (pid, parent pid, exe name).
    let mut procs: Vec<(u32, u32, String)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                procs.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }

    // Index pids → children pids.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut name_of: HashMap<u32, &str> = HashMap::new();
    for (pid, parent, name) in &procs {
        children.entry(*parent).or_default().push(*pid);
        name_of.insert(*pid, name.as_str());
    }

    // Walk descendants from our own pid (inclusive). A visited set guards against
    // any parent-pid cycle from PID reuse.
    let root = std::process::id();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    let mut out = Vec::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(name) = name_of.get(&pid) {
            let (working_set, cpu_100ns, io_bytes) = metrics(pid).unwrap_or((0, 0, 0));
            out.push(ProcSample { name: (*name).to_string(), pid, working_set, cpu_100ns, io_bytes });
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    out.sort_by(|a, b| b.working_set.cmp(&a.working_set));
    out
}

/// `(working_set, cpu_100ns, io_bytes)` for a process, or `None` if it can't be
/// opened/queried (e.g. it just exited).
#[cfg(windows)]
fn metrics(pid: u32) -> Option<(u64, u64, u64)> {
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        GetProcessIoCounters, GetProcessTimes, OpenProcess, IO_COUNTERS,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut mem = PROCESS_MEMORY_COUNTERS::default();
        let working_set =
            match GetProcessMemoryInfo(handle, &mut mem, size_of::<PROCESS_MEMORY_COUNTERS>() as u32)
            {
                Ok(()) => mem.WorkingSetSize as u64,
                Err(_) => 0,
            };

        let (mut create, mut exit, mut kernel, mut user) = Default::default();
        let cpu = match GetProcessTimes(handle, &mut create, &mut exit, &mut kernel, &mut user) {
            Ok(()) => ft(kernel) + ft(user),
            Err(_) => 0,
        };

        let mut io = IO_COUNTERS::default();
        let io_bytes = match GetProcessIoCounters(handle, &mut io) {
            Ok(()) => io.ReadTransferCount + io.WriteTransferCount,
            Err(_) => 0,
        };

        let _ = CloseHandle(handle);
        Some((working_set, cpu, io_bytes))
    }
}

#[cfg(not(windows))]
pub fn tree_sample() -> Vec<ProcSample> {
    Vec::new()
}

/// Human-readable byte size: MB with one decimal, GB with two past 1024 MB.
pub fn fmt_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.1} MB")
    }
}

/// Human-readable transfer rate: scales B/s → KB/s → MB/s.
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.0} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec.max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_scales() {
        assert_eq!(fmt_bytes(0), "0.0 MB");
        assert_eq!(fmt_bytes(15 * 1024 * 1024), "15.0 MB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.00 GB");
    }

    #[test]
    fn fmt_rate_scales() {
        assert_eq!(fmt_rate(0.0), "0 B/s");
        assert_eq!(fmt_rate(2048.0), "2 KB/s");
        assert_eq!(fmt_rate(3.0 * 1024.0 * 1024.0), "3.0 MB/s");
    }

    #[cfg(windows)]
    #[test]
    fn sample_includes_self_with_memory() {
        let sample = tree_sample();
        let me = sample.iter().find(|p| p.pid == std::process::id()).expect("self in sample");
        assert!(me.working_set > 0, "own working set should be > 0");
    }
}
