//! Certificate issuance with ACME HTTP-01.
//!
//! The account key and every certificate key are written to the node's
//! [`SecretStore`]; what is returned for control state is public (validity
//! and fingerprint) plus a secret reference.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use serde::{Deserialize, Serialize};

use crate::secrets::SecretStore;

/// Where certificates come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// The ACME directory URL, such as Let's Encrypt's.
    pub directory: String,
    /// `mailto:` contact for the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    /// A private CA that signs the ACME server's own TLS certificate (for
    /// a test CA such as Pebble). Public roots are used otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// Renew this many days before expiry.
    #[serde(default = "AcmeConfig::default_renew_before_days")]
    pub renew_before_days: u32,
}

impl AcmeConfig {
    fn default_renew_before_days() -> u32 {
        30
    }

    pub fn lets_encrypt(contact: Option<String>) -> Self {
        Self {
            directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            contact,
            ca_file: None,
            renew_before_days: Self::default_renew_before_days(),
        }
    }
}

/// Serves HTTP-01 key authorizations while a challenge is outstanding.
pub trait ChallengeResponder: Send + Sync {
    fn present(&self, token: &str, key_authorization: &str);
    fn clear(&self, token: &str);
}

/// An issued certificate. The key is in the secret store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub fingerprint: String,
    /// The reference to the chain and key in the node's secret store.
    pub secret_reference: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AcmeError(pub String);

impl From<instant_acme::Error> for AcmeError {
    fn from(error: instant_acme::Error) -> Self {
        Self(error.to_string())
    }
}

/// A certificate as the secret store holds it: the chain and the key.
#[derive(Serialize, Deserialize)]
pub struct StoredCertificate {
    pub chain_pem: String,
    pub key_pem: String,
}

impl StoredCertificate {
    pub fn load(secrets: &SecretStore, reference: &str) -> Option<Self> {
        secrets
            .get(reference)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }
}

fn account_secret(config: &AcmeConfig) -> String {
    format!(
        "acme/account-{}.json",
        &crate::tls::fingerprint(config.directory.as_bytes())["sha256:".len()..][..16]
    )
}

async fn account(config: &AcmeConfig, secrets: &SecretStore) -> Result<Account, AcmeError> {
    let builder = || match &config.ca_file {
        Some(ca_file) => Account::builder_with_root(ca_file),
        None => Account::builder(),
    };
    let name = account_secret(config);
    if let Some(bytes) = secrets
        .get_named(&name)
        .map_err(|error| AcmeError(error.to_string()))?
        && let Ok(credentials) = serde_json::from_slice::<AccountCredentials>(&bytes)
        && let Ok(account) = builder()?.from_credentials(credentials).await
    {
        return Ok(account);
    }
    let contact = config
        .contact
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let (account, credentials) = builder()?
        .create(
            &NewAccount {
                contact: &contact,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            config.directory.clone(),
            None,
        )
        .await?;
    let bytes = serde_json::to_vec(&credentials).map_err(|error| AcmeError(error.to_string()))?;
    secrets
        .put(&name, &bytes)
        .map_err(|error| AcmeError(error.to_string()))?;
    Ok(account)
}

/// Issue a certificate for `domain`, answering HTTP-01 through `responder`.
pub async fn issue(
    config: &AcmeConfig,
    secrets: &SecretStore,
    domain: &str,
    responder: &dyn ChallengeResponder,
) -> Result<Issued, AcmeError> {
    let account = account(config, secrets).await?;
    let identifiers = [Identifier::Dns(domain.to_string())];
    let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
    let mut tokens = vec![];
    let result = async {
        let mut authorizations = order.authorizations();
        while let Some(authorization) = authorizations.next().await {
            let mut authorization = authorization?;
            match authorization.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => {
                    return Err(AcmeError(format!(
                        "authorization for {domain} is {other:?}"
                    )));
                }
            }
            let mut challenge = authorization
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| AcmeError(format!("no http-01 challenge offered for {domain}")))?;
            responder.present(&challenge.token, challenge.key_authorization().as_str());
            tokens.push(challenge.token.clone());
            challenge.set_ready().await?;
        }
        let retries = RetryPolicy::new()
            .initial_delay(Duration::from_millis(250))
            .timeout(Duration::from_secs(90));
        let status = order.poll_ready(&retries).await?;
        if status != OrderStatus::Ready {
            return Err(AcmeError(format!("the order for {domain} is {status:?}")));
        }
        let key_pem = order.finalize().await?;
        let chain_pem = order.poll_certificate(&retries).await?;
        Ok((chain_pem, key_pem))
    }
    .await;
    for token in &tokens {
        responder.clear(token);
    }
    let (chain_pem, key_pem) = result?;
    let info = crate::tls::info(chain_pem.as_bytes()).map_err(|error| AcmeError(error.0))?;
    let stored = serde_json::to_vec(&StoredCertificate { chain_pem, key_pem })
        .map_err(|error| AcmeError(error.to_string()))?;
    let name = format!(
        "tls/{domain}/{}.json",
        info.fingerprint.trim_start_matches("sha256:")
    );
    let secret_reference = secrets
        .put(&name, &stored)
        .map_err(|error| AcmeError(error.to_string()))?;
    Ok(Issued {
        not_before: info.not_before,
        not_after: info.not_after,
        fingerprint: info.fingerprint,
        secret_reference,
    })
}
