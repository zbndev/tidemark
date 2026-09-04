use std::io::{self, BufRead as _, Write as _};
use std::path::PathBuf;

use tidemark_ipc_p2p_spike::{RunningServer, pending_status};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut endpoint = None;
    let mut version = "ipc-spike".to_owned();
    let mut provider = "zai".to_owned();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--socket") => endpoint = args.next().map(PathBuf::from),
            Some("--version") => {
                version = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or("--version requires UTF-8 text")?;
            }
            Some("--provider") => {
                provider = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or("--provider requires UTF-8 text")?;
            }
            Some(other) => return Err(format!("unknown argument {other:?}").into()),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    let endpoint = endpoint.ok_or("--socket is required")?;
    let server = RunningServer::start(
        endpoint.clone(),
        version.clone(),
        vec![pending_status(&provider)],
    )?;

    println!(
        "READY pid={} socket={} version={version}",
        std::process::id(),
        endpoint.display()
    );
    io::stdout().flush()?;

    let command = tokio::task::spawn_blocking(|| {
        let mut command = String::new();
        io::stdin().lock().read_line(&mut command).map(|_| command)
    })
    .await??;
    if command.trim() != "stop" {
        return Err(format!("expected `stop` on stdin, got {command:?}").into());
    }
    server.shutdown().await?;
    println!("STOPPED pid={}", std::process::id());
    io::stdout().flush()?;
    Ok(())
}
