//! Starting a Trident helper in a sandbox.
//!
//! MSHTML has no sandbox of its own once it's hosted outside Internet Explorer, and it's
//! a frequent exploit target. So each helper runs in an **AppContainer** (the sandbox
//! Edge's legacy IE mode and store apps use): it can reach the network, but not the
//! user's files, the browser's profile or other engines' cookies; it can't start other
//! programs; and it lives in a job that dies with the browser.
use std::{fs::File, os::windows::io::FromRawHandle, path::Path};

use anyhow::Context;
use windows::{
    core::{w, HRESULT, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, HANDLE,
            HANDLE_FLAG_INHERIT, HLOCAL,
        },
        Security::{
            Authorization::ConvertStringSidToSidW,
            FreeSid,
            Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName},
            PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        },
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Pipes::CreatePipe,
            Threading::{
                CreateProcessW, InitializeProcThreadAttributeList, ResumeThread,
                UpdateProcThreadAttribute, CREATE_NO_WINDOW, CREATE_SUSPENDED,
                EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            },
        },
    },
};

/// The AppContainer every Trident helper runs in.
const CONTAINER: PCWSTR = w!("kayf.browser.trident");
/// What the container may do: reach the internet (`internetClient`), and intranet and
/// local servers (`privateNetworkClientServer`), the old sites Trident is for.
const CAPABILITIES: [&str; 2] = ["S-1-15-3-1", "S-1-15-3-3"];
/// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED`: the helper can't start programs.
const NO_CHILD_PROCESSES: u32 = 0x1;
/// IE11 standards mode for pages without a `<!DOCTYPE>`-forced mode (the control's
/// default is IE7). Pages can still ask for older modes with `X-UA-Compatible`.
const IE11_EMULATION: u32 = 11001;

/// A running helper: its process and the two ends of its pipes.
pub(crate) struct Helper {
    pub process: HANDLE,
    pub stdin: File,
    pub stdout: File,
}

/// Start a sandboxed helper.
pub(crate) fn spawn() -> anyhow::Result<Helper> {
    let exe = std::env::current_exe().context("the browser's own path")?;
    set_document_mode(&exe);
    let container = container_sid()?;
    // No file permissions to grant: Windows opens the executable for the new process
    // with the browser's own rights, so it starts even from the user's own folders.
    unsafe { start(&exe, container.0) }.context("starting the Trident sandbox")
}

/// The container's SID, creating its profile on first use.
struct Sid(PSID);
impl Drop for Sid {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeSid(self.0);
        }
    }
}

fn container_sid() -> anyhow::Result<Sid> {
    unsafe {
        match CreateAppContainerProfile(
            CONTAINER,
            w!("browser: Trident"),
            w!("Sandbox for the browser's Internet Explorer (Trident) engine"),
            None,
        ) {
            Ok(sid) => Ok(Sid(sid)),
            Err(e) if e.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) => {
                Ok(Sid(DeriveAppContainerSidFromAppContainerName(CONTAINER)?))
            }
            Err(e) => Err(e).context("creating the Trident sandbox"),
        }
    }
}

/// Owned kernel handle, closed on drop.
struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

