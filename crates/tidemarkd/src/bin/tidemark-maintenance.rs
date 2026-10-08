//! Native helper run from Setup's temporary directory, never from a locked program file.
#[cfg(windows)]
mod native {
    use serde::{Deserialize, Serialize};
    use std::os::windows::process::CommandExt;
    use std::{
        fs, io,
        path::{Path, PathBuf},
        process::{Child, Command},
        time::{Duration, Instant},
    };
    use tidemark_types::ids;
    use tidemarkd::{installation, installer_integration, installer_process, lifecycle};

    const STATE: &str = "process-state.json";
    const STATE_PENDING: &str = ".process-state.pending";
    const NO_WINDOW: u32 = 0x0800_0000;

    #[derive(Debug, thiserror::Error)]
    pub enum Error {
        #[error("{0}")]
        Io(#[from] io::Error),
        #[error("{0}")]
        Json(#[from] serde_json::Error),
        #[error("{0}")]
        Files(#[from] installation::Error),
        #[error("{0}")]
        Task(String),
        #[error("{0}")]
        State(&'static str),
        #[error("{failure}; restoring prior state also failed: {recovery}")]
        Unwind {
            failure: Box<Error>,
            recovery: Box<Error>,
        },
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct State {
        install: PathBuf,
        ui: bool,
        visible: bool,
        independent_daemon: bool,
        task: Option<bool>,
        run: Option<Vec<u16>>,
        legacy: bool,
        #[serde(default)]
        mode: Mode,
        #[serde(default)]
        remove_files: Vec<String>,
        #[serde(default)]
        finalizing: bool,
        #[serde(default)]
        shortcut: Option<PathBuf>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    enum Mode {
        #[default]
        Setup,
        Remove,
        LegacyDirectory,
    }

    fn prepare_saved(
        install: &Path,
        state: &Path,
        files: Option<&[String]>,
        saved: &State,
        pause: impl FnOnce() -> Result<(), Error>,
        restore: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let saved_path = installation::state_file(install, state, STATE)?;
        if saved_path.exists() || installation::state_file(install, state, STATE_PENDING)?.exists()
        {
            return Err(Error::State(
                "unfinished installation state exists; recover it before retrying",
            ));
        }
        if saved.mode == Mode::LegacyDirectory {
            installation::validate_incoming(files.ok_or(Error::State(
                "directory replacement requires an incoming manifest",
            ))?)?;
            installation::prepare_directory(install, state)?;
        } else if let Some(files) = files {
            installation::backup(install, state, files)?;
        } else {
            for name in [
                "journal.json",
                "files",
                "directory-state.json",
                "previous-install",
                "failed-install",
            ] {
                if installation::state_file(install, state, name)?.exists() {
                    return Err(Error::State(
                        "unfinished file backup exists; recover it before uninstalling",
                    ));
                }
            }
            fs::create_dir_all(state)?;
        }
        let mut created = false;
        let mut paused = false;
        let result = (|| {
            publish_state(install, state, saved)?;
            created = true;
            paused = true;
            pause()
        })();
        if let Err(failure) = result {
            let unwind = (|| {
                if paused {
                    restore()?;
                }
                if saved.mode == Mode::LegacyDirectory {
                    installation::finalize_directory(state)?;
                } else if files.is_some() {
                    installation::finalize(state)?;
                }
                if created {
                    fs::remove_file(installation::state_file(install, state, STATE)?)?;
                }
                Ok(())
            })();
            return match unwind {
                Ok(()) => Err(failure),
                Err(recovery) => Err(Error::Unwind {
                    failure: Box::new(failure),
                    recovery: Box::new(recovery),
                }),
            };
        }
        Ok(())
    }

    fn recover_files(install: &Path, state: &Path, saved: &State) -> Result<(), Error> {
        match saved.mode {
            Mode::Setup => installation::rollback(install, state)?,
            Mode::LegacyDirectory => installation::rollback_directory(install, state)?,
            Mode::Remove => (),
        }
        Ok(())
    }

    fn probe_ready(endpoint: &Path, timeout: Duration, pid: u32) -> Result<(), Error> {
        let stream = uds_windows::UnixStream::connect(endpoint)?;
        if installer_process::socket_peer_pid(&stream)? != pid {
            return Err(Error::State("daemon endpoint belongs to another process"));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                let connection = zbus::connection::Builder::async_io_unix_stream(stream)
                    .p2p()
                    .build()
                    .await?;
                let proxy = zbus::Proxy::new(
                    &connection,
                    ids::DAEMON_BUS_NAME,
                    ids::OBJECT_PATH,
                    ids::DAEMON_INTERFACE,
                )
                .await?;
                let _: String = proxy.get_property("Version").await?;
                Ok::<_, zbus::Error>(())
            })
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon did not answer Version before the readiness timeout",
                )
            })?
            .map_err(io::Error::other)
        })?;
        Ok(())
    }

