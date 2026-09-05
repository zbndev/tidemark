#![allow(unsafe_code)]
//! Windows lifecycle primitives: single-instance mutex, the kill-on-close job
//! object, and the per-user startup registrations (Scheduled Task for the daemon,
//! HKCU Run for the UI).
//!
//! This module joins `tidemark-core`'s secrets backend as a locally-audited
//! `unsafe` island: the generated Win32 bindings are unsafe functions, its public
//! surface is entirely safe, and raw handles never escape it.
//!
//! Mechanism choices, per the port plan's daemon-lifecycle verdict:
//! - The daemon's login autostart is a **per-user Scheduled Task** registered
//!   through the Task Scheduler 2.0 COM interface (`ITaskService`, the same
//!   backend `Register-ScheduledTask` drives). The first cut drove
//!   `schtasks.exe /SC ONLOGON`, but on this machine that command is denied to
//!   an unelevated token even for the calling user's own task folder — while
//!   COM accepts it with no elevation, no machine-level access and no helper
//!   process. Enable/disable both map to `RegisterTaskDefinition` with
//!   `TASK_CREATE_OR_UPDATE`: an overwrite, so re-registering is idempotent.
//! - The singleton is a **session-local named mutex** (`Local\` prefix). Sessions on
//!   Windows are per-user logons, so the `Local\` namespace is already per-user and
//!   the machine-wide `Global\` namespace is deliberately avoided; the user SID in
//!   the name would be redundant and is left out. It is acquired before the history
//!   database is opened, so a second instance can never touch SQLite; it exits 0
//!   quietly because "already running" is success, not failure.
//! - The UI's login autostart is an **HKCU Run** value, the Windows equivalent of the
//!   XDG autostart override the Linux arm writes for the desktop entry.

#[cfg(test)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, ITaskService, TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, Interface as _};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
};
#[cfg(test)]
use windows::Win32::System::Registry::{RegDeleteTreeW, RegQueryValueExW};
use windows::Win32::System::Threading::CreateMutexW;

use windows::core::HSTRING;

/// The Scheduled Task that starts the daemon at logon. A fixed, short name: it is
/// referenced by `schtasks`, the uninstaller, and this module, and it lives in the
/// calling user's task folder, so no per-user disambiguation is needed.
pub const DAEMON_TASK_NAME: &str = "TidemarkDaemon";

/// The HKCU Run value that starts the UI at logon.
pub const UI_RUN_VALUE_NAME: &str = "Tidemark";

/// The registry path of the per-user Run key.
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The named mutex that makes this user's daemon single-instance.
const SINGLETON_MUTEX: &str = r"Local\io.github.zbndev.Tidemark.Daemon";

/// Holds the daemon's single-instance mutex for as long as the daemon runs.
///
/// Acquire it before any shared state is opened: the mutex is what makes "open the
/// history database" safe to attempt at all. Dropping the guard releases (and closes)
/// the handle; if the process dies without dropping it, the kernel closes the handle
/// anyway, so a crashed daemon never locks the next one out.
pub struct Singleton {
    handle: HANDLE,
}

impl Singleton {
    /// Takes the per-user singleton mutex. `Ok(None)` means another daemon of this
    /// user already holds it: the caller should exit 0 quietly.
    pub fn acquire() -> Result<Option<Self>, windows::core::Error> {
        let name = HSTRING::from(SINGLETON_MUTEX);
        // SAFETY: `name` is a valid nul-terminated HSTRING borrowed for the call; the
        // returned handle is owned by us and closed in `Drop`.
        let handle = unsafe { CreateMutexW(None, false, &name) }?;
        // `CreateMutexW` succeeds even when the mutex already exists; the reason it
        // succeeded is only visible through the last error.
        // SAFETY: no parameters; reading the thread's last error.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: the duplicate handle is ours and nothing waits on it.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }
}

impl Drop for Singleton {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is the mutex this guard owns.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

impl std::fmt::Debug for Singleton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Singleton").finish_non_exhaustive()
    }
}

