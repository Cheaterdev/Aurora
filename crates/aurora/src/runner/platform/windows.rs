//! Windows: PATHEXT lookup, `.cmd`/`.bat` shims, no console window, a job
//! object per run that dies with the server, tree kill.
//!
//! The only place with `unsafe`: Win32 calls through `windows-sys`, each in a
//! small block with a safety note, behind the safe API the runner uses
//! ([`spawn`], [`Contained`], [`inspect`], [`kill_tree`]).

use std::ffi::OsString;
use std::os::windows::io::RawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, THREAD_SUSPEND_RESUME,
};

use super::{Inspect, Resolved};

/// `CREATE_NO_WINDOW`: the agent never gets a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `CREATE_SUSPENDED`: the initial thread waits until the process is in its job.
const CREATE_SUSPENDED: u32 = 0x0000_0004;
/// What `GetExitCodeProcess` says of a process that is still running.
const STILL_ACTIVE: u32 = 259;
/// What `OpenProcess` fails with for a pid that does not exist.
const ERROR_INVALID_PARAMETER: u32 = 87;

fn classify(path: &Path) -> Result<Resolved, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "exe" | "com" => Ok(Resolved {
            path: path.to_path_buf(),
            shim: false,
        }),
        // Batch files run through cmd.exe; Rust escapes their arguments for it
        // and refuses the ones it cannot escape safely.
        "bat" | "cmd" => Ok(Resolved {
            path: path.to_path_buf(),
            shim: true,
        }),
        other => Err(format!(
            "{} is not an executable (.exe, .com, .cmd or .bat), its extension is {other:?}",
            path.display()
        )),
    }
}

fn candidates(base: &Path, pathext: &[String]) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if base.extension().is_some() {
        list.push(base.to_path_buf());
    }
    for ext in pathext {
        let mut name: OsString = base.as_os_str().to_os_string();
        name.push(ext);
        list.push(PathBuf::from(name));
    }
    list
}

pub fn resolve_command(
    command: &str,
    path_var: Option<OsString>,
    pathext: Option<String>,
) -> Result<Resolved, String> {
    let pathext: Vec<String> = pathext
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|e| e.starts_with('.'))
        .map(str::to_string)
        .collect();
    let has_dir = command.contains(['\\', '/', ':']);
    let bases: Vec<PathBuf> = if has_dir {
        vec![PathBuf::from(command)]
    } else {
        std::env::split_paths(&path_var.unwrap_or_default())
            .map(|dir| dir.join(command))
            .collect()
    };
    for base in bases {
        for candidate in candidates(&base, &pathext) {
            if candidate.is_file() {
                return classify(&candidate);
            }
        }
    }
    Err(format!(
        "the command {command:?} was not found{}",
        if has_dir { "" } else { " in PATH" }
    ))
}

