#[cfg(windows)]
mod windows_gate {
    use std::error::Error;
    use std::ffi::OsStr;
    use std::fs;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use tidemark_ipc_p2p_spike::{acl_receipt, current_user_sid, default_endpoint};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessWithLogonW, GetExitCodeProcess, LOGON_NETCREDENTIALS_ONLY,
        PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
    const RESTART_CYCLES: u32 = 100;

    type GateResult<T> = Result<T, Box<dyn Error>>;

    pub fn run() -> GateResult<()> {
        let binaries = Binaries::locate()?;
        let mut root = ScratchRoot::new()?;
        let mut child_processes = 0_u32;

        let os = powershell(
            "$o=Get-CimInstance Win32_OperatingSystem; \"$($o.Caption)|$($o.Version)|$($o.BuildNumber)|$($o.OSArchitecture)\"",
        )?;
        println!("G1 OS: {}", os.trim());
        let defender_before = defender_status()?;
        println!("G1 DEFENDER BEFORE: {defender_before}");

        let product_endpoint = default_endpoint()?;
        let product_run = product_endpoint
            .parent()
            .ok_or("product endpoint has no parent")?
            .to_owned();
        let product_run_existed = product_run.exists();
        if product_endpoint.exists() {
            return Err(format!(
                "refusing to disturb an active product endpoint: {}",
                product_endpoint.display()
            )
            .into());
        }
        {
            let server = ServerChild::spawn(&binaries.server, &product_endpoint, "g1-product")?;
            child_processes += 1;
            let report = run_client(&binaries.client, &product_endpoint)?;
            child_processes += 1;
            assert_report(&report, "g1-product", "zai")?;
            let acl = acl_receipt(&product_run)?;
            let lowered = acl.to_ascii_lowercase();
            if lowered.contains("everyone") || lowered.contains("builtin\\users") {
                return Err(format!("endpoint directory ACL is broad: {acl}").into());
            }
            println!(
                "G1 PRODUCT ENDPOINT PASS: path={} bytes={} sid={} acl={}",
                product_endpoint.display(),
                utf8_len(&product_endpoint)?,
                current_user_sid()?,
                one_line(&acl)
            );
            server.stop()?;
        }
        assert_absent(&product_endpoint)?;
        assert_absent(&singleton_path(&product_endpoint)?)?;
        if !product_run_existed {
            fs::remove_dir(&product_run)?;
        }

        let unicode_endpoint = root.path().join("用户-é-run").join("d.sock");
        {
            let server = ServerChild::spawn(&binaries.server, &unicode_endpoint, "g1-unicode")?;
            child_processes += 1;
            let report = run_client(&binaries.client, &unicode_endpoint)?;
            child_processes += 1;
            assert_report(&report, "g1-unicode", "zai")?;
            server.stop()?;
        }
        println!(
            "G1 NON-ASCII PATH PASS: path={} utf8_bytes={}",
            unicode_endpoint.display(),
            utf8_len(&unicode_endpoint)?
        );

        let near_limit = endpoint_with_utf8_len(root.path(), 107)?;
        {
            let server = ServerChild::spawn(&binaries.server, &near_limit, "g1-path-107")?;
            child_processes += 1;
            let report = run_client(&binaries.client, &near_limit)?;
            child_processes += 1;
            assert_report(&report, "g1-path-107", "zai")?;
            server.stop()?;
        }
        println!(
            "G1 PATH BOUNDARY PASS: 107-byte pathname bound and connected ({})",
            near_limit.display()
        );

        let at_limit = endpoint_with_utf8_len(root.path(), 108)?;
        let rejection = Command::new(&binaries.server)
            .args([
                "--socket",
                path_text(&at_limit)?,
                "--version",
                "must-reject",
            ])
            .stdin(Stdio::null())
            .output()?;
        child_processes += 1;
        let rejection_stdout = String::from_utf8(rejection.stdout)?;
        let rejection_stderr = String::from_utf8(rejection.stderr)?;
        if rejection.status.success()
            || rejection_stdout.contains("READY")
            || !rejection_stderr.contains("path must be shorter than SUN_LEN")
        {
            return Err(format!(
                "108-byte pathname was not rejected honestly: status={} stdout={rejection_stdout:?} stderr={rejection_stderr:?}",
                rejection.status
            )
            .into());
        }
        println!(
            "G1 PATH LIMIT CLASSIFIED: 108-byte pathname rejected verbatim: path must be shorter than SUN_LEN"
        );

        let session_endpoint = root.path().join("second-logon").join("d.sock");
        {
            let server =
                ServerChild::spawn(&binaries.server, &session_endpoint, "g1-second-logon")?;
            child_processes += 1;
            let receipt = root.path().join("second-logon-client.txt");
            let parent_logon = current_logon_sid()?;
            launch_in_new_logon(&binaries.client, &session_endpoint, &receipt)?;
            child_processes += 1;
            let report = fs::read_to_string(&receipt)?;
            assert_report(&report, "g1-second-logon", "zai")?;
            let child_user = field(&report, "user_sid")?;
            let child_logon = field(&report, "logon_sid")?;
            if child_user != current_user_sid()? {
                return Err(format!(
                    "new-logon client changed user SID: parent={} child={child_user}",
                    current_user_sid()?
                )
                .into());
            }
            if child_logon == parent_logon {
                // LOGON_NETCREDENTIALS_ONLY is documented to reuse the caller's logon session;
                // a genuinely distinct same-user session needs the account password, so this
                // probe is reported as user-run VM residue instead of a false pass or fail.
                println!(
                    "G1 SAME-USER SECOND-SESSION RESIDUE: same-SID child connected, but a distinct logon session requires user-run execution (session={child_logon})"
                );
            } else {
                println!(
                    "G1 SAME-USER SECOND-SESSION PASS: user_sid={child_user} parent_logon={parent_logon} child_logon={child_logon}"
                );
            }
            server.stop()?;
            fs::remove_file(receipt)?;
        }

        let truth_endpoint = root.path().join("truthful-output").join("d.sock");
        {
            let server = ServerChild::spawn(&binaries.server, &truth_endpoint, "g1-truth")?;
            child_processes += 1;
            let collision = Command::new(&binaries.server)
                .args([
                    "--socket",
                    path_text(&truth_endpoint)?,
                    "--version",
                    "collision",
                ])
                .stdin(Stdio::null())
                .output()?;
            child_processes += 1;
            if collision.status.success()
                || String::from_utf8_lossy(&collision.stdout).contains("READY")
            {
                return Err("a singleton collision emitted misleading READY output".into());
            }
            server.stop()?;
        }
        let absent_endpoint = root.path().join("absent").join("d.sock");
        let absent = Command::new(&binaries.client)
            .args(["--socket", path_text(&absent_endpoint)?])
            .output()?;
        child_processes += 1;
        if absent.status.success() || String::from_utf8_lossy(&absent.stdout).contains("PASS") {
            return Err("an absent endpoint emitted misleading client PASS output".into());
        }
        println!(
            "G1 MISLEADING-SUCCESS GUARD PASS: collision had no READY; absent endpoint had no PASS"
        );

        let cancellation_endpoint = root.path().join("cancel-resume").join("d.sock");
        let server = ServerChild::spawn(&binaries.server, &cancellation_endpoint, "g1-cancel")?;
        child_processes += 1;
        let watcher = LineChild::spawn(
            &binaries.client,
            &[
                "--socket",
                path_text(&cancellation_endpoint)?,
                "--await-close",
            ],
        )?;
        child_processes += 1;
        let pass = watcher.line()?;
        if !pass.starts_with("PASS ") {
            return Err(format!("watch client did not prove its proxy first: {pass:?}").into());
        }
        let connected = watcher.line()?;
        if !connected.starts_with("CONNECTED ") {
            return Err(format!("watch client did not reach its barrier: {connected:?}").into());
        }
        server.kill()?;
        let closed = watcher.line()?;
        if !closed.starts_with("CLOSED ") {
            return Err(format!("watch client did not observe EOF: {closed:?}").into());
        }
        watcher.wait_success()?;
        let replacement =
            ServerChild::spawn(&binaries.server, &cancellation_endpoint, "g1-resumed")?;
        child_processes += 1;
        let report = run_client(&binaries.client, &cancellation_endpoint)?;
        child_processes += 1;
        assert_report(&report, "g1-resumed", "zai")?;
        replacement.stop()?;
        println!(
            "G1 CANCEL/RESUME PASS: client observed EOF, stale endpoint rebound, proxy reloaded"
        );

        let restart_endpoint = root.path().join("restart-stress").join("d.sock");
        fs::create_dir_all(
            restart_endpoint
                .parent()
                .ok_or("restart path has no parent")?,
        )?;
        fs::write(&restart_endpoint, b"deliberately stale before cycle zero")?;
        let mut stale_observed = 0_u32;
        for cycle in 0..RESTART_CYCLES {
            let version = format!("kill-{cycle}");
            let server = ServerChild::spawn(&binaries.server, &restart_endpoint, &version)?;
            child_processes += 1;
            let report = run_client(&binaries.client, &restart_endpoint)?;
            child_processes += 1;
            assert_report(&report, &version, "zai")?;
            server.kill()?;
            if restart_endpoint.exists() {
                stale_observed += 1;
            }
            if (cycle + 1) % 10 == 0 {
                println!("G1 RESTART PROGRESS: {}/{}", cycle + 1, RESTART_CYCLES);
            }
        }
        let final_server = ServerChild::spawn(&binaries.server, &restart_endpoint, "kill-final")?;
        child_processes += 1;
        let final_report = run_client(&binaries.client, &restart_endpoint)?;
        child_processes += 1;
        assert_report(&final_report, "kill-final", "zai")?;
        final_server.stop()?;
        println!(
            "G1 KILL/RESTART PASS: {RESTART_CYCLES}/{} forced cycles; stale_socket_observed_after_kill={stale_observed}; final graceful recovery passed",
            RESTART_CYCLES
        );

        let defender_after = defender_status()?;
        println!("G1 DEFENDER AFTER: {defender_after}");
        root.cleanup()?;
        if root.path().exists() {
            return Err(format!(
                "temporary G1 root survived cleanup: {}",
                root.path().display()
            )
            .into());
        }
        assert_no_server_process()?;
        println!(
            "G1 CLEANUP PASS: child_processes_waited={child_processes}; product_socket_absent=true; temp_root_absent=true; ipc-p2p-server.exe processes=0"
        );
        println!("G1 LOCAL PASS");
        Ok(())
    }

