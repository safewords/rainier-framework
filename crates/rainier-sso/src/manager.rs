//! Named providers — [`Sso`], and the [`SsoConfig`] section that declares
//! them.
//!
//! ```ignore
//! // config: services.sso
//! // {
//! //   "github":  { "client_id": "…", "client_secret": "…", "redirect": "https://app/auth/github/callback" },
//! //   "work":    { "driver": "oidc", "issuer": "https://login.example.com", … },
//! //   "azure":   { "driver": "microsoft", "tenant": "contoso.onmicrosoft.com", … }
//! // }
//! let sso = Sso::from_config(&config, http)?;
//! sso.driver("github")?.redirect(&request).await
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use rainier_http_client::Transport;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::api::Api;
use crate::provider::{ClientAuth, ClientCredentials, OAuth2Provider, Provider};
use crate::providers::{self, GitHub, GitLab};

/// One provider's declaration.
///
/// `Debug` prints no secret.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// `github`, `gitlab`, `discord`, `x`, `google`, `microsoft` or `oidc`.
    /// Defaults to the entry's own name, so a `github` entry needs none.
    #[serde(default)]
    pub driver: Option<String>,
    /// The client id.
    pub client_id: String,
    /// The client secret.
    pub client_secret: String,
    /// The callback URL, as registered.
    #[serde(alias = "redirect_uri")]
    pub redirect: String,
    /// Scopes to request instead of the driver's defaults.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Extra authorize-URL parameters.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// For `oidc`: the issuer to discover.
    #[serde(default)]
    pub issuer: Option<String>,
    /// For `microsoft`: the tenant. `common` when absent.
    #[serde(default)]
    pub tenant: Option<String>,
    /// For `gitlab` and `github`: a self-hosted instance's base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Authenticate to the token endpoint with HTTP Basic instead of form
    /// fields.
    #[serde(default)]
    pub basic_auth: bool,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("driver", &self.driver)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("redirect", &self.redirect)
            .field("issuer", &self.issuer)
            .field("tenant", &self.tenant)
            .finish()
    }
}

/// Every provider an application declares, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SsoConfig {
    /// The declarations.
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// The drivers [`Sso::from_config`] knows.
pub const DRIVERS: &[&str] = &["github", "gitlab", "discord", "x", "google", "microsoft", "oidc"];

/// Build one provider from its declaration.
pub fn build(name: &str, config: &ProviderConfig, api: Api) -> Result<OAuth2Provider> {
    let credentials =
        ClientCredentials::new(&config.client_id, &config.client_secret, &config.redirect);
    let driver = config.driver.as_deref().unwrap_or(name);

    let provider = match driver {
        "github" => match &config.base_url {
            Some(base) => OAuth2Provider::new(name, credentials, GitHub::enterprise(base), api)
                .scopes(["read:user", "user:email"]),
            None => providers::github(credentials, api),
        },
        "gitlab" => match &config.base_url {
            Some(base) => {
                OAuth2Provider::new(name, credentials, GitLab::at(base), api).scopes(["read_user"])
            }
            None => providers::gitlab(credentials, api),
        },
        "discord" => providers::discord(credentials, api),
        "x" | "twitter" => providers::x(credentials, api),
        "google" => providers::google(credentials, api),
        "microsoft" => {
            providers::microsoft(config.tenant.as_deref().unwrap_or("common"), credentials, api)
        }
        "oidc" => {
            let issuer = config.issuer.as_deref().ok_or_else(|| {
                Error::internal(format!(
                    "the `{name}` sign-in provider is OIDC and names no `issuer`"
                ))
            })?;
            providers::oidc(name, issuer, credentials, api)
        }
        other => {
            return Err(Error::internal(format!(
                "the `{name}` sign-in provider names the driver `{other}`; the drivers are {}",
                DRIVERS.join(", ")
            )))
        }
    };

    // A built-in registered under another name — `work` for an `oidc` — is
    // still known by the name it was declared under.
    let mut provider = provider.renamed(name);
    if let Some(scopes) = &config.scopes {
        provider = provider.scopes(scopes.iter().cloned());
    }
    for (key, value) in &config.parameters {
        provider = provider.with_parameter(key, value);
    }
    if config.basic_auth {
        provider = provider.client_auth(ClientAuth::Basic);
    }
    Ok(provider)
}

/// Providers by name.
#[derive(Default, Clone)]
pub struct Sso {
    providers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Sso {
    /// No providers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every provider `config` declares, calling out through `http`.
    pub fn from_config(config: &SsoConfig, http: Arc<dyn Transport>) -> Result<Self> {
        let mut sso = Self::new();
        for (name, declaration) in &config.providers {
            let provider = build(name, declaration, Api::new(Arc::clone(&http)))?;
            sso = sso.register(Arc::new(provider));
        }
        Ok(sso)
    }

    /// Add a provider, under its own name.
    #[must_use]
    pub fn register(mut self, provider: Arc<dyn Provider>) -> Self {
        self.providers.insert(provider.name().to_string(), provider);
        self
    }

    /// The provider named `name`.
    ///
    /// # Errors
    ///
    /// A `404` naming the ones that exist — `name` is usually a route
    /// parameter, so an unknown one is a bad URL rather than a server fault.
    pub fn driver(&self, name: &str) -> Result<Arc<dyn Provider>> {
        self.providers.get(name).cloned().ok_or_else(|| {
            Error::not_found(format!(
                "there is no `{name}` sign-in provider; there is {}",
                self.names().join(", ")
            ))
        })
    }

    /// Whether `name` is registered.
    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// Every registered name, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }
}

impl std::fmt::Debug for Sso {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sso").field("providers", &self.names()).finish()
    }
}