/// No console window; the child is not attached to the server's console.
pub fn prepare(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

// ---- containment: one job object per run ----
//
// Every agent process lives in a job object the server creates for that run
// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the server process ends, for
// whatever reason (a crash, a kill, power loss does not matter: the OS closes
// the handle), the OS terminates every process in the job. No breakaway flag
// is set (neither `BREAKAWAY_OK` nor `SILENT_BREAKAWAY_OK`), so a descendant
// cannot leave the job; one that asks for `CREATE_BREAKAWAY_FROM_JOB` is
// refused by the OS. The process starts suspended, is assigned to the job and
// only then resumed, so it cannot start a grandchild before it is contained.
// Nested jobs work on Windows 8 and later: a server that is itself inside a
// job (a terminal, an IDE, a service host) can still make its own.

/// A job object handle. Closing it kills what is still in the job.
struct Job(HANDLE);

// SAFETY: a job handle is an opaque kernel handle; the calls made on it
// (assign, terminate, close) are thread-safe.
unsafe impl Send for Job {}
// SAFETY: as above.
unsafe impl Sync for Job {}

impl Job {
    fn create() -> std::io::Result<Job> {
        // The environment hook of the tests: pretend job objects are not
        // available, to exercise the taskkill fallback. Compiled out of a
        // release build.
        if aurora_store::failpoints::compiled_in()
            && std::env::var_os("AURORA_TEST_NO_JOB").is_some_and(|v| v == "1")
        {
            return Err(std::io::Error::other(
                "job objects switched off by AURORA_TEST_NO_JOB",
            ));
        }
        // SAFETY: both arguments may be null (default security, no name).
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(handle);
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..JOBOBJECT_BASIC_LIMIT_INFORMATION::default()
            },
            ..JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default()
        };
        // SAFETY: `info` outlives the call and the length is its size.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, process: RawHandle) -> std::io::Result<()> {
        // SAFETY: both handles are valid for the call: the job is ours and the
        // process handle belongs to a live `Child`.
        if unsafe { AssignProcessToJobObject(self.0, process as HANDLE) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate(&self) {
        // SAFETY: the handle is valid until `drop`. Failing (nothing left in
        // the job) is fine.
        unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// How one run's process tree is held; see the notes above.
pub struct Contained {
    job: Option<Job>,
}

impl Contained {
    /// For logs and tests.
    pub fn kind(&self) -> &'static str {
        if self.job.is_some() {
            "job object"
        } else {
            "none (taskkill fallback)"
        }
    }

    /// Ends the whole tree now: the job is terminated (instant, and it includes
    /// every descendant), or `taskkill /T /F` when there is no job.
    pub async fn kill(&self, pid: u32) {
        match &self.job {
            Some(job) => job.terminate(),
            None => kill_tree(pid).await,
        }
    }

    /// The run's process ended by itself: whatever it left running in the job
    /// goes with it (the job's handle is dropped with this value as well).
    pub fn release(&self) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

/// Resumes the (single, suspended) initial thread of the process `pid`.
fn resume_process(pid: u32) -> std::io::Result<()> {
    // SAFETY: a snapshot of all threads; the handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut resumed = 0_u32;
    // SAFETY: `entry` has its size set, as the API demands, and outlives the calls.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening a thread by id; the handle is closed right after.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                // SAFETY: a valid thread handle with suspend/resume access.
                if unsafe { ResumeThread(thread) } != u32::MAX {
                    resumed += 1;
                }
                // SAFETY: closing the handle opened above.
                unsafe { CloseHandle(thread) };
            }
        }
        // SAFETY: as for the first call.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closing the snapshot handle opened above.
    unsafe { CloseHandle(snapshot) };
    if resumed == 0 {
        return Err(std::io::Error::other(
            "no thread of the process could be resumed",
        ));
    }
    Ok(())
}

/// Starts the agent's process inside a job object of its own (see the notes
/// above). When a job cannot be made or the process cannot be put in it (a
/// warning says why), the process runs without one and is stopped with
/// `taskkill /T /F`; it then does not die with the server.
pub fn spawn(
    command: &mut tokio::process::Command,
) -> std::io::Result<(tokio::process::Child, Contained)> {
    let job = match Job::create() {
        Ok(job) => job,
        Err(err) => {
            tracing::warn!(
                %err,
                "could not create a job object; the agent runs without one and will not die with the server"
            );
            command.creation_flags(CREATE_NO_WINDOW);
            return Ok((command.spawn()?, Contained { job: None }));
        }
    };
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let pid = child.id().unwrap_or(0);
    let assigned = match child.raw_handle() {
        Some(handle) => job.assign(handle),
        None => Err(std::io::Error::other("the process handle is not available")),
    };
    let contained = match assigned {
        Ok(()) => Contained { job: Some(job) },
        Err(err) => {
            tracing::warn!(
                pid, %err,
                "could not put the agent in a job object; it runs without one and will not die with the server"
            );
            Contained { job: None }
        }
    };
    if let Err(err) = resume_process(pid) {
        // Never leave a suspended process behind.
        contained.release();
        let _ = child.start_kill();
        return Err(std::io::Error::other(format!(
            "could not resume the agent's process: {err}"
        )));
    }
    Ok((child, contained))
}

/// What is at `pid`: nothing, or a process and the time it was created (the
/// 100 ns FILETIME). A process that has exited (but whose handle somebody still
/// holds) is gone.
pub fn inspect(pid: u32) -> Inspect {
    // SAFETY: opening by id; the handle is closed before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // SAFETY: reads the calling thread's last error.
        return match unsafe { GetLastError() } {
            ERROR_INVALID_PARAMETER => Inspect::Gone,
            _ => Inspect::Unreadable,
        };
    }
    let mut code = 0_u32;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: valid handle and valid out pointers for the calls.
    let (has_code, has_times) = unsafe {
        (
            GetExitCodeProcess(handle, &mut code) != 0,
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0,
        )
    };
    // SAFETY: closing the handle opened above.
    unsafe { CloseHandle(handle) };
    if has_code && code != STILL_ACTIVE {
        return Inspect::Gone;
    }
    if !has_times {
        return Inspect::Unreadable;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    match i64::try_from(ticks) {
        Ok(ticks) => Inspect::Alive(ticks),
        Err(_) => Inspect::Unreadable,
    }
}

/// Best effort: `taskkill /T /F` ends the process and the processes it
/// started. The fallback when there is no job object, and what start-up uses
/// on a verified leftover (a job cannot be recovered after a restart).
pub async fn kill_tree(pid: u32) {
    let mut taskkill = tokio::process::Command::new("taskkill");
    taskkill
        .args(["/T", "/F", "/PID"])
        .arg(pid.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match taskkill.status().await {
        Ok(_) => {}
        Err(err) => tracing::warn!(pid, %err, "could not run taskkill"),
    }
}
