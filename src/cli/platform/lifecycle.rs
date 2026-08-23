#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};

pub(super) fn spawn_suspend_watcher() -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let Ok(mut signals) = signal(SignalKind::from_raw(libc::SIGTSTP)) else {
            return;
        };
        while signals.recv().await.is_some() {
            if sender.send(()).is_err() {
                break;
            }
        }
    });
    #[cfg(not(unix))]
    drop(sender);
    receiver
}

#[cfg(unix)]
pub(super) fn suspend_process() -> Result<()> {
    let result = unsafe { libc::raise(libc::SIGSTOP) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("could not suspend platform cockpit")
    }
}

#[cfg(not(unix))]
pub(super) fn suspend_process() -> Result<()> {
    bail!("terminal suspend is unavailable on this platform")
}
