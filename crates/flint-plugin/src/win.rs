//! Windows confinement for plugin processes.
//!
//! * **Low integrity token.** The process cannot write to the user's files,
//!   the registry or most of the profile (only `LocalLow`-style locations),
//!   and cannot send messages to higher-integrity windows (UIPI). It can
//!   still *read* what the user can read.
//! * **Job Object.** Memory limit, a hard 25% CPU cap, killed when Flint
//!   closes the job (including if Flint crashes), and exactly one process
//!   unless the plugin was granted `launch_processes`. UI restrictions block
//!   the clipboard, global atoms, desktop switching, system-parameter
//!   changes and handles to windows outside the job.
//! * **Explicit handle inheritance.** Only the plugin's pipe ends and NUL
//!   are inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
//!
//! If the low-integrity launch fails, the plugin starts at normal integrity
//! inside the same Job Object. That fallback is recorded in
//! [`Isolation::note`] and shown in diagnostics. Network access is not
//! restricted (that would need an AppContainer; see ADR-019).
//!
//! Status: compile-checked and run under Wine (which does not enforce
//! integrity levels); not validated on Windows hardware.

use crate::host::{Isolation, Launch, Process, Spawned};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::*;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

impl Handle {
    fn take(mut self) -> HANDLE {
        std::mem::take(&mut self.0)
    }
}

struct Confined {
    process: HANDLE,
    job: HANDLE,
}

// SAFETY: process and job handles may be used and closed from any thread.
unsafe impl Send for Confined {}

impl Process for Confined {
    fn kill(&mut self) {
        unsafe {
            if !self.job.0.is_null() {
                // Ends the plugin and anything it started.
                let _ = TerminateJobObject(self.job, 1);
                let _ = CloseHandle(self.job);
                self.job = HANDLE::default();
            }
            if !self.process.0.is_null() {
                let _ = TerminateProcess(self.process, 1);
                WaitForSingleObject(self.process, 2000);
                let _ = CloseHandle(self.process);
                self.process = HANDLE::default();
            }
        }
    }
}

impl Drop for Confined {
    fn drop(&mut self) {
        self.kill();
    }
}

fn err(e: windows::core::Error) -> io::Error {
    io::Error::other(e.message())
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Environment block (sorted, `K=V\0…\0\0`) with the launch's additions.
fn env_block(extra: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).filter(|(k, _)| !extra.iter().any(|(e, _)| e.eq_ignore_ascii_case(k))).collect();
    vars.extend(extra.iter().cloned());
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut b = Vec::new();
    for (k, v) in vars {
        b.extend(format!("{k}={v}").encode_utf16());
        b.push(0);
    }
    b.push(0);
    b
}

/// A primary token like ours, at low integrity.
unsafe fn low_integrity_token() -> windows::core::Result<Handle> {
    let mut own = HANDLE::default();
    OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY, &mut own)?;
    let own = Handle(own);
    let mut dup = HANDLE::default();
    DuplicateTokenEx(own.0, TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY, None, SecurityImpersonation, TokenPrimary, &mut dup)?;
    let dup = Handle(dup);
    let mut sid = PSID::default();
    ConvertStringSidToSidW(windows::core::w!("S-1-16-4096"), &mut sid)?;
    let label = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: sid, Attributes: 0x20 /* SE_GROUP_INTEGRITY */ } };
    let r = SetTokenInformation(dup.0, TokenIntegrityLevel, &label as *const _ as *const _, (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + GetLengthSid(sid) as usize) as u32);
    LocalFree(Some(HLOCAL(sid.0)));
    r?;
    Ok(dup)
}

unsafe fn job_for(l: &Launch, notes: &mut Vec<String>) -> io::Result<Handle> {
    let job = Handle(CreateJobObjectW(None, PCWSTR::null()).map_err(err)?);
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if l.memory_limit > 0 {
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = l.memory_limit as usize;
    }
    if !l.allow_children {
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
    }
    SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32).map_err(err)?;
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS | JOB_OBJECT_UILIMIT_EXITWINDOWS | JOB_OBJECT_UILIMIT_GLOBALATOMS | JOB_OBJECT_UILIMIT_HANDLES | JOB_OBJECT_UILIMIT_READCLIPBOARD | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    if SetInformationJobObject(job.0, JobObjectBasicUIRestrictions, &ui as *const _ as *const _, std::mem::size_of_val(&ui) as u32).is_err() {
        notes.push("UI limits unavailable".into());
    }
    if l.cpu_percent > 0 {
        let mut cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION { ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, ..Default::default() };
        cpu.Anonymous.CpuRate = l.cpu_percent.clamp(1, 100) * 100;
        if SetInformationJobObject(job.0, JobObjectCpuRateControlInformation, &cpu as *const _ as *const _, std::mem::size_of_val(&cpu) as u32).is_err() {
            notes.push("CPU cap unavailable".into());
        }
    }
    Ok(job)
}