unsafe fn start(exe: &Path, container: PSID) -> windows::core::Result<Helper> {
    // Pipes: the child's ends are inheritable, ours aren't.
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut child_in, mut our_in) = (HANDLE::default(), HANDLE::default());
    CreatePipe(&mut child_in, &mut our_in, Some(&inherit), 0)?;
    let (child_in, our_in) = (Owned(child_in), Owned(our_in));
    let (mut our_out, mut child_out) = (HANDLE::default(), HANDLE::default());
    CreatePipe(&mut our_out, &mut child_out, Some(&inherit), 0)?;
    let (our_out, child_out) = (Owned(our_out), Owned(child_out));
    SetHandleInformation(our_in.0, HANDLE_FLAG_INHERIT.0, Default::default())?;
    SetHandleInformation(our_out.0, HANDLE_FLAG_INHERIT.0, Default::default())?;

    let mut sids: Vec<PSID> = Vec::new();
    for s in CAPABILITIES {
        let mut sid = PSID::default();
        ConvertStringSidToSidW(&HSTRING::from(s), &mut sid)?;
        sids.push(sid);
    }
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = sids
        .iter()
        .map(|&sid| SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0x4, // SE_GROUP_ENABLED
        })
        .collect();
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: container,
        Capabilities: capabilities.as_mut_ptr(),
        CapabilityCount: capabilities.len() as u32,
        Reserved: 0,
    };
    // Only the two pipe ends are inherited, never anything else this process holds.
    let handles = [child_in.0, child_out.0];
    let policy = NO_CHILD_PROCESSES;

    let mut size = 0usize;
    let _ = InitializeProcThreadAttributeList(None, 3, None, &mut size);
    let mut buffer = vec![0u8; size];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
    InitializeProcThreadAttributeList(Some(list), 3, None, &mut size)?;
    UpdateProcThreadAttribute(
        list,
        0,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
        Some((&security as *const SECURITY_CAPABILITIES).cast()),
        std::mem::size_of::<SECURITY_CAPABILITIES>(),
        None,
        None,
    )?;
    UpdateProcThreadAttribute(
        list,
        0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        Some(handles.as_ptr().cast()),
        std::mem::size_of_val(&handles),
        None,
        None,
    )?;
    UpdateProcThreadAttribute(
        list,
        0,
        PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
        Some((&policy as *const u32).cast()),
        std::mem::size_of::<u32>(),
        None,
        None,
    )?;

    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    info.StartupInfo.hStdInput = child_in.0;
    info.StartupInfo.hStdOutput = child_out.0;
    info.StartupInfo.hStdError = child_out.0;
    info.lpAttributeList = list;

    let mut command: Vec<u16> = format!("\"{}\" {}", exe.display(), super::HOST_FLAG)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // The working folder must be one the container can open.
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut process = PROCESS_INFORMATION::default();
    let created = CreateProcessW(
        &HSTRING::from(exe.as_os_str()),
        Some(PWSTR(command.as_mut_ptr())),
        None,
        None,
        true,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
        None,
        &HSTRING::from(system),
        &info.StartupInfo,
        &mut process,
    );
    for sid in sids {
        let _ = LocalFree(Some(HLOCAL(sid.0)));
    }
    created?;
    let thread = Owned(process.hThread);
    if let Some(job) = job() {
        let _ = AssignProcessToJobObject(job, process.hProcess);
    }
    ResumeThread(thread.0);

    // Hand our pipe ends to `File`s, which close them.
    let stdin = File::from_raw_handle(our_in.0 .0);
    let stdout = File::from_raw_handle(our_out.0 .0);
    std::mem::forget(our_in);
    std::mem::forget(our_out);
    Ok(Helper {
        process: process.hProcess,
        stdin,
        stdout,
    })
}

/// The job every helper joins: it ends them all when the browser exits, however it
/// exits, and a helper that crashes just dies (no Windows error-reporting dialog).
fn job() -> Option<HANDLE> {
    static JOB: std::sync::OnceLock<Option<isize>> = std::sync::OnceLock::new();
    let raw = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        // Held for the life of the process: closing it would end every helper.
        Some(job.0 as isize)
    });
    raw.map(|h| HANDLE(h as *mut _))
}

/// MSHTML's per-executable switches (per user; harmless to repeat): IE11 standards
/// mode, and pointer events, which the hosted control leaves off ("legacy input") but
/// the shell bridge uses to see clicks.
fn set_document_mode(exe: &Path) {
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};
    let Some(name) = exe.file_name() else { return };
    let name = HSTRING::from(name);
    for (feature, value) in [
        (
            w!(
                r"Software\Microsoft\Internet Explorer\Main\FeatureControl\FEATURE_BROWSER_EMULATION"
            ),
            IE11_EMULATION,
        ),
        (
            w!(
                r"Software\Microsoft\Internet Explorer\Main\FeatureControl\FEATURE_NINPUT_LEGACYMODE"
            ),
            0,
        ),
    ] {
        unsafe {
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                feature,
                &name,
                REG_DWORD.0,
                Some((&value as *const u32).cast()),
                4,
            );
        }
    }
}
