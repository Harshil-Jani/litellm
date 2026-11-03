use std::{ffi::OsString, time::Duration};

use serde::Deserialize;

use crate::Error;

const UPSTREAM: &str = "LITELLM_FORWARD_UPSTREAM";

#[derive(Deserialize)]
struct Environment {
    litellm_forward_upstream: String,
    litellm_forward_host: String,
    litellm_forward_port: u16,
    litellm_forward_upstream_keepalive: f64,
    litellm_forward_shutdown_drain: f64,
}

/// Everything forward mode runs with. The Python supervisor resolves each value from the settings
/// Python actually runs with, so none of them is guessed or defaulted here.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub upstream: String,
    pub host: String,
    pub port: u16,
    pub upstream_keepalive: Duration,
    pub shutdown_drain: Duration,
}

impl Settings {
    /// `None` when `LITELLM_FORWARD_UPSTREAM` is unset, which means the gateway is not forwarding.
    pub fn from_env() -> Result<Option<Self>, Error> {
        Self::from_vars(std::env::vars_os())
    }

    pub fn from_vars(
        variables: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Option<Self>, Error> {
        let variables: Vec<(String, String)> = variables
            .into_iter()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        if !variables.iter().any(|(key, _)| key == UPSTREAM) {
            return Ok(None);
        }
        let environment: Environment = envy::from_iter(variables).map_err(|error| match error {
            envy::Error::MissingValue(field) => Error::MissingSetting(field.to_uppercase()),
            envy::Error::Custom(message) => Error::InvalidSetting(message),
        })?;
        Ok(Some(Self {
            upstream: environment.litellm_forward_upstream,
            host: environment.litellm_forward_host,
            port: environment.litellm_forward_port,
            upstream_keepalive: seconds(
                "LITELLM_FORWARD_UPSTREAM_KEEPALIVE",
                environment.litellm_forward_upstream_keepalive,
            )?,
            shutdown_drain: seconds(
                "LITELLM_FORWARD_SHUTDOWN_DRAIN",
                environment.litellm_forward_shutdown_drain,
            )?,
        }))
    }
}

fn seconds(variable: &str, value: f64) -> Result<Duration, Error> {
    Duration::try_from_secs_f64(value).map_err(|_| {
        Error::InvalidSetting(format!(
            "{variable} must be a non-negative number of seconds, got {value}"
        ))
    })
}
