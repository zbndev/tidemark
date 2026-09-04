use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;

use tidemark_ipc_p2p_spike::{bounded, connect, current_user_sid};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut endpoint = None;
    let mut receipt = None;
    let mut await_close = false;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--socket") => endpoint = args.next().map(PathBuf::from),
            Some("--receipt") => receipt = args.next().map(PathBuf::from),
            Some("--await-close") => await_close = true,
            Some(other) => return Err(format!("unknown argument {other:?}").into()),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    let endpoint = endpoint.ok_or("--socket is required")?;
    let client = bounded("client connection", connect(&endpoint)).await??;
    bounded("client Hello", client.hello("process-client", false)).await??;
    let version = bounded("Version reply", client.proxy().version()).await??;
    let statuses = bounded("GetStatus reply", client.proxy().get_status()).await??;
    let report = format!(
        "PASS pid={} user_sid={} logon_sid={} version={} statuses={} provider={}\n",
        std::process::id(),
        current_user_sid()?,
        current_logon_sid()?,
        version,
        statuses.len(),
        statuses
            .first()
            .map(|status| status.provider.as_str())
            .unwrap_or("none")
    );

    if let Some(path) = receipt {
        fs::write(path, &report)?;
    } else {
        print!("{report}");
        io::stdout().flush()?;
    }

    if await_close {
        println!("CONNECTED pid={}", std::process::id());
        io::stdout().flush()?;
        bounded("server-side connection close", client.connection().closed()).await?;
        println!("CLOSED pid={}", std::process::id());
        io::stdout().flush()?;
    }
    Ok(())
}

#[cfg(windows)]
fn current_logon_sid() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("whoami.exe")
        .arg("/logonid")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "whoami.exe /logonid failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    if !value.starts_with("S-1-5-5-") {
        return Err(format!("whoami.exe returned malformed logon SID {value:?}").into());
    }
    Ok(value)
}

#[cfg(unix)]
fn current_logon_sid() -> Result<String, Box<dyn std::error::Error>> {
    current_user_sid().map_err(Into::into)
}