unsafe fn spawn(l: &Launch, low: bool) -> io::Result<Spawned> {
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: true.into() };
    let (mut in_r, mut in_w, mut out_r, mut out_w) = (HANDLE::default(), HANDLE::default(), HANDLE::default(), HANDLE::default());
    CreatePipe(&mut in_r, &mut in_w, Some(&sa), 0).map_err(err)?;
    let (in_r, in_w) = (Handle(in_r), Handle(in_w));
    CreatePipe(&mut out_r, &mut out_w, Some(&sa), 0).map_err(err)?;
    let (out_r, out_w) = (Handle(out_r), Handle(out_w));
    // Our ends stay private.
    SetHandleInformation(in_w.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(err)?;
    SetHandleInformation(out_r.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(err)?;
    let nul = Handle(CreateFileW(windows::core::w!("NUL"), GENERIC_WRITE.0, FILE_SHARE_READ | FILE_SHARE_WRITE, Some(&sa), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None).map_err(err)?);

    let inherit = [in_r.0, out_w.0, nul.0];
    let mut size = 0usize;
    let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
    let mut attr_buf = vec![0u8; size.max(64)];
    let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut _);
    InitializeProcThreadAttributeList(Some(attrs), 1, None, &mut size).map_err(err)?;
    struct AttrList(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attrs_guard = AttrList(attrs);
    UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, Some(inherit.as_ptr() as *const _), std::mem::size_of_val(&inherit), None, None).map_err(err)?;

    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = in_r.0;
    si.StartupInfo.hStdOutput = out_w.0;
    si.StartupInfo.hStdError = nul.0;
    si.lpAttributeList = attrs;

    let exe = wide(l.exe.as_os_str());
    let mut cmd: Vec<u16> = std::iter::once(b'"' as u16).chain(l.exe.as_os_str().encode_wide()).chain([b'"' as u16, 0]).collect();
    let dir = wide(l.dir.as_os_str());
    let env = env_block(&l.env);
    let flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
    let mut pi = PROCESS_INFORMATION::default();
    if low {
        let token = low_integrity_token().map_err(err)?;
        CreateProcessAsUserW(Some(token.0), PCWSTR(exe.as_ptr()), Some(PWSTR(cmd.as_mut_ptr())), None, None, true, flags, Some(env.as_ptr() as *const _), PCWSTR(dir.as_ptr()), &si.StartupInfo, &mut pi).map_err(err)?;
    } else {
        CreateProcessW(PCWSTR(exe.as_ptr()), Some(PWSTR(cmd.as_mut_ptr())), None, None, true, flags, Some(env.as_ptr() as *const _), PCWSTR(dir.as_ptr()), &si.StartupInfo, &mut pi).map_err(err)?;
    }
    let (process, thread) = (Handle(pi.hProcess), Handle(pi.hThread));
    let mut notes = Vec::new();
    let job = match job_for(l, &mut notes) {
        Ok(j) if AssignProcessToJobObject(j.0, process.0).is_ok() => j,
        r => {
            // Never run a plugin outside its job.
            let _ = TerminateProcess(process.0, 1);
            return Err(match r {
                Err(e) => e,
                Ok(_) => io::Error::other("cannot place the plugin in a Job Object"),
            });
        }
    };
    ResumeThread(thread.0);
    drop((in_r, out_w, nul, thread));
    let stdin = File::from_raw_handle(in_w.take().0 as _);
    let stdout = File::from_raw_handle(out_r.take().0 as _);
    Ok(Spawned {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        process: Box::new(Confined { process: process.take(), job: job.take() }),
        isolation: Isolation { low_integrity: low, job_object: true, note: notes.join(", ") },
    })
}

/// Low integrity in a Job Object; normal integrity in a Job Object if the
/// low-integrity launch is refused (recorded in the isolation note).
pub fn spawn_confined(l: &Launch) -> io::Result<Spawned> {
    unsafe {
        match spawn(l, true) {
            Ok(s) => Ok(s),
            Err(low_err) => {
                let mut s = spawn(l, false)?;
                let extra = format!("low integrity refused: {low_err}");
                s.isolation.note = if s.isolation.note.is_empty() { extra } else { format!("{}, {extra}", s.isolation.note) };
                Ok(s)
            }
        }
    }
}