    fn incoming(manifest: &Path) -> Result<Vec<String>, Error> {
        let mut files: Vec<String> = serde_json::from_slice(&fs::read(manifest)?)?;
        // Inno's stable uninstall files also need to survive a failed upgrade.
        for file in ["unins000.exe", "unins000.dat"] {
            if !files.iter().any(|path| path.eq_ignore_ascii_case(file)) {
                files.push(file.into());
            }
        }
        Ok(files)
    }

    fn load(install: &Path, state: &Path) -> Result<State, Error> {
        let saved: State =
            serde_json::from_slice(&fs::read(installation::state_file(install, state, STATE)?)?)?;
        if saved.install != installation::install_root(install)? {
            return Err(Error::State(
                "process state belongs to another installation",
            ));
        }
        if let Some(shortcut) = &saved.shortcut {
            validate_shortcut(shortcut)?;
        }
        Ok(saved)
    }

    fn validate_shortcut(shortcut: &Path) -> Result<(), Error> {
        if !shortcut.is_absolute()
            || shortcut
                .file_name()
                .is_none_or(|name| name != "Tidemark.lnk")
        {
            return Err(Error::State("invalid Start-menu shortcut path"));
        }
        // Apply the same symlink/reparse ancestor checks as the program root,
        // allowing the shortcut itself to be absent after partial uninstall.
        let _ = installation::install_root(shortcut)?;
        Ok(())
    }

