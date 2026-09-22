//! docker login and logout: credentials for a registry, checked against it and kept for
//! pull, push and search.

use anyhow::{bail, Result};

use crate::api::{require_root, Context};
use crate::auth::{self, Credentials};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedIn {
    pub registry: String,
    pub username: String,
    /// Whether the registry asked for credentials at all; when it did not, they are
    /// kept anyway.
    pub asked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedOut {
    pub registry: String,
    /// False when nothing was stored for the registry.
    pub removed: bool,
}

/// `registry`: the hub when None.
pub async fn login(
    ctx: &Context,
    registry: Option<String>,
    credentials: Credentials,
) -> Result<LoggedIn> {
    require_root("login")?;
    let config = &ctx.config;
    let registry = registry.unwrap_or_else(|| config.registry.clone());
    if credentials.username.is_empty() || credentials.password.is_empty() {
        bail!("a username and a password are needed");
    }
    // The CA certificate applies to the hub only.
    let ca_cert = (auth::canonical(&registry) == auth::canonical(&config.registry))
        .then_some(config.ca_cert.as_deref())
        .flatten();
    let asked = auth::verify(&registry, &credentials, ca_cert).await?;
    auth::store(&registry, &credentials)?;
    Ok(LoggedIn {
        registry,
        username: credentials.username,
        asked,
    })
}

pub fn logout(ctx: &Context, registry: Option<String>) -> Result<LoggedOut> {
    require_root("logout")?;
    let registry = registry.unwrap_or_else(|| ctx.config.registry.clone());
    let removed = auth::forget(&registry)?;
    Ok(LoggedOut { registry, removed })
}
