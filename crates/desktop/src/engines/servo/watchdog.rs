//! Notices Servo content processes that die. Servo itself reports a panicking page
//! (`notify_crashed`), but a hard crash (stack overflow, access violation, a killed
//! process) leaves its pages silently dead. Windows tells a job object about every
//! process that starts or exits inside it, so this costs nothing while pages run.
//!
//! The browser joins a limit-free job when Servo first starts; children (Servo's
//! content processes, and the WebView2 runtime) inherit it. The job only observes:
//! it imposes no limits, allows breakaway, and isn't killed when its handle closes.
//!
//! Each content process also goes into a second, nested job that is killed when its
//! last handle closes. Only this browser holds that handle, so however the browser
//! ends (quit, the shutdown limit, a crash, being killed), its content processes end
//! with it instead of lingering. Nothing else is put in that job.
use super::{post, Event};
use crate::UserEvent;
use std::collections::HashMap;
use tao::event_loop::EventLoopProxy;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW,
            JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            GetCurrentProcess, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
            PROCESS_TERMINATE,
        },
        IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED},
    },
};

// From SystemServices, a very large module this is the only use of.
const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;

/// Start watching. Call once, before Servo can spawn a content process.
pub(super) fn start(proxy: EventLoopProxy<UserEvent>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    unsafe {
        let job =
            CreateJobObjectW(None, windows::core::PCWSTR::null()).map_err(|e| e.to_string())?;
        let port =
            CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1).map_err(|e| e.to_string())?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // WebView2 may ask to start processes outside the job; let it.
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: std::ptr::null_mut(),
            CompletionPort: port,
        };
        let setup = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of_val(&limits) as u32,
        )
        .and_then(|()| {
            SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                &associate as *const _ as *const _,
                std::mem::size_of_val(&associate) as u32,
            )
        })
        .and_then(|()| AssignProcessToJobObject(job, GetCurrentProcess()));
        if let Err(error) = setup {
            let _ = CloseHandle(port);
            let _ = CloseHandle(job);
            return Err(error.to_string());
        }
        // Without it pages still run and crashes are still noticed; only cleanup after
        // an abrupt exit is lost.
        let reaper = reaper_job()
            .inspect_err(|e| super::smoke::log(&format!("Servo content cleanup unavailable: {e}")))
            .ok();
        // All handles live as long as the browser: the thread owns the port, the
        // observing job must outlive every process in it to keep reporting them, and
        // closing the reaper job (at exit) is what ends the content processes.
        let (job, port) = (job.0 as usize, port.0 as usize);
        let reaper = reaper.map(|r| r.0 as usize);
        std::thread::Builder::new()
            .name("servo-watchdog".into())
            .spawn(move || {
                let _job = job;
                let reaper = reaper.map(|r| HANDLE(r as *mut _));
                watch(HANDLE(port as *mut _), reaper, &exe, &proxy)
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A job that ends every process in it when its last handle closes.
unsafe fn reaper_job() -> windows::core::Result<HANDLE> {
    let job = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if let Err(error) = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &limits as *const _ as *const _,
        std::mem::size_of_val(&limits) as u32,
    ) {
        let _ = CloseHandle(job);
        return Err(error);
    }
    Ok(job)
}

fn watch(
    port: HANDLE,
    reaper: Option<HANDLE>,
    exe: &std::path::Path,
    proxy: &EventLoopProxy<UserEvent>,
) {
    // Content processes by ID, each with a handle opened while it ran, so its exit
    // code stays readable after it exits (and its ID can't be reused meanwhile).
    let mut content: HashMap<u32, HANDLE> = HashMap::new();
    loop {
        let (mut message, mut key, mut overlapped) =
            (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
        if unsafe {
            GetQueuedCompletionStatus(port, &mut message, &mut key, &mut overlapped, u32::MAX)
        }
        .is_err()
        {
            return;
        }
        // For job notifications the "overlapped" pointer carries the process ID.
        let pid = overlapped as usize as u32;
        match message {
            JOB_OBJECT_MSG_NEW_PROCESS => {
                if let Some(process) = open_if_ours(pid, exe) {
                    if let Some(reaper) = reaper {
                        if let Err(error) = unsafe { AssignProcessToJobObject(reaper, process) } {
                            super::smoke::log(&format!("Servo content cleanup: {error}"));
                        }
                    }
                    content.insert(pid, process);
                }
            }
            JOB_OBJECT_MSG_EXIT_PROCESS | JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => {
                let Some(process) = content.remove(&pid) else {
                    continue;
                };
                let mut code = 0u32;
                let read = unsafe { GetExitCodeProcess(process, &mut code) }.is_ok();
                unsafe {
                    let _ = CloseHandle(process);
                }
                // A content process with no pages left exits normally, with code 0.
                if message == JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS || (read && code != 0) {
                    post(proxy, Event::ContentCrashed { code });
                }
            }
            _ => {}
        }
    }
}

/// A handle to `pid` if it runs this browser's executable: Servo's content
/// processes are copies of it, while WebView2's and the terminal helper are not.
fn open_if_ours(pid: u32, exe: &std::path::Path) -> Option<HANDLE> {
    unsafe {
        // Joining the reaper job needs quota and terminate rights.
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE;
        let process = OpenProcess(access, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let ours = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut len,
        )
        .is_ok()
            && String::from_utf16_lossy(&path[..len as usize])
                .eq_ignore_ascii_case(&exe.to_string_lossy());
        if ours {
            Some(process)
        } else {
            let _ = CloseHandle(process);
            None
        }
    }
}