    pub fn run(args: &[std::ffi::OsString]) -> Result<(), Error> {
        if args.len() < 6 {
            return Err(Error::State(
                "usage: tidemark-maintenance COMMAND INSTALL STATE MANIFEST ERRORFILE [EXTRA]",
            ));
        }
        let operation = args[1].to_str().ok_or(Error::State("invalid command"))?;
        let install = Path::new(&args[2]);
        let state = Path::new(&args[3]);
        let manifest = Path::new(&args[4]);
        match operation {
            "prepare" | "prepare-remove" => {
                if installation::state_file(install, state, STATE)?.exists() {
                    // A removal whose owned files are all gone stranded only its
                    // bookkeeping (an interrupted uninstall tail); finishing it
                    // here keeps the promised "rerun the uninstaller" recovery.
                    if !removal_already_complete(install, state)? {
                        return Err(Error::State(
                            "unfinished installation state exists; recover it before retrying",
                        ));
                    }
                }
                let processes = installer_process::running(install)?;
                let ui = processes.iter().find(|process| process.ui);
                let task = lifecycle::installed_task_enabled(install).map_err(Error::Task)?;
                let run = lifecycle::ui_run_snapshot().map_err(Error::Task)?;
                let legacy =
                    operation == "prepare" && args.iter().any(|arg| arg == "--legacy-directory");
                let independent_daemon = processes
                    .iter()
                    .any(|process| !process.ui && ui.is_none_or(|ui| process.parent_pid != ui.pid));
                let remove = operation == "prepare-remove";
                let files = if remove {
                    None
                } else {
                    Some(incoming(manifest)?)
                };
                let shortcut = if remove {
                    let path = PathBuf::from(
                        args.get(6)
                            .ok_or(Error::State("prepare-remove requires a shortcut path"))?,
                    );
                    validate_shortcut(&path)?;
                    Some(path)
                } else {
                    None
                };
                let saved = State {
                    install: installation::install_root(install)?,
                    ui: ui.is_some(),
                    visible: ui.is_some_and(|ui| ui.visible),
                    independent_daemon,
                    task,
                    run,
                    legacy,
                    mode: if remove {
                        Mode::Remove
                    } else if legacy {
                        Mode::LegacyDirectory
                    } else {
                        Mode::Setup
                    },
                    remove_files: if remove {
                        installation::owned(install)?
                    } else {
                        Vec::new()
                    },
                    finalizing: false,
                    shortcut,
                };
                prepare_saved(
                    install,
                    state,
                    files.as_deref(),
                    &saved,
                    || {
                        if task == Some(true) {
                            lifecycle::enable_installed_task(install, false).map_err(Error::Task)
                        } else {
                            Ok(())
                        }
                    },
                    || restore_task(install, state),
                )?;
            }
            "stop" => {
                let saved = load(install, state)?;
                stop(install, state, &saved)?;
            }
            "archive" => {
                let saved = load(install, state)?;
                if saved.mode != Mode::LegacyDirectory || saved.finalizing {
                    return Err(Error::State(
                        "archive requires a prepared directory replacement",
                    ));
                }
                if !installer_process::running(install)?.is_empty() {
                    return Err(Error::State(
                        "stop all matched program processes before archiving",
                    ));
                }
                installation::archive_directory(install, state)?;
            }
            "snapshot-integration" => {
                let _ = load(install, state)?;
                let shortcut = args.get(6).ok_or(Error::State(
                    "snapshot-integration requires a shortcut path",
                ))?;
                installer_integration::backup(state, Path::new(shortcut))?;
            }
            "commit" => {
                let saved = load(install, state)?;
                match saved.mode {
                    Mode::Setup => installation::commit(install, state, &incoming(manifest)?)?,
                    Mode::LegacyDirectory => {
                        installation::commit_directory(install, state, &incoming(manifest)?)?
                    }
                    Mode::Remove => return Err(Error::State("commit requires setup preparation")),
                }
                restore_task(install, state)?;
            }
            "rollback" => {
                let saved = load(install, state)?;
                recover_files(install, state, &saved)?;
                installer_integration::rollback(state)?;
                restore_startup(install, state)?;
            }
            "cancel" => {
                let saved = load(install, state)?;
                if saved.mode == Mode::LegacyDirectory {
                    installation::rollback_directory(install, state)?;
                }
                installer_integration::rollback(state)?;
                restore_startup(install, state)?;
            }
            "resume" => {
                resume(install, &load(install, state)?)?;
            }
            "recover" => {
                if !installation::state_file(install, state, STATE)?.exists() {
                    let _gate = installer_process::MaintenanceGuard::acquire()?;
                    return recover_unpublished(install, state);
                }
                let saved = load(install, state)?;
                if saved.finalizing {
                    let _gate = installer_process::MaintenanceGuard::acquire()?;
                    return finalize(install, state, &saved);
                }
                if saved.mode == Mode::Remove {
                    return Err(Error::State(
                        "partial uninstall has no program-file backup; use finish-remove with this state",
                    ));
                }
                let gate = installer_process::MaintenanceGuard::acquire()?;
                stop(install, state, &saved)?;
                recover_files(install, state, &saved)?;
                installer_integration::rollback(state)?;
                restore_startup(install, state)?;
                drop(gate);
                resume(install, &saved)?;
                finalize(install, state, &saved)?;
            }
            "remove-startup" => {
                let saved = load(install, state)?;
                if saved.mode != Mode::Remove {
                    return Err(Error::State(
                        "remove-startup requires uninstall preparation",
                    ));
                }
                // Run is removed first: a failure here leaves the task untouched. Task
                // removal is last and cannot be followed by another registry mutation.
                let result = (|| {
                    // Verify task ownership even though its executable may now be absent.
                    let task = lifecycle::installed_task_enabled(install).map_err(Error::Task)?;
                    lifecycle::set_ui_run(false).map_err(Error::Task)?;
                    if task.is_some() {
                        lifecycle::set_daemon_task(false).map_err(Error::Task)?;
                    }
                    Ok(())
                })();
                if let Err(failure) = result {
                    return match restore_startup(install, state) {
                        Ok(()) => Err(failure),
                        Err(recovery) => Err(Error::Unwind {
                            failure: Box::new(failure),
                            recovery: Box::new(recovery),
                        }),
                    };
                }
            }
            "verify-remove" => {
                let saved = load(install, state)?;
                if saved.mode != Mode::Remove {
                    return Err(Error::State("verify-remove requires uninstall preparation"));
                }
                let files: Vec<_> = saved
                    .remove_files
                    .iter()
                    .filter(|p| {
                        !p.eq_ignore_ascii_case("unins000.exe")
                            && !p.eq_ignore_ascii_case("unins000.dat")
                    })
                    .cloned()
                    .collect();
                if !installation::remaining(install, &files)?.is_empty() {
                    return Err(Error::State(
                        "uninstall left owned program files behind; recovery state has been preserved",
                    ));
                }
            }
            "finish-remove" => {
                let saved = load(install, state)?;
                if saved.mode != Mode::Remove {
                    return Err(Error::State(
                        "finish-remove requires interrupted uninstall state",
                    ));
                }
                let _gate = installer_process::MaintenanceGuard::acquire()?;
                stop(install, state, &saved)?;
                let mut files = saved.remove_files.clone();
                for file in ["unins000.exe", "unins000.dat"] {
                    if !files.iter().any(|p| p.eq_ignore_ascii_case(file)) {
                        files.push(file.into());
                    }
                }
                installation::remove_files(install, &files)?;
                // File removal has started, so completing startup removal is appropriate.
                let task = lifecycle::installed_task_enabled(install).map_err(Error::Task)?;
                lifecycle::set_ui_run(false).map_err(Error::Task)?;
                if task.is_some() {
                    lifecycle::set_daemon_task(false).map_err(Error::Task)?;
                }
                if !installation::remaining(install, &files)?.is_empty() {
                    return Err(Error::State(
                        "owned files remain; uninstall recovery state has been preserved",
                    ));
                }
                installer_integration::remove(
                    saved
                        .shortcut
                        .as_deref()
                        .ok_or(Error::State("uninstall state has no shortcut path"))?,
                )?;
                finalize(install, state, &saved)?;
            }
            "finalize" | "finalize-remove" => {
                let saved = load(install, state)?;
                if (operation == "finalize-remove") != (saved.mode == Mode::Remove) {
                    return Err(Error::State(
                        "finalization does not match the prepared operation",
                    ));
                }
                finalize(install, state, &saved)?;
            }
            _ => return Err(Error::State("unknown maintenance command")),
        }
        Ok(())
    }

