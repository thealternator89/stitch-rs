use github_copilot_sdk::{Client, ClientInfo, ClientOptions};
use tokio::sync::Mutex;

use crate::types::{CopilotAuthInfo, CopilotModelInfo};

struct ManagedClient {
    token: Option<String>,
    client: Client,
}

/// Manages the Copilot SDK client lifecycle, auth checks, and cached model discovery.
pub struct CopilotService {
    client_slot: Mutex<Option<ManagedClient>>,
    cached_models: Mutex<Option<(Option<String>, Vec<CopilotModelInfo>)>>,
}

impl CopilotService {
    pub fn new() -> Self {
        Self {
            client_slot: Mutex::new(None),
            cached_models: Mutex::new(None),
        }
    }

    fn normalize_token(token: Option<String>) -> Option<String> {
        token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    fn build_client_options(token: Option<&str>) -> ClientOptions {
        let mut options = ClientOptions::new().with_client_info(
            ClientInfo::new()
                .with_application_name("Stitch")
                .with_application_version(env!("CARGO_PKG_VERSION")),
        );

        if let Some(tok) = token {
            options = options.with_github_token(tok).with_use_logged_in_user(false);
        }

        options
    }

    async fn ensure_client_locked<'a>(
        guard: &'a mut Option<ManagedClient>,
        token: &Option<String>,
    ) -> anyhow::Result<&'a Client> {
        let token_changed = guard
            .as_ref()
            .is_some_and(|existing| &existing.token != token);

        if token_changed {
            if let Some(old) = guard.take() {
                let _ = old.client.stop().await;
            }
        }

        if guard.is_none() {
            let options = Self::build_client_options(token.as_deref());
            let client = Client::start(options).await.map_err(|err| {
                anyhow::anyhow!(
                    "Failed to start GitHub Copilot runtime: {err}. Ensure the Copilot CLI is available or configured."
                )
            })?;
            *guard = Some(ManagedClient {
                token: token.clone(),
                client,
            });
        }

        Ok(&guard.as_ref().expect("client initialized").client)
    }

    /// Verifies authentication state and CLI runtime metadata via `get_auth_status()` and `get_status()`.
    pub async fn check_auth_status(&self, token: Option<String>) -> anyhow::Result<CopilotAuthInfo> {
        let normalized_token = Self::normalize_token(token);
        let mut guard = self.client_slot.lock().await;

        let client = Self::ensure_client_locked(&mut guard, &normalized_token).await?;

        let auth_res = client.get_auth_status().await;
        let auth = match auth_res {
            Ok(auth) => auth,
            Err(err) => {
                // Reset the client so a subsequent check spawns a clean process.
                if let Some(stale) = guard.take() {
                    let _ = stale.client.stop().await;
                }
                return Err(anyhow::anyhow!("Copilot auth check failed: {err}"));
            }
        };

        let status = client.get_status().await.ok();

        Ok(CopilotAuthInfo {
            is_authenticated: auth.is_authenticated,
            login: auth.login,
            auth_type: auth.auth_type,
            host: auth.host,
            status_message: auth.status_message,
            cli_version: status.as_ref().map(|s| s.version.clone()),
            protocol_version: status.as_ref().map(|s| s.protocol_version.to_string()),
        })
    }

    /// Lists available Copilot models, caching results per token unless `force_refresh` is true.
    pub async fn list_models(
        &self,
        token: Option<String>,
        force_refresh: bool,
    ) -> anyhow::Result<Vec<CopilotModelInfo>> {
        let normalized_token = Self::normalize_token(token);

        if !force_refresh {
            let cache = self.cached_models.lock().await;
            if let Some((cached_token, models)) = cache.as_ref() {
                if cached_token == &normalized_token {
                    return Ok(models.clone());
                }
            }
        }

        let mut guard = self.client_slot.lock().await;
        let client = Self::ensure_client_locked(&mut guard, &normalized_token).await?;

        let raw_models = if force_refresh {
            client.rpc().models().list().await.map(|res| res.models)
        } else {
            client.list_models().await
        };

        let raw_models = match raw_models {
            Ok(models) => models,
            Err(err) => {
                if let Some(stale) = guard.take() {
                    let _ = stale.client.stop().await;
                }
                return Err(anyhow::anyhow!("Failed to list Copilot models: {err}"));
            }
        };

        let models: Vec<CopilotModelInfo> = raw_models
            .into_iter()
            .map(|m| CopilotModelInfo {
                id: m.id,
                name: m.name,
                billing_multiplier: m.billing.and_then(|b| b.multiplier),
            })
            .collect();

        let mut cache = self.cached_models.lock().await;
        *cache = Some((normalized_token, models.clone()));

        Ok(models)
    }

    /// Clears cached model listings and stops any active client so future calls reload fresh state.
    pub async fn invalidate_cache(&self) {
        {
            let mut cache = self.cached_models.lock().await;
            *cache = None;
        }
        let mut guard = self.client_slot.lock().await;
        if let Some(existing) = guard.take() {
            let _ = existing.client.stop().await;
        }
    }

    /// Gracefully stops the underlying Copilot SDK client if running.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        self.invalidate_cache().await;
    }
}
