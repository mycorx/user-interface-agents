// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::engine::EngineId;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub enum Credentials {
    ApiKey(String),
    /// Nova resolves real credentials through the AWS chain; the region is the
    /// part we must supply explicitly, because the model does not exist in the
    /// account's home region.
    Aws {
        region: String,
    },
    /// Foundry has no fixed endpoint like OpenAI or a region-derived one like
    /// Nova — each customer's Azure resource is a distinct URL, so it must
    /// travel with the key rather than being a compile-time constant.
    AzureFoundry {
        endpoint: String,
        api_key: String,
    },
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CredError {
    #[error("no credentials configured for {0:?}")]
    Missing(EngineId),
    #[error("credential store error: {0}")]
    Store(String),
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn credentials_for(&self, engine: EngineId) -> Result<Credentials, CredError>;
}

/// Test and bootstrap adapter. SP2 replaces this with the encrypted store.
pub struct StaticCredentials {
    pub openai_api_key: Option<String>,
    pub nova_region: Option<String>,
    pub foundry_endpoint: Option<String>,
    pub foundry_api_key: Option<String>,
}

#[async_trait]
impl CredentialProvider for StaticCredentials {
    async fn credentials_for(&self, engine: EngineId) -> Result<Credentials, CredError> {
        match engine {
            EngineId::OpenAi => self
                .openai_api_key
                .clone()
                .map(Credentials::ApiKey)
                .ok_or(CredError::Missing(engine)),
            EngineId::NovaSonic => self
                .nova_region
                .clone()
                .map(|region| Credentials::Aws { region })
                .ok_or(CredError::Missing(engine)),
            EngineId::Foundry => match (&self.foundry_endpoint, &self.foundry_api_key) {
                (Some(endpoint), Some(api_key)) => Ok(Credentials::AzureFoundry {
                    endpoint: endpoint.clone(),
                    api_key: api_key.clone(),
                }),
                _ => Err(CredError::Missing(engine)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn foundry_credentials_require_both_endpoint_and_key() {
        let store = StaticCredentials {
            openai_api_key: None,
            nova_region: None,
            foundry_endpoint: Some("https://my-resource.services.ai.azure.com".into()),
            foundry_api_key: Some("secret".into()),
        };

        let creds = store
            .credentials_for(EngineId::Foundry)
            .await
            .expect("both fields present must resolve");

        assert!(matches!(
            creds,
            Credentials::AzureFoundry { endpoint, api_key }
                if endpoint == "https://my-resource.services.ai.azure.com" && api_key == "secret"
        ));
    }

    #[tokio::test]
    async fn foundry_credentials_missing_endpoint_is_an_error() {
        let store = StaticCredentials {
            openai_api_key: None,
            nova_region: None,
            foundry_endpoint: None,
            foundry_api_key: Some("secret".into()),
        };

        let err = store
            .credentials_for(EngineId::Foundry)
            .await
            .expect_err("endpoint missing must not silently resolve");
        assert!(matches!(err, CredError::Missing(EngineId::Foundry)));
    }
}
