mod error;
mod forward;
mod hop;
mod settings;
mod websocket;

pub use error::Error;
pub use forward::{Forward, PEER_HEADER};
pub use settings::Settings;

/// Binds the public listener and forwards to Python until `shutdown` resolves, then drains for
/// `settings.shutdown_drain`.
pub async fn run(
    settings: Settings,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    let forward = Forward::new(&settings.upstream, settings.upstream_keepalive)?;
    let listener = tokio::net::TcpListener::bind((settings.host.as_str(), settings.port)).await?;
    tracing::info!(address = %listener.local_addr()?, "forwarding gateway listening");
    forward
        .serve(listener, shutdown, settings.shutdown_drain)
        .await?;
    Ok(())
}