/// A kill-on-close job object: every process assigned to it dies when the job is
/// dropped or its owner process exits, so a daemon death reaps the vendor helpers
/// it spawned instead of orphaning them. Consumed by the agy supervisor, which
/// puts every ConPTY spawn into one of these at birth.
pub struct KillOnCloseJob {
    handle: HANDLE,
}

impl KillOnCloseJob {
    /// Creates an empty job whose processes are terminated when the job handle closes.
    pub fn new() -> Result<Self, windows::core::Error> {
        // SAFETY: both parameters are None; the returned handle is owned by us.
        let handle = unsafe { CreateJobObjectW(None, None) }?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `handle` is a valid job, and `limits` is a valid struct of exactly
        // the class named, borrowed for the duration of the call.
        let result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(error) = result {
            // SAFETY: the job handle is ours and nothing else uses it.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }

    /// Puts a spawned child process into the job. An already-terminating process is
    /// accepted by the kernel and dies on its own, so assignment never needs to fail
    /// the caller for a race it cannot observe.
    ///
    /// The supervisor spawns through `CreateProcessW` and uses [`Self::assign_handle`];
    /// this `std::process::Child` arm is exercised by this module's own tests.
    #[cfg(test)]
    pub fn assign(&self, child: &std::process::Child) -> Result<(), windows::core::Error> {
        // SAFETY: the child's raw handle is valid for as long as `child` is.
        self.assign_handle(HANDLE(child.as_raw_handle() as *mut _))
    }

    /// Puts an already-open process handle into the job. The raw-handle arm the
    /// supervisor needs: a ConPTY child is spawned directly through
    /// `CreateProcessW`, so there is no `std::process::Child` to borrow.
    pub fn assign_handle(&self, process: HANDLE) -> Result<(), windows::core::Error> {
        // SAFETY: the job handle is valid for as long as `&self` is borrowed,
        // and `process` is a live handle the caller owns.
        unsafe { AssignProcessToJobObject(self.handle, process) }
    }

    /// Terminates every process in the job now. The Unix supervisor's graceful
    /// arm has no Windows analogue — there is no signal to send — so this
    /// immediate termination *is* the teardown semantics.
    pub fn terminate(&self) -> Result<(), windows::core::Error> {
        // SAFETY: the job handle is ours and valid for as long as `&self` is
        // borrowed.
        unsafe { TerminateJobObject(self.handle, 0) }
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // Closing the last job handle is what kills the assigned processes; no
        // explicit TerminateJobObject here.
        // SAFETY: the job handle is ours.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

impl std::fmt::Debug for KillOnCloseJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KillOnCloseJob").finish_non_exhaustive()
    }
}

/// Sets the daemon's login autostart: register the Scheduled Task when enabled,
/// delete it when not. Both directions are idempotent: registration uses
/// `TASK_CREATE_OR_UPDATE` (an overwrite, not an error) and deleting an absent
/// task reports success.
///
/// Mechanism choice, updated per the plan's failure branch: the first cut drove
/// `schtasks.exe /SC ONLOGON`, but on this machine that command is denied to an
/// unelevated token even for the per-user task folder — while the Task Scheduler
/// 2.0 COM interface (the same `Register-ScheduledTask` backend) accepts it. COM
/// it is: no machine-level access, no elevation, no helper process.
pub fn set_daemon_task(enabled: bool) -> Result<(), String> {
    let exe = daemon_exe()?;
    // SAFETY: `CoInitializeEx` starts a COM apartment for this thread; every object
    // created in `daemon_task_com` is released before `CoUninitialize` runs.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|error| format!("could not initialize COM: {error}"))?;
        let result = daemon_task_com(enabled, &exe);
        CoUninitialize();
        result
    }
}

/// The whole Scheduled Task round-trip. The caller owns an initialized COM
/// apartment for the duration.
///
/// SAFETY: all interfaces are valid objects obtained from the Task Scheduler
/// service; every `unsafe` call follows its interface's contract and outlives no
/// borrowed data.
fn daemon_task_com(enabled: bool, exe: &Path) -> Result<(), String> {
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("could not start the Task Scheduler service: {error}"))?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .map_err(|error| format!("could not connect to the Task Scheduler service: {error}"))?;
        let root = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|error| format!("could not open the root task folder: {error}"))?;
        let name = BSTR::from(DAEMON_TASK_NAME);

        if !enabled {
            return match root.DeleteTask(&name, 0) {
                Ok(()) => Ok(()),
                // A missing task is already what "off" means, so removing it again is a
                // success, not an error.
                Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_FILE_NOT_FOUND) => {
                    Ok(())
                }
                Err(error) => Err(format!(
                    "could not unregister task {DAEMON_TASK_NAME}: {error}"
                )),
            };
        }

        let definition = service
            .NewTask(0)
            .map_err(|error| format!("could not create the task definition: {error}"))?;
        let registration = definition
            .RegistrationInfo()
            .map_err(|error| format!("could not open the registration info: {error}"))?;
        registration
            .SetDescription(&BSTR::from("Starts the Tidemark polling daemon at logon."))
            .map_err(|error| format!("could not describe the task: {error}"))?;
        // Least privilege and the logged-on session only: the equivalent of the Linux
        // arm's `WantedBy=graphical-session.target` user unit.
        let principal = definition
            .Principal()
            .map_err(|error| format!("could not open the task principal: {error}"))?;
        principal
            .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
            .map_err(|error| format!("could not set the logon type: {error}"))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_LUA)
            .map_err(|error| format!("could not set the run level: {error}"))?;
        // Registering without an explicit principal user is denied; naming this user
        // scopes the task to their account, which is what per-user means anyway.
        let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(domain), Ok(name)) => format!(r"{domain}\{name}"),
            (_, Ok(name)) => name,
            _ => String::new(),
        };
        principal
            .SetUserId(&BSTR::from(user.as_str()))
            .map_err(|error| format!("could not set the task's user: {error}"))?;
        let triggers = definition
            .Triggers()
            .map_err(|error| format!("could not open the triggers: {error}"))?;
        let logon: ILogonTrigger = triggers
            .Create(TASK_TRIGGER_LOGON)
            .and_then(|trigger| trigger.cast::<ILogonTrigger>())
            .map_err(|error| format!("could not create the logon trigger: {error}"))?;
        // A logon trigger with no user is a machine-wide one and needs elevation;
        // naming this user keeps the task per-user, which is the point.
        logon
            .SetUserId(&BSTR::from(user.as_str()))
            .map_err(|error| format!("could not scope the logon trigger: {error}"))?;
        drop(logon);
        let actions = definition
            .Actions()
            .map_err(|error| format!("could not open the actions: {error}"))?;
        let exec: IExecAction = actions
            .Create(TASK_ACTION_EXEC)
            .and_then(|action| action.cast::<IExecAction>())
            .map_err(|error| format!("could not create the exec action: {error}"))?;
        exec.SetPath(&BSTR::from(exe.to_string_lossy().as_ref()))
            .map_err(|error| format!("could not point the task at {}: {error}", exe.display()))?;

        root.RegisterTaskDefinition(
            &name,
            &definition,
            TASK_CREATE_OR_UPDATE.0,
            &empty,
            &empty,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &empty,
        )
        .map(|_registered| ())
        .map_err(|error| format!("could not register task {DAEMON_TASK_NAME}: {error}"))
    }
}

