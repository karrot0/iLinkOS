//! iLinkOS — drive an iPhone from a PC over Apple's CoreDevice channel.

pub mod ax;
pub mod control;
pub mod daemon;
pub mod device;
pub mod mcp;
pub mod record;
pub mod serve;
pub mod tools;
pub mod vm;
pub mod wifi;

use std::{future::Future, path::PathBuf};

/// The ffmpeg to run: the copy shipped beside this program, or else whichever is on PATH.
pub fn ffmpeg() -> PathBuf {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(name))
        .filter(|bundled| bundled.is_file())
        .unwrap_or_else(|| "ffmpeg".into())
}

/// Runs the future `make` returns to completion on a runtime whose threads have deep stacks: the
/// tunnel and pairing futures overflow Windows' 1 MB default.
pub fn block_on_deep<T, F>(make: impl FnOnce() -> F + Send + 'static) -> anyhow::Result<T>
where
    T: Send + 'static,
    // Made on the new thread, because the pairing futures are not `Send`.
    F: Future<Output = anyhow::Result<T>>,
{
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(16 * 1024 * 1024)
                .build()?
                .block_on(make())
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("main task panicked"))?
}
