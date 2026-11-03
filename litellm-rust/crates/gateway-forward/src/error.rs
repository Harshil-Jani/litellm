#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid forwarding upstream URI")]
    UpstreamUri(#[source] axum::http::uri::InvalidUri),
    #[error(
        "LITELLM_FORWARD_UPSTREAM must be a loopback IPv4 http origin without a path, query or credentials"
    )]
    Upstream,
    #[error("{0} must be set in forward mode")]
    MissingSetting(String),
    #[error("invalid forwarding setting: {0}")]
    InvalidSetting(String),
    #[error("forwarding gateway I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