/// The UI's login autostart: an HKCU Run value pointing at the sibling `tidemark.exe`,
/// written or removed per `enabled`. A missing value deletes to success.
pub fn set_ui_run(enabled: bool) -> Result<(), String> {
    let exe = ui_exe()?;
    set_run_value_with(HKEY_CURRENT_USER, enabled, &exe)
}

/// Where the daemon executable lives, for the task's command line.
fn daemon_exe() -> Result<PathBuf, String> {
    current_exe_sibling("tidemarkd.exe")
}

/// Where the UI executable lives, for the Run value.
fn ui_exe() -> Result<PathBuf, String> {
    current_exe_sibling("tidemark.exe")
}

fn current_exe_sibling(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate this process: {error}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent", exe.display()))?;
    Ok(dir.join(name))
}

/// The REG_SZ payload of a Run value: the quoted command line Windows executes.
fn run_value(exe: &Path) -> Vec<u16> {
    format!("\"{}\"", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// Maps a Win32 error-code return to the error type the rest of the module uses.
fn checked(result: windows::Win32::Foundation::WIN32_ERROR) -> Result<(), windows::core::Error> {
    result.ok()
}

fn set_run_value_with(root: HKEY, enabled: bool, exe: &Path) -> Result<(), String> {
    let key = open_run_key(root)?;
    let result = if enabled {
        write_run_value(key, &run_value(exe))
    } else {
        delete_run_value(key)
    };
    // SAFETY: `key` is the handle opened above.
    let _ = unsafe { RegCloseKey(key) };
    result
}

/// Opens (creating if absent) the per-user Run key under `root`. Predefined roots
/// address absolute paths; a test can pass an open scratch key instead, which then
/// holds the same `Run` path relative to itself.
fn open_run_key(root: HKEY) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    // SAFETY: the path is a constant nul-terminated HSTRING borrowed for the call;
    // `key` receives an open handle the caller closes.
    unsafe {
        RegCreateKeyExW(
            root,
            &HSTRING::from(RUN_KEY_PATH),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .map_err(|error| format!("could not open {RUN_KEY_PATH}: {error}"))?;
    Ok(key)
}

fn write_run_value(key: HKEY, payload: &[u16]) -> Result<(), String> {
    // SAFETY: `key` is a valid open key with KEY_SET_VALUE, the name is a
    // nul-terminated HSTRING, and `payload` is a valid UTF-16 buffer including its
    // terminator, passed as its little-endian bytes.
    let mut bytes = Vec::with_capacity(payload.len() * 2);
    for unit in payload {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    checked(unsafe {
        RegSetValueExW(
            key,
            &HSTRING::from(UI_RUN_VALUE_NAME),
            Some(0),
            REG_SZ,
            Some(&bytes),
        )
    })
    .map_err(|error| format!("could not write the {UI_RUN_VALUE_NAME} Run value: {error}"))
}

fn delete_run_value(key: HKEY) -> Result<(), String> {
    // SAFETY: `key` is a valid open key with KEY_SET_VALUE and the name is a
    // nul-terminated HSTRING.
    match unsafe { RegDeleteValueW(key, &HSTRING::from(UI_RUN_VALUE_NAME)) } {
        result if result.is_ok() => Ok(()),
        // A missing value is already what "off" means, so removing it again is a
        // success, not an error.
        result if result == ERROR_FILE_NOT_FOUND => Ok(()),
        result => Err(format!(
            "could not remove the {UI_RUN_VALUE_NAME} Run value: {}",
            windows::core::Error::from(result)
        )),
    }
}

/// Reads back the Run value as a lossy string, for tests and diagnostics.
#[cfg(test)]
fn read_run_value(key: HKEY) -> Option<String> {
    let mut kind = REG_SZ;
    let mut bytes = [0u8; 2048];
    let mut length = bytes.len() as u32;
    // SAFETY: `key` is valid, and the buffer and its length describe `bytes` exactly.
    unsafe {
        RegQueryValueExW(
            key,
            &HSTRING::from(UI_RUN_VALUE_NAME),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut length),
        )
    }
    .ok()
    .ok()?;
    let end = length as usize;
    let units: Vec<u16> = bytes[..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_registration_is_an_overwrite_not_a_conflict() {
        // The idempotency guarantee: re-registering over an existing task must be a
        // plain update, which is what the Task Scheduler's create-or-update flag is.
        assert_eq!(TASK_CREATE_OR_UPDATE.0, 6);
    }

    /// Manual QA (run with `-- --ignored`): drives the real registration path on
    /// the real machine, twice, to show re-registering is an overwrite; the caller
    /// unregisters afterwards. Ignored by default because it mutates the user's
    /// task folder.
    #[test]
    #[ignore = "manual QA: registers and unregisters a real per-user Scheduled Task"]
    fn the_daemon_task_registers_twice_and_unregisters() {
        set_daemon_task(true).expect("the task registers");
        set_daemon_task(true).expect("re-registering over it is an overwrite");
        set_daemon_task(false).expect("the task unregisters");
        set_daemon_task(false).expect("unregistering an absent task is a success");
    }

    #[test]
    fn a_run_value_is_a_quoted_nul_terminated_reg_sz() {
        let payload = run_value(Path::new(r"C:\Apps\Tidemark\tidemark.exe"));

        assert_eq!(payload.last().copied(), Some(0));
        let text = String::from_utf16(&payload[..payload.len() - 1]).expect("utf-16");
        assert_eq!(text, r#""C:\Apps\Tidemark\tidemark.exe""#);
    }

    /// A real registry round-trip against a scratch key under HKCU: the same call
    /// path production takes, cleaned up unconditionally. A test hive via
    /// `RegLoadAppKey` would need a seeded hive file; HKCU-scratch exercises the
    /// identical API sequence.
    #[test]
    fn the_run_value_round_trips_against_the_real_registry() {
        const SCRATCH: &str = r"Software\Tidemark\lifecycle-test";
        let mut key = HKEY::default();
        // SAFETY: scratch key path is a constant; the handle is closed on every path.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(SCRATCH),
                Some(0),
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .expect("the scratch key opens");

        set_run_value_with(key, true, Path::new(r"C:\Apps\tidemark.exe")).expect("written");
        let run = open_run_key(key).expect("the Run key opens");
        assert_eq!(
            read_run_value(run).as_deref(),
            Some(r#""C:\Apps\tidemark.exe""#)
        );
        // SAFETY: the Run key's handle, opened above.
        let _ = unsafe { RegCloseKey(run) };

        set_run_value_with(key, false, Path::new(r"C:\Apps\tidemark.exe")).expect("removed");
        let run = open_run_key(key).expect("the Run key reopens");
        assert_eq!(
            read_run_value(run),
            None,
            "a second removal is also a success"
        );
        // SAFETY: the Run key's handle, opened above.
        let _ = unsafe { RegCloseKey(run) };

        // SAFETY: the scratch key's handle, opened above.
        let _ = unsafe { RegCloseKey(key) };
        // SAFETY: the scratch subtree is ours from the first line of this test.
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(SCRATCH)) };
    }

    #[test]
    fn the_singleton_is_exclusive_within_this_session() {
        let _first = Singleton::acquire()
            .expect("the first acquire works")
            .expect("unheld");

        assert!(
            Singleton::acquire()
                .expect("the second acquire works")
                .is_none(),
            "a second acquire while the first guard lives must find the mutex taken"
        );
    }

    /// The whole point of the job: dropping it kills the children it holds, and it
    /// does so promptly. A real `ping` child stands in for a vendor helper.
    #[tokio::test]
    async fn dropping_the_job_kills_the_assigned_child() {
        let job = KillOnCloseJob::new().expect("the job creates");
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("a long-lived child spawns");
        job.assign(&child).expect("the child joins the job");
        drop(job);

        let (mut child, done) = tokio::task::spawn_blocking(move || {
            let status = child.wait().expect("the child is waitable");
            (child, status)
        })
        .await
        .expect("the waiter does not panic");
        let _ = &mut child;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while done_success(&done) && !exited(&mut child) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok(),
            "the child is reaped"
        );
    }

    fn done_success(status: &std::process::ExitStatus) -> bool {
        !status.success()
    }

    fn exited(child: &mut std::process::Child) -> bool {
        child.try_wait().expect("waitable").is_some()
    }
}
