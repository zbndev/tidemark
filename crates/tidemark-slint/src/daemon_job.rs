#![allow(unsafe_code)]
//! The daemon this client started dies with this client (Windows).
//!
//! On Linux the daemon is somebody else's process: systemd starts it, D-Bus activates it,
//! and `PartOf=graphical-session.target` ends it with the session. Quitting the window
//! there leaves a supervised service running, which is what a service is for, and
//! `systemctl --user stop` is how a user ends it.
//!
//! Windows has none of that. When nothing is serving the endpoint the client spawns
//! `tidemarkd.exe` itself (see `bus`), with `CREATE_NO_WINDOW` and no parent-child
//! relationship the system enforces — so quitting from the tray closed the window and left
//! a daemon behind: no icon, no window, still polling providers and still raising toasts,
//! and reachable only through Task Manager. Every launch after that spawned another one,
//! because the singleton mutex the first one holds is what makes the second exit rather
//! than anything the user can see.
//!
//! A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the platform's answer. The
//! kernel closes the handle when this process ends — however it ends, Quit or crash or
//! kill — and closing the last handle terminates everything in the job. Nothing has to run
//! at shutdown for it to work, which is the property a quit path cannot offer.
//!
//! # What is deliberately not in the job
//!
//! A daemon started by the login Scheduled Task ("Daemon only" in Preferences) is never
//! spawned here and so never joins the job: background monitoring without a window keeps
//! working, and quitting the client does not end it. The job holds exactly the daemons
//! this client started, which are exactly the ones nothing else will ever stop.
//!
//! # Why a copy of tidemarkd's job
//!
//! `tidemarkd::lifecycle::KillOnCloseJob` is the same object for the same reason, but the
//! client may not depend on the daemon crate — `scripts/check-layering.sh` is what keeps
//! the two processes separable. Forty lines of Win32 duplicated is the price of that, and
//! the cheaper of the two prices.
//!
//! This module joins `single_instance` as a locally-audited `unsafe` island: the generated
//! Win32 bindings are unsafe functions, its public surface is entirely safe, and raw
//! handles never escape it.

use std::process::Child;
use std::sync::OnceLock;

use std::os::windows::io::AsRawHandle as _;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// The one job every daemon this process spawns joins.
///
/// A `OnceLock` and never a `Drop`: the handle is meant to outlive every value in the
/// program and be closed by the kernel at exit, which is precisely when the daemon should
/// die. `None` records a job that could not be created — the failure is logged once, and a
/// client that cannot make one still runs, it just leaves the daemon behind as before.
static JOB: OnceLock<Option<Job>> = OnceLock::new();

/// Puts a freshly spawned daemon under this client's lifetime.
///
/// Called on the child of every successful spawn. Failure is never the caller's problem:
/// an unreaped daemon is worse than the old behaviour in no way at all, and the alternative
/// — refusing to spawn a daemon because it could not be adopted — would trade a leak for
/// a client that cannot talk to anything.
pub fn adopt(child: &Child) {
    let Some(job) = JOB.get_or_init(|| match Job::new() {
        Ok(job) => Some(job),
        Err(error) => {
            tracing::warn!(
                %error,
                "no job object; a daemon spawned by this client will outlive it"
            );
            None
        }
    }) else {
        return;
    };
    // A child that has already exited is refused by the kernel, and that is a race with no
    // consequence: it is gone, which is the state the job exists to produce.
    if let Err(error) = job.assign(child) {
        tracing::warn!(%error, "the spawned daemon did not join the job");
    }
}

/// A job object whose processes are terminated when its last handle closes.
struct Job {
    handle: HANDLE,
}

// SAFETY: a job handle is a kernel object with no thread affinity; the Win32 calls that
// take it are safe from any thread, and nothing here hands the raw handle out.
unsafe impl Send for Job {}
// SAFETY: as above — `assign` takes `&self` and the kernel serialises the call.
unsafe impl Sync for Job {}

impl Job {
    /// Creates an empty job whose processes die when the job handle closes.
    fn new() -> Result<Self, windows::core::Error> {
        // SAFETY: both parameters are None (an unnamed job with default security); the
        // returned handle is owned by us.
        let handle = unsafe { CreateJobObjectW(None, None) }?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `handle` is a valid job, and `limits` is a valid struct of exactly the
        // class named, borrowed for the duration of the call.
        let result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(error) = result {
            // SAFETY: the job handle is ours and nothing else uses it.
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }

    /// Puts a spawned child into the job.
    ///
    /// Assignment happens just after the spawn rather than atomically with it: a client
    /// killed in the microseconds between the two still orphans that one daemon. Closing
    /// that window means `CreateProcessW` with `CREATE_SUSPENDED` and a resumed thread
    /// handle `std::process::Child` does not expose — a great deal of unsafe for a race
    /// nothing has ever observed.
    fn assign(&self, child: &Child) -> Result<(), windows::core::Error> {
        // SAFETY: the job handle is ours and valid for the borrow, and the child's raw
        // handle is valid for as long as `child` is.
        unsafe { AssignProcessToJobObject(self.handle, HANDLE(child.as_raw_handle() as *mut _)) }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Job").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Waits for a child to be reaped, or gives up. Returns whether it died.
    fn died(child: &mut Child, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return false,
            }
        }
        false
    }

    fn long_lived() -> Child {
        std::process::Command::new("cmd")
            .args(["/c", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("a long-lived child spawns")
    }

    /// The whole bug in one assertion: a daemon this client spawned must not survive it.
    /// Dropping the job stands in for the process exiting, which is what closes the last
    /// handle in the real thing.
    #[test]
    fn closing_the_job_kills_the_daemon_it_holds() {
        let job = Job::new().expect("the job creates");
        let mut child = long_lived();

        job.assign(&child).expect("the child joins the job");
        // SAFETY: the handle is this job's, taken because the test needs the close to be
        // the observable event rather than a `Drop` this type deliberately does not have.
        let _ = unsafe { CloseHandle(job.handle) };

        assert!(
            died(&mut child, Duration::from_secs(5)),
            "closing the job must terminate the daemon assigned to it"
        );
    }

    /// The guard against fixing this by killing everything: a process that never joined
    /// the job — the login Scheduled Task's daemon — is untouched by the job closing.
    #[test]
    fn a_daemon_that_never_joined_the_job_survives_it() {
        let job = Job::new().expect("the job creates");
        let mut child = long_lived();

        // SAFETY: the handle is this job's; see above.
        let _ = unsafe { CloseHandle(job.handle) };

        assert!(
            !died(&mut child, Duration::from_millis(500)),
            "only the daemons this client spawned belong to the job"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
