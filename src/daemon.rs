//! Finding, starting and talking to the background daemon from other commands.

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Serialize, Deserialize, Clone)]
pub struct Daemon {
    pub port: u16,
    pub token: String,
}

fn info_path() -> Result<PathBuf> {
    // A virtual iPhone gets a daemon of its own, beside the real phone's.
    let name = if crate::vm::target().is_some() { "daemon-vm.json" } else { "daemon.json" };
    Ok(crate::wifi::state_dir()?.join(name))
}

pub fn write_info(port: u16, token: &str) -> Result<()> {
    let info = Daemon { port, token: token.into() };
    std::fs::write(info_path()?, serde_json::to_vec(&info)?)?;
    Ok(())
}

impl Daemon {
    /// A minimal HTTP/1.1 request; returns the status code and body.
    pub async fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-iLinkOS-Token: {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.token,
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("malformed response from the daemon")?;
        let status = std::str::from_utf8(&response[..split])?
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .context("malformed status line from the daemon")?;
        Ok((status, response[split + 4..].to_vec()))
    }

    async fn alive(&self) -> bool {
        matches!(self.request("GET", "/status", b"").await, Ok((200, _)))
    }
}

/// The daemon that is already up, whether headless or inside the app.
pub async fn running() -> Option<Daemon> {
    let info: Daemon = serde_json::from_slice(&std::fs::read(info_path().ok()?).ok()?).ok()?;
    info.alive().await.then_some(info)
}

/// Returns the running daemon, starting one in the background if there is none.
pub async fn ensure(usb: bool) -> Result<Daemon> {
    if let Some(daemon) = running().await {
        return Ok(daemon);
    }
    let log = std::fs::File::create(crate::wifi::state_dir()?.join("daemon.log"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("serve");
    if usb {
        command.arg("--usb");
    }
    command.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        // Otherwise the daemon inherits our stdio pipes and whoever launched us
        // (an MCP client, a shell) never sees them close.
        unsafe extern "system" {
            fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 1;
        let handles = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        for handle in handles {
            // SAFETY: these are this process's own live standard handles.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().context("could not start the iLinkOS daemon")?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(daemon) = running().await {
            return Ok(daemon);
        }
    }
    bail!("the iLinkOS daemon did not start — see daemon.log in the iLinkOS data folder")
}