    fn restore_task(install: &Path, state: &Path) -> Result<(), Error> {
        if let Some(enabled) = load(install, state)?.task {
            lifecycle::enable_installed_task(install, enabled).map_err(Error::Task)?;
        }
        Ok(())
    }

    fn recover_unpublished(install: &Path, state: &Path) -> Result<(), Error> {
        installation::validate_preparation(install, state)?;
        let pending = installation::state_file(install, state, STATE_PENDING)?;
        if pending.exists() {
            fs::remove_file(pending)?;
        }
        installation::finalize(state)?;
        installation::finalize_directory(state)?;
        match fs::remove_dir(state) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Finish a stranded removal whose owned files are already gone, so the
    /// promised "rerun the uninstaller" recovery needs no manual helper run.
    fn removal_already_complete(install: &Path, state: &Path) -> Result<bool, Error> {
        let saved = load(install, state)?;
        if saved.mode != Mode::Remove || saved.finalizing {
            return Ok(false);
        }
        let files: Vec<_> = saved
            .remove_files
            .iter()
            .filter(|path| {
                !path.eq_ignore_ascii_case("unins000.exe")
                    && !path.eq_ignore_ascii_case("unins000.dat")
            })
            .cloned()
            .collect();
        if !installation::remaining(install, &files)?.is_empty() {
            return Ok(false);
        }
        installer_integration::remove(
            saved
                .shortcut
                .as_deref()
                .ok_or(Error::State("uninstall state has no shortcut path"))?,
        )?;
        finalize(install, state, &saved)?;
        Ok(true)
    }

    fn restore_startup(install: &Path, state: &Path) -> Result<(), Error> {
        lifecycle::restore_ui_run(load(install, state)?.run.as_deref()).map_err(Error::Task)?;
        restore_task(install, state)
    }

    fn stop(install: &Path, state: &Path, saved: &State) -> Result<(), Error> {
        let legacy = saved.mode == Mode::LegacyDirectory
            && !installation::directory_archived(install, state)?;
        let order = if legacy {
            [
                (true, ids::CLIENT_STOP_EVENT),
                (false, ids::DAEMON_STOP_EVENT),
            ]
        } else {
            [
                (false, ids::DAEMON_STOP_EVENT),
                (true, ids::CLIENT_STOP_EVENT),
            ]
        };
        // Modern daemons flush first. A legacy GUI cannot obey the maintenance
        // gate and may respawn its daemon, so close it first and rescan afterwards.
        for (ui, event) in order {
            let processes = installer_process::running(install)?;
            // Each signalled event stays alive until the role's waits finish, so a
            // process that is still creating its listener inherits the request.
            let mut pending = Vec::new();
            for process in processes.iter().filter(|p| p.ui == ui) {
                if let Some(signal) = process.stop_signal(event)? {
                    pending.push((signal, process));
                }
            }
            for (signal, process) in &pending {
                let _signal = signal;
                if !process.wait(Duration::from_secs(if legacy { 2 } else { 30 }))? {
                    if legacy {
                        process.force_stop()?;
                    } else {
                        return Err(Error::State(
                            "Tidemark did not stop cleanly; the installed files have not been changed",
                        ));
                    }
                }
            }
        }
        if !installer_process::running(install)?.is_empty() {
            return Err(Error::State(
                "a process restarted during maintenance; installation was stopped",
            ));
        }
        Ok(())
    }

    fn wait_daemon(
        install: &Path,
        pid: u32,
        child: &mut Option<Child>,
        legacy: bool,
    ) -> Result<(), Error> {
        let ready = installer_process::StopEvent::new(&installer_process::stop_event_name(
            ids::DAEMON_READY_EVENT,
            pid,
        ))?;
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if !daemon_alive(install, pid, child)? {
                return Err(Error::State("restored daemon exited before becoming ready"));
            }
            let mut announced = ready.wait(0)?;
            if !announced && legacy {
                let endpoint = tidemark_core::paths::data_dir()
                    .map_err(io::Error::other)?
                    .join("run/d.sock");
                if probe_ready(&endpoint, Duration::from_millis(300), pid).is_ok() {
                    announced = true;
                }
            }
            if announced {
                std::thread::sleep(Duration::from_millis(150));
                if !daemon_alive(install, pid, child)? {
                    return Err(Error::State("restored daemon exited during startup"));
                }
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(Error::State(
                    "restored daemon did not become ready; recovery state has been preserved",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn daemon_alive(install: &Path, pid: u32, child: &mut Option<Child>) -> Result<bool, Error> {
        if let Some(child) = child {
            return Ok(child.try_wait()?.is_none());
        }
        for process in installer_process::running(install)? {
            if process.pid == pid && !process.ui {
                return Ok(!process.wait(Duration::ZERO)?);
            }
        }
        Ok(false)
    }

    fn resume(install: &Path, saved: &State) -> Result<(), Error> {
        if installer_process::active()? {
            return Err(Error::State(
                "release the maintenance gate before resuming processes",
            ));
        }
        let current = installer_process::running(install)?;
        let mut daemon = None;
        let daemon_pid = if let Some(process) = current.iter().find(|p| !p.ui) {
            Some(process.pid)
        } else if saved.independent_daemon {
            let child = Command::new(install.join("tidemarkd.exe"))
                .creation_flags(NO_WINDOW)
                .spawn()?;
            let pid = child.id();
            daemon = Some(child);
            Some(pid)
        } else {
            None
        };
        if let Some(pid) = daemon_pid {
            wait_daemon(
                install,
                pid,
                &mut daemon,
                saved.legacy && !install.join("install-manifest.json").exists(),
            )?;
        }
        if saved.ui && !installer_process::running(install)?.iter().any(|p| p.ui) {
            let mut command = Command::new(install.join("tidemark.exe"));
            if !saved.visible {
                command.arg("--background");
            }
            let mut child = command.creation_flags(NO_WINDOW).spawn()?;
            std::thread::sleep(Duration::from_millis(150));
            if child.try_wait()?.is_some() {
                return Err(Error::State(
                    "restored GUI exited during startup; recovery state has been preserved",
                ));
            }
        }
        // Dropping std::process::Child closes its handle without killing the process.
        Ok(())
    }

    fn finalize(install: &Path, state: &Path, saved: &State) -> Result<(), Error> {
        if !saved.finalizing {
            let mut finalizing = saved.clone();
            finalizing.finalizing = true;
            publish_state(install, state, &finalizing)?;
        }
        installer_integration::finalize(state)?;
        match saved.mode {
            Mode::Setup => installation::finalize(state)?,
            Mode::LegacyDirectory => installation::finalize_directory(state)?,
            Mode::Remove => (),
        }
        // A scanner can hold the freshly published state file for a moment; a
        // plain remove would strand the transaction and block the next setup.
        let state_file = installation::state_file(install, state, STATE)?;
        let mut removal = match fs::remove_file(&state_file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Some(error),
            _ => None,
        };
        for _ in 0..5 {
            if removal.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            removal = match fs::remove_file(&state_file) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Some(error),
                _ => None,
            };
        }
        if let Some(error) = removal {
            return Err(error.into());
        }
        match fs::remove_dir(state) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn publish_state(install: &Path, state: &Path, saved: &State) -> Result<(), Error> {
        use std::io::Write;
        let pending = installation::state_file(install, state, STATE_PENDING)?;
        let destination = installation::state_file(install, state, STATE)?;
        // A valid published state proves this reserved pending file belongs to a
        // prior interrupted publication. Initial preparation rejects it instead.
        if destination.exists() && pending.exists() {
            let _ = load(install, state)?;
            fs::remove_file(&pending)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec(saved)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&pending, &destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(installation::state_file(install, state, STATE_PENDING)?);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let root = std::env::temp_dir().join(format!(
                    "tidemark-maintenance-{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&root).unwrap();
                fs::create_dir(root.join("install")).unwrap();
                Self(root)
            }
            fn install(&self) -> PathBuf {
                self.0.join("install")
            }
            fn state(&self) -> PathBuf {
                self.0.join("state")
            }
            fn saved(&self, mode: Mode) -> State {
                State {
                    install: self.install().canonicalize().unwrap(),
                    ui: false,
                    visible: false,
                    independent_daemon: false,
                    task: None,
                    run: None,
                    legacy: false,
                    mode,
                    remove_files: Vec::new(),
                    finalizing: false,
                    shortcut: None,
                }
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn a_stranded_removal_with_no_files_left_is_finalized_automatically() {
            let f = Fixture::new();
            let mut saved = f.saved(Mode::Remove);
            saved.remove_files = vec!["tidemark.exe".into()];
            saved.shortcut = Some(f.install().join("Tidemark.lnk"));
            fs::create_dir_all(f.state()).unwrap();
            publish_state(&f.install(), &f.state(), &saved).unwrap();
            // The owned file still exists: no self-heal, the state stays for recovery.
            fs::write(f.install().join("tidemark.exe"), b"present").unwrap();
            assert!(!removal_already_complete(&f.install(), &f.state()).unwrap());
            assert!(f.state().join(STATE).exists());
            fs::remove_file(f.install().join("tidemark.exe")).unwrap();
            assert!(removal_already_complete(&f.install(), &f.state()).unwrap());
            assert!(!f.state().join(STATE).exists());
        }

        #[test]
        fn uninstall_prepare_does_not_read_the_locked_uninstaller_database() {
            use std::os::windows::fs::OpenOptionsExt;
            let f = Fixture::new();
            fs::write(f.install().join("unins000.dat"), b"live database").unwrap();
            let _lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(f.install().join("unins000.dat"))
                .unwrap();
            prepare_saved(
                &f.install(),
                &f.state(),
                None,
                &f.saved(Mode::Remove),
                || Ok(()),
                || Ok(()),
            )
            .unwrap();
            assert!(f.state().join(STATE).exists());
            assert!(!f.state().join("journal.json").exists());
        }

        #[test]
        fn failed_task_pause_unwinds_new_backup_and_process_state() {
            let f = Fixture::new();
            fs::write(f.install().join("tidemark.exe"), b"original").unwrap();
            let files = vec!["tidemark.exe".into()];
            let error = prepare_saved(
                &f.install(),
                &f.state(),
                Some(&files),
                &f.saved(Mode::Setup),
                || Err(Error::Task("pause failed".into())),
                || Ok(()),
            );
            assert!(error.is_err());
            assert!(!f.state().join(STATE).exists());
            assert!(!f.state().join("journal.json").exists());
            assert_eq!(
                fs::read(f.install().join("tidemark.exe")).unwrap(),
                b"original"
            );
        }

        #[test]
        fn failed_task_unwind_retains_recovery_state() {
            let f = Fixture::new();
            let saved = f.saved(Mode::Setup);
            let error = prepare_saved(
                &f.install(),
                &f.state(),
                Some(&[]),
                &saved,
                || Err(Error::Task("pause failed".into())),
                || Err(Error::Task("restore failed".into())),
            );
            assert!(error.is_err());
            assert!(f.state().join(STATE).exists());
            assert!(f.state().join("journal.json").exists());
        }

        #[test]
        fn recovery_restores_setup_files_and_retains_journal_until_full_success() {
            let f = Fixture::new();
            fs::write(f.install().join("tidemark.exe"), b"original").unwrap();
            let files = vec!["tidemark.exe".into()];
            installation::backup(&f.install(), &f.state(), &files).unwrap();
            fs::write(f.install().join("tidemark.exe"), b"replacement").unwrap();
            recover_files(&f.install(), &f.state(), &f.saved(Mode::Setup)).unwrap();
            assert_eq!(
                fs::read(f.install().join("tidemark.exe")).unwrap(),
                b"original"
            );
            assert!(f.state().join("journal.json").exists());
        }

        #[test]
        fn directory_prepare_defers_archiving_and_recovers_the_entire_original() {
            let f = Fixture::new();
            fs::write(f.install().join("tidemark.exe"), b"old").unwrap();
            fs::write(f.install().join("opaque.bin"), b"opaque").unwrap();
            let saved = f.saved(Mode::LegacyDirectory);
            let incoming = vec!["tidemark.exe".into()];
            prepare_saved(
                &f.install(),
                &f.state(),
                Some(&incoming),
                &saved,
                || Ok(()),
                || Ok(()),
            )
            .unwrap();
            assert!(!f.state().join("files").exists());
            assert!(!f.state().join("previous-install").exists());
            assert_eq!(fs::read(f.install().join("opaque.bin")).unwrap(), b"opaque");
            run(&arguments(&f, "archive")).unwrap();
            assert!(fs::read_dir(f.install()).unwrap().next().is_none());
            fs::write(f.install().join("tidemark.exe"), b"new").unwrap();
            recover_files(&f.install(), &f.state(), &saved).unwrap();
            assert_eq!(fs::read(f.install().join("opaque.bin")).unwrap(), b"opaque");
            assert_eq!(fs::read(f.install().join("tidemark.exe")).unwrap(), b"old");
            finalize(&f.install(), &f.state(), &saved).unwrap();
            assert!(!f.state().exists());
        }

        #[test]
        fn failed_directory_task_pause_cleans_only_the_preparation() {
            let f = Fixture::new();
            fs::write(f.install().join("opaque.bin"), b"opaque").unwrap();
            fs::create_dir(f.state()).unwrap();
            fs::write(f.state().join("other.txt"), b"preserve").unwrap();
            let error = prepare_saved(
                &f.install(),
                &f.state(),
                Some(&[]),
                &f.saved(Mode::LegacyDirectory),
                || Err(Error::Task("pause failed".into())),
                || Ok(()),
            );
            assert!(error.is_err());
            assert!(!f.state().join(STATE).exists());
            assert!(!f.state().join("directory-state.json").exists());
            assert_eq!(fs::read(f.state().join("other.txt")).unwrap(), b"preserve");
            assert_eq!(fs::read(f.install().join("opaque.bin")).unwrap(), b"opaque");
        }

        #[test]
        fn interrupted_cleanup_publishes_finalizing_before_destroying_snapshots() {
            use std::os::windows::fs::OpenOptionsExt;
            let f = Fixture::new();
            fs::write(f.install().join("a.exe"), b"old a").unwrap();
            fs::write(f.install().join("b.exe"), b"old b").unwrap();
            let files = vec!["a.exe".into(), "b.exe".into()];
            let saved = f.saved(Mode::Setup);
            prepare_saved(
                &f.install(),
                &f.state(),
                Some(&files),
                &saved,
                || Ok(()),
                || Ok(()),
            )
            .unwrap();
            fs::write(f.install().join("a.exe"), b"new a").unwrap();
            fs::write(f.install().join("b.exe"), b"new b").unwrap();
            let lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(f.state().join("files/1"))
                .unwrap();
            assert!(finalize(&f.install(), &f.state(), &saved).is_err());
            assert!(load(&f.install(), &f.state()).unwrap().finalizing);
            assert!(!f.state().join("files/0").exists());
            drop(lock);
            finalize(
                &f.install(),
                &f.state(),
                &load(&f.install(), &f.state()).unwrap(),
            )
            .unwrap();
            assert_eq!(fs::read(f.install().join("a.exe")).unwrap(), b"new a");
            assert_eq!(fs::read(f.install().join("b.exe")).unwrap(), b"new b");
            assert!(!f.state().exists());
        }

        #[test]
        fn interrupted_state_publication_keeps_valid_state_and_can_retry() {
            let f = Fixture::new();
            let saved = f.saved(Mode::Remove);
            prepare_saved(&f.install(), &f.state(), None, &saved, || Ok(()), || Ok(())).unwrap();
            fs::write(f.state().join(STATE_PENDING), b"{partial").unwrap();
            assert!(!load(&f.install(), &f.state()).unwrap().finalizing);
            let mut replacement = saved.clone();
            replacement.finalizing = true;
            publish_state(&f.install(), &f.state(), &replacement).unwrap();
            assert!(load(&f.install(), &f.state()).unwrap().finalizing);
            assert!(!f.state().join(STATE_PENDING).exists());
        }

        #[test]
        fn preparation_preserves_a_preexisting_reserved_pending_file() {
            let f = Fixture::new();
            fs::create_dir(f.state()).unwrap();
            fs::write(f.state().join(STATE_PENDING), b"preexisting").unwrap();
            assert!(
                prepare_saved(
                    &f.install(),
                    &f.state(),
                    None,
                    &f.saved(Mode::Remove),
                    || Ok(()),
                    || Ok(())
                )
                .is_err()
            );
            assert_eq!(
                fs::read(f.state().join(STATE_PENDING)).unwrap(),
                b"preexisting"
            );
            assert!(!f.state().join(STATE).exists());
        }

        #[test]
        fn unpublished_preparation_can_be_recovered_without_changing_program_files() {
            for directory in [false, true] {
                let f = Fixture::new();
                fs::write(f.install().join("tidemark.exe"), b"original").unwrap();
                if directory {
                    installation::prepare_directory(&f.install(), &f.state()).unwrap();
                } else {
                    installation::backup(&f.install(), &f.state(), &["tidemark.exe".into()])
                        .unwrap();
                }
                fs::write(f.state().join(STATE_PENDING), b"{partial").unwrap();
                fs::write(f.state().join("unrelated.txt"), b"preserve").unwrap();
                recover_unpublished(&f.install(), &f.state()).unwrap();
                assert_eq!(
                    fs::read(f.install().join("tidemark.exe")).unwrap(),
                    b"original"
                );
                assert_eq!(
                    fs::read(f.state().join("unrelated.txt")).unwrap(),
                    b"preserve"
                );
                assert!(!f.state().join(STATE_PENDING).exists());
            }
        }

        #[test]
        fn unpublished_recovery_preserves_unbound_pending_and_moved_archives() {
            let f = Fixture::new();
            fs::create_dir(f.state()).unwrap();
            fs::write(f.state().join(STATE_PENDING), b"preexisting").unwrap();
            assert!(recover_unpublished(&f.install(), &f.state()).is_err());
            assert!(f.state().join(STATE_PENDING).exists());
            installation::prepare_directory(&f.install(), &f.state()).unwrap();
            installation::archive_directory(&f.install(), &f.state()).unwrap();
            assert!(recover_unpublished(&f.install(), &f.state()).is_err());
            assert!(f.state().join("previous-install").exists());
            assert!(f.state().join(STATE_PENDING).exists());
        }

        #[test]
        fn daemon_readiness_rejects_a_socket_that_never_answers_version() {
            let f = Fixture::new();
            let socket = f.0.join("test.sock");
            let listener = uds_windows::UnixListener::bind(&socket).unwrap();
            let worker = std::thread::spawn(move || {
                let (_stream, _) = listener.accept().unwrap();
                std::thread::sleep(Duration::from_millis(150));
            });
            let result = probe_ready(&socket, Duration::from_millis(30), std::process::id());
            assert!(result.is_err());
            worker.join().unwrap();
        }

        #[test]
        fn legacy_readiness_rejects_another_process_endpoint() {
            let f = Fixture::new();
            let socket = f.0.join("wrong.sock");
            let listener = uds_windows::UnixListener::bind(&socket).unwrap();
            let worker = std::thread::spawn(move || {
                let (_stream, _) = listener.accept().unwrap();
                std::thread::sleep(Duration::from_millis(150));
            });
            let result = probe_ready(&socket, Duration::from_secs(1), std::process::id() + 1);
            assert!(
                matches!(
                    result,
                    Err(Error::State("daemon endpoint belongs to another process"))
                ),
                "{result:?}"
            );
            worker.join().unwrap();
        }

        fn arguments(f: &Fixture, operation: &str) -> Vec<std::ffi::OsString> {
            vec![
                "helper".into(),
                operation.into(),
                f.install().into(),
                f.state().into(),
                "unused-manifest".into(),
                f.0.join("error.txt").into(),
            ]
        }

        #[test]
        fn uninstall_verification_keeps_state_while_owned_payload_remains() {
            let f = Fixture::new();
            fs::write(f.install().join("tidemark.exe"), b"left over").unwrap();
            let mut saved = f.saved(Mode::Remove);
            saved.remove_files = vec!["tidemark.exe".into(), "unins000.dat".into()];
            prepare_saved(&f.install(), &f.state(), None, &saved, || Ok(()), || Ok(())).unwrap();
            assert!(run(&arguments(&f, "verify-remove")).is_err());
            assert!(f.state().join(STATE).exists());
            fs::remove_file(f.install().join("tidemark.exe")).unwrap();
            fs::write(f.install().join("unins000.dat"), b"live uninstaller").unwrap();
            fs::write(f.install().join("personal.txt"), b"personal").unwrap();
            run(&arguments(&f, "verify-remove")).unwrap();
            assert_eq!(
                fs::read(f.install().join("personal.txt")).unwrap(),
                b"personal"
            );
        }

        #[test]
        fn uninstall_state_can_be_finalized_after_the_empty_install_root_was_removed() {
            let f = Fixture::new();
            prepare_saved(
                &f.install(),
                &f.state(),
                None,
                &f.saved(Mode::Remove),
                || Ok(()),
                || Ok(()),
            )
            .unwrap();
            fs::remove_dir(f.install()).unwrap();
            run(&arguments(&f, "verify-remove")).unwrap();
            run(&arguments(&f, "finalize-remove")).unwrap();
            assert!(!f.state().exists());
        }
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().collect();
        match native::run(&args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                if let Some(path) = args.get(5) {
                    let _ = std::fs::write(path, error.to_string());
                }
                eprintln!("{error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(windows))]
    std::process::ExitCode::SUCCESS
}
