use tokio::signal;

#[derive(Debug, thiserror::Error)]
pub enum ShutdownSignalError {
    #[error("failed to install Ctrl+C handler: {0}")]
    CtrlC(std::io::Error),
    #[cfg(unix)]
    #[error("failed to install SIGTERM handler: {0}")]
    Sigterm(std::io::Error),
}

pub async fn shutdown_signal() -> Result<(), ShutdownSignalError> {
    let ctrl_c = async { signal::ctrl_c().await.map_err(ShutdownSignalError::CtrlC) };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .map_err(ShutdownSignalError::Sigterm)?;
        let _ = sigterm.recv().await;
        Ok(())
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<Result<(), ShutdownSignalError>>();

    tokio::select! {
        result = ctrl_c => result,
        result = terminate => result,
    }
}