    #[derive(Debug)]
    struct Binaries {
        server: PathBuf,
        client: PathBuf,
    }

    impl Binaries {
        fn locate() -> GateResult<Self> {
            let directory = std::env::current_exe()?
                .parent()
                .ok_or("G1 executable has no parent directory")?
                .to_owned();
            let server = directory.join("ipc-p2p-server.exe");
            let client = directory.join("ipc-p2p-client.exe");
            for binary in [&server, &client] {
                if !binary.is_file() {
                    return Err(format!(
                        "missing {}; run `cargo build --manifest-path spikes/ipc-p2p/Cargo.toml --bins` first",
                        binary.display()
                    )
                    .into());
                }
            }
            Ok(Self { server, client })
        }
    }

    #[derive(Debug)]
    struct ScratchRoot {
        path: PathBuf,
        cleaned: bool,
    }

    impl ScratchRoot {
        fn new() -> GateResult<Self> {
            let path =
                PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unset")?)
                    .join(format!("tidemark-ipc-p2p-g1-{}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            Ok(Self {
                path,
                cleaned: false,
            })
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn cleanup(&mut self) -> GateResult<()> {
            fs::remove_dir_all(&self.path)?;
            self.cleaned = true;
            Ok(())
        }
    }

    impl Drop for ScratchRoot {
        fn drop(&mut self) {
            if !self.cleaned {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    #[derive(Debug)]
    struct ServerChild {
        child: Option<Child>,
        stdin: Option<ChildStdin>,
        lines: Receiver<String>,
        pid: u32,
    }

    impl ServerChild {
        fn spawn(binary: &Path, endpoint: &Path, version: &str) -> GateResult<Self> {
            let mut child = Command::new(binary)
                .args(["--socket", path_text(endpoint)?, "--version", version])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let pid = child.id();
            let stdin = child.stdin.take().ok_or("server stdin was not piped")?;
            let stdout = child.stdout.take().ok_or("server stdout was not piped")?;
            let (send, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if send.send(line).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            let server = Self {
                child: Some(child),
                stdin: Some(stdin),
                lines,
                pid,
            };
            let ready = server.line()?;
            if !ready.starts_with("READY ") {
                return Err(
                    format!("server {pid} did not emit READY after binding: {ready:?}").into(),
                );
            }
            Ok(server)
        }

        fn line(&self) -> GateResult<String> {
            self.lines
                .recv_timeout(PROCESS_TIMEOUT)
                .map_err(|error| format!("server {} event wait failed: {error}", self.pid).into())
        }

        fn stop(mut self) -> GateResult<()> {
            let mut stdin = self.stdin.take().ok_or("server stdin already closed")?;
            writeln!(stdin, "stop")?;
            stdin.flush()?;
            drop(stdin);
            let stopped = self.line()?;
            if !stopped.starts_with("STOPPED ") {
                return Err(
                    format!("server {} did not confirm cleanup: {stopped:?}", self.pid).into(),
                );
            }
            let status = self
                .child
                .as_mut()
                .ok_or("server process already consumed")?
                .wait()?;
            self.child.take();
            if !status.success() {
                return Err(format!("server {} exited with {status}", self.pid).into());
            }
            Ok(())
        }

        fn kill(mut self) -> GateResult<()> {
            let child = self
                .child
                .as_mut()
                .ok_or("server process already consumed")?;
            child.kill()?;
            let status = child.wait()?;
            self.child.take();
            self.stdin.take();
            if status.success() {
                return Err(
                    format!("forced server {} exit unexpectedly succeeded", self.pid).into(),
                );
            }
            Ok(())
        }
    }

    impl Drop for ServerChild {
        fn drop(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[derive(Debug)]
    struct LineChild {
        child: std::sync::Mutex<Option<Child>>,
        lines: Receiver<String>,
    }

    impl LineChild {
        fn spawn(binary: &Path, arguments: &[&str]) -> GateResult<Self> {
            let mut child = Command::new(binary)
                .args(arguments)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let stdout = child.stdout.take().ok_or("client stdout was not piped")?;
            let (send, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if send.send(line).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                child: std::sync::Mutex::new(Some(child)),
                lines,
            })
        }

        fn line(&self) -> GateResult<String> {
            self.lines
                .recv_timeout(PROCESS_TIMEOUT)
                .map_err(|error| format!("client event wait failed: {error}").into())
        }

        fn wait_success(self) -> GateResult<()> {
            let mut child = self
                .child
                .lock()
                .map_err(|_| "client process lock was poisoned")?
                .take()
                .ok_or("client process already consumed")?;
            let status = child.wait()?;
            if !status.success() {
                return Err(format!("watch client exited with {status}").into());
            }
            Ok(())
        }
    }

    impl Drop for LineChild {
        fn drop(&mut self) {
            if let Ok(mut child) = self.child.lock()
                && let Some(child) = child.as_mut()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn run_client(binary: &Path, endpoint: &Path) -> GateResult<String> {
        let output = Command::new(binary)
            .args(["--socket", path_text(endpoint)?])
            .output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if !output.status.success() || !stdout.starts_with("PASS ") {
            return Err(format!(
                "proxy client failed: status={} stdout={stdout:?} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(stdout)
    }

    fn assert_report(report: &str, version: &str, provider: &str) -> GateResult<()> {
        if field(report, "version")? != version
            || field(report, "statuses")? != "1"
            || field(report, "provider")? != provider
        {
            return Err(format!("client report did not match the live server: {report:?}").into());
        }
        Ok(())
    }

    fn field<'a>(report: &'a str, name: &str) -> GateResult<&'a str> {
        let prefix = format!("{name}=");
        report
            .split_ascii_whitespace()
            .find_map(|field| field.strip_prefix(&prefix))
            .ok_or_else(|| format!("report has no {name} field: {report:?}").into())
    }

    fn endpoint_with_utf8_len(root: &Path, target: usize) -> GateResult<PathBuf> {
        let suffix = Path::new("d.sock");
        let fixed = utf8_len(root)? + 1 + 1 + utf8_len(suffix)?;
        let fill = target
            .checked_sub(fixed)
            .ok_or("scratch root is too long for requested AF_UNIX boundary")?;
        let endpoint = root.join("x".repeat(fill)).join(suffix);
        if utf8_len(&endpoint)? != target {
            return Err(format!(
                "constructed pathname has {} bytes instead of {target}",
                utf8_len(&endpoint)?
            )
            .into());
        }
        Ok(endpoint)
    }

    fn utf8_len(path: &Path) -> GateResult<usize> {
        Ok(path
            .to_str()
            .ok_or("Windows pathname is not UTF-8")?
            .as_bytes()
            .len())
    }

    fn path_text(path: &Path) -> GateResult<&str> {
        path.to_str()
            .ok_or_else(|| "Windows pathname is not UTF-8".into())
    }

    fn singleton_path(endpoint: &Path) -> GateResult<PathBuf> {
        Ok(endpoint.with_file_name(format!(
            "{}.singleton",
            endpoint
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("endpoint filename is not UTF-8")?
        )))
    }

    fn assert_absent(path: &Path) -> GateResult<()> {
        if path.exists() {
            Err(format!("cleanup left {}", path.display()).into())
        } else {
            Ok(())
        }
    }

    fn one_line(value: &str) -> String {
        value.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn powershell(script: &str) -> GateResult<String> {
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "PowerShell probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn defender_status() -> GateResult<String> {
        let value = powershell(
            "$s=Get-MpComputerStatus; \"AM=$($s.AMServiceEnabled)|AV=$($s.AntivirusEnabled)|RTP=$($s.RealTimeProtectionEnabled)|Signature=$($s.AntivirusSignatureVersion)\"",
        )?;
        let value = value.trim().to_owned();
        if !value.contains("AM=True") || !value.contains("AV=True") || !value.contains("RTP=True") {
            return Err(format!("Microsoft Defender is not fully active: {value}").into());
        }
        Ok(value)
    }

    fn current_logon_sid() -> GateResult<String> {
        let output = Command::new("whoami.exe").arg("/logonid").output()?;
        if !output.status.success() {
            return Err(format!(
                "whoami.exe /logonid failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let sid = String::from_utf8(output.stdout)?.trim().to_owned();
        if !sid.starts_with("S-1-5-5-") {
            return Err(format!("malformed logon SID {sid:?}").into());
        }
        Ok(sid)
    }

    fn launch_in_new_logon(client: &Path, endpoint: &Path, receipt: &Path) -> GateResult<()> {
        let username = std::env::var("USERNAME")?;
        let domain = std::env::var("USERDOMAIN")?;
        let command_line = command_line(&[
            client.as_os_str(),
            OsStr::new("--socket"),
            endpoint.as_os_str(),
            OsStr::new("--receipt"),
            receipt.as_os_str(),
        ]);
        let mut command_line = wide(&command_line);
        let application = wide(client.as_os_str());
        let username = wide(OsStr::new(&username));
        let domain = wide(OsStr::new(&domain));
        // LOGON_NETCREDENTIALS_ONLY deliberately does not validate these network-only
        // credentials. Local access retains the current user's SID and logon session.
        let password = wide(OsStr::new("tidemark-spike-unused"));
        let current = wide(
            client
                .parent()
                .ok_or("client has no parent directory")?
                .as_os_str(),
        );
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: every UTF-16 buffer is NUL-terminated and retained for the call; command_line
        // is mutable as required by CreateProcessWithLogonW; both output handles are initialized
        // into `process` and closed below.
        unsafe {
            CreateProcessWithLogonW(
                PCWSTR(username.as_ptr()),
                PCWSTR(domain.as_ptr()),
                PCWSTR(password.as_ptr()),
                LOGON_NETCREDENTIALS_ONLY,
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                CREATE_NO_WINDOW,
                None,
                PCWSTR(current.as_ptr()),
                &startup,
                &mut process,
            )?;
        }
        let process_handle = OwnedHandle(process.hProcess);
        let thread_handle = OwnedHandle(process.hThread);
        let timeout_ms = u32::try_from(PROCESS_TIMEOUT.as_millis())?;
        // SAFETY: process_handle remains valid and owned for this wait.
        let wait = unsafe { WaitForSingleObject(process_handle.0, timeout_ms) };
        if wait == WAIT_TIMEOUT {
            // SAFETY: the valid process handle is still owned and the timeout path must not leak it.
            unsafe { TerminateProcess(process_handle.0, 124)? };
            return Err("new-logon client timed out".into());
        }
        if wait != WAIT_OBJECT_0 {
            return Err(format!("new-logon client wait returned {wait:?}").into());
        }
        let mut exit_code = u32::MAX;
        // SAFETY: the process is signaled and the valid handle remains live for the query.
        unsafe { GetExitCodeProcess(process_handle.0, &mut exit_code)? };
        drop(thread_handle);
        drop(process_handle);
        if exit_code != 0 {
            return Err(format!("new-logon client exited with code {exit_code}").into());
        }
        Ok(())
    }

    #[derive(Debug)]
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: this guard uniquely owns the process or thread handle returned by
                // CreateProcessWithLogonW and closes it exactly once.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn command_line(arguments: &[&OsStr]) -> std::ffi::OsString {
        let mut command = std::ffi::OsString::new();
        for (index, argument) in arguments.iter().enumerate() {
            if index != 0 {
                command.push(" ");
            }
            command.push(quote_windows_argument(argument));
        }
        command
    }

    fn quote_windows_argument(argument: &OsStr) -> std::ffi::OsString {
        let text = argument.to_string_lossy();
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for character in text.chars() {
            match character {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                other => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    quoted.push(other);
                    backslashes = 0;
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted.into()
    }

    fn assert_no_server_process() -> GateResult<()> {
        let output = Command::new("tasklist.exe")
            .args([
                "/FI",
                "IMAGENAME eq ipc-p2p-server.exe",
                "/FO",
                "CSV",
                "/NH",
            ])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "tasklist cleanup query failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let listing = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        if listing.contains("ipc-p2p-server.exe") {
            return Err(format!("server process survived cleanup: {listing}").into());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_gate::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("VM gate G1 is Windows-only");
    std::process::exit(2);
}
