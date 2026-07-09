use std::fmt;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ShutdownSignal {
    CtrlC,
    Terminate,
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CtrlC => f.write_str("Ctrl+C/SIGINT"),
            Self::Terminate => f.write_str("SIGTERM"),
        }
    }
}

pub async fn wait_for_shutdown_signal() -> Result<ShutdownSignal, String> {
    platform::wait_for_shutdown_signal().await
}

#[cfg(unix)]
mod platform {
    use super::ShutdownSignal;
    use tokio::signal::unix::{signal, SignalKind};

    pub(super) async fn wait_for_shutdown_signal() -> Result<ShutdownSignal, String> {
        let mut sigterm =
            signal(SignalKind::terminate()).map_err(|e| format!("listen SIGTERM failed: {e}"))?;

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result
                    .map(|_| ShutdownSignal::CtrlC)
                    .map_err(|e| format!("listen Ctrl+C/SIGINT failed: {e}"))
            }
            _ = sigterm.recv() => Ok(ShutdownSignal::Terminate),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::ShutdownSignal;

    pub(super) async fn wait_for_shutdown_signal() -> Result<ShutdownSignal, String> {
        tokio::signal::ctrl_c()
            .await
            .map(|_| ShutdownSignal::CtrlC)
            .map_err(|e| format!("listen Ctrl+C/SIGINT failed: {e}"))
    }
}

#[cfg(all(not(unix), not(windows)))]
mod platform {
    use super::ShutdownSignal;

    pub(super) async fn wait_for_shutdown_signal() -> Result<ShutdownSignal, String> {
        tokio::signal::ctrl_c()
            .await
            .map(|_| ShutdownSignal::CtrlC)
            .map_err(|e| format!("listen Ctrl+C/SIGINT failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::ShutdownSignal;

    #[test]
    fn shutdown_signal_display_names_are_stable() {
        assert_eq!(ShutdownSignal::CtrlC.to_string(), "Ctrl+C/SIGINT");
        assert_eq!(ShutdownSignal::Terminate.to_string(), "SIGTERM");
    }
}
