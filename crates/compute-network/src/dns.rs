//! DNS providers.
//!
//! Compute is not a DNS server. It asks the provider that holds a zone to
//! keep a record at a value, reads the record back to detect drift, and
//! removes it when the domain goes. Credentials come from environment
//! variables named in configuration; they are never written to control
//! state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A record as a provider holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsValue {
    pub value: String,
    pub ttl: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("DNS provider credentials are missing: set {0}")]
    MissingCredentials(String),
    #[error("{0}")]
    Invalid(String),
    #[error("DNS provider request failed: {0}")]
    Provider(String),
}

impl From<reqwest::Error> for DnsError {
    fn from(error: reqwest::Error) -> Self {
        Self::Provider(error.to_string())
    }
}

#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// `hetzner`, `cloudflare`, or `file`.
    fn kind(&self) -> &'static str;

    /// The zone this provider manages, such as `example.com`.
    fn zone(&self) -> &str;

    /// The values `name` (relative to the zone, `@` for the apex) holds
    /// for `record_type`.
    async fn lookup(&self, name: &str, record_type: &str) -> Result<Vec<DnsValue>, DnsError>;

    /// Make `name` hold exactly `value` for `record_type`.
    async fn apply(
        &self,
        name: &str,
        record_type: &str,
        value: &str,
        ttl: u32,
    ) -> Result<DnsValue, DnsError>;

    /// Remove `name`'s `record_type` records.
    async fn remove(&self, name: &str, record_type: &str) -> Result<(), DnsError>;
}

/// A configured provider: `[network.dns.<name>]` in compute.toml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DnsProviderConfig {
    /// Hetzner DNS through the Hetzner Cloud API.
    Hetzner {
        zone: String,
        /// The environment variable holding the API token.
        token_env: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
    },
    Cloudflare {
        zone: String,
        token_env: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
    },
    /// Records kept in a local JSON file: for development, tests, and
    /// zones managed by hand.
    File { zone: String, path: PathBuf },
}

impl DnsProviderConfig {
    pub fn zone(&self) -> &str {
        match self {
            Self::Hetzner { zone, .. }
            | Self::Cloudflare { zone, .. }
            | Self::File { zone, .. } => zone,
        }
    }

    /// Build the provider. Reads credentials from the environment now, so a
    /// missing token is reported before anything is attempted.
    pub fn build(&self) -> Result<Box<dyn DnsProvider>, DnsError> {
        let token = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|token| !token.is_empty())
                .ok_or_else(|| DnsError::MissingCredentials(name.into()))
        };
        Ok(match self {
            Self::Hetzner {
                zone,
                token_env,
                api_url,
            } => Box::new(Hetzner {
                zone: zone.clone(),
                token: token(token_env)?,
                api: api_url
                    .clone()
                    .unwrap_or_else(|| "https://api.hetzner.cloud/v1".into()),
                http: client()?,
            }),
            Self::Cloudflare {
                zone,
                token_env,
                api_url,
            } => Box::new(Cloudflare {
                zone: zone.clone(),
                token: token(token_env)?,
                api: api_url
                    .clone()
                    .unwrap_or_else(|| "https://api.cloudflare.com/client/v4".into()),
                http: client()?,
                zone_id: tokio::sync::OnceCell::new(),
            }),
            Self::File { zone, path } => Box::new(FileDns {
                zone: zone.clone(),
                path: path.clone(),
            }),
        })
    }
}

fn client() -> Result<reqwest::Client, DnsError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(DnsError::from)
}

/// `www.example.com` in `example.com` → `www`; the apex → `@`.
pub fn relative_name(domain: &str, zone: &str) -> Result<String, DnsError> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let zone = zone.trim_end_matches('.').to_ascii_lowercase();
    if domain == zone {
        return Ok("@".into());
    }
    domain
        .strip_suffix(&format!(".{zone}"))
        .map(str::to_string)
        .ok_or_else(|| DnsError::Invalid(format!("{domain} is not in the zone {zone}")))
}

async fn checked(response: reqwest::Response) -> Result<Value, DnsError> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(DnsError::Provider(format!(
            "{status}: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body).map_err(|error| DnsError::Provider(error.to_string()))
}

// ---- Hetzner --------------------------------------------------------------

/// Hetzner DNS through the Hetzner Cloud API's RRSets.
struct Hetzner {
    zone: String,
    token: String,
    api: String,
    http: reqwest::Client,
}

impl Hetzner {
    fn rrset_url(&self, name: &str, record_type: &str) -> String {
        format!(
            "{}/zones/{}/rrsets/{name}/{record_type}",
            self.api, self.zone
        )
    }
}

#[async_trait]
impl DnsProvider for Hetzner {
    fn kind(&self) -> &'static str {
        "hetzner"
    }

    fn zone(&self) -> &str {
        &self.zone
    }

    async fn lookup(&self, name: &str, record_type: &str) -> Result<Vec<DnsValue>, DnsError> {
        let response = self
            .http
            .get(self.rrset_url(name, record_type))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        let body = checked(response).await?;
        let rrset = &body["rrset"];
        let ttl = rrset["ttl"].as_u64().unwrap_or(0) as u32;
        let id = rrset["id"].as_str().map(str::to_string);
        Ok(rrset["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|record| record["value"].as_str())
            .map(|value| DnsValue {
                value: value.to_string(),
                ttl,
                id: id.clone(),
            })
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        record_type: &str,
        value: &str,
        ttl: u32,
    ) -> Result<DnsValue, DnsError> {
        let existing = self.lookup(name, record_type).await?;
        let records = json!([{ "value": value }]);
        if existing.is_empty() {
            let body = checked(
                self.http
                    .post(format!("{}/zones/{}/rrsets", self.api, self.zone))
                    .bearer_auth(&self.token)
                    .json(&json!({
                        "name": name,
                        "type": record_type,
                        "ttl": ttl,
                        "records": records,
                    }))
                    .send()
                    .await?,
            )
            .await?;
            return Ok(DnsValue {
                value: value.into(),
                ttl,
                id: body["rrset"]["id"].as_str().map(str::to_string),
            });
        }
        let url = self.rrset_url(name, record_type);
        checked(
            self.http
                .post(format!("{url}/actions/set_records"))
                .bearer_auth(&self.token)
                .json(&json!({ "records": records }))
                .send()
                .await?,
        )
        .await?;
        if existing[0].ttl != ttl {
            checked(
                self.http
                    .post(format!("{url}/actions/change_ttl"))
                    .bearer_auth(&self.token)
                    .json(&json!({ "ttl": ttl }))
                    .send()
                    .await?,
            )
            .await?;
        }
        Ok(DnsValue {
            value: value.into(),
            ttl,
            id: existing[0].id.clone(),
        })
    }

    async fn remove(&self, name: &str, record_type: &str) -> Result<(), DnsError> {
        let response = self
            .http
            .delete(self.rrset_url(name, record_type))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        checked(response).await.map(|_| ())
    }
}

// ---- Cloudflare -----------------------------------------------------------

struct Cloudflare {
    zone: String,
    token: String,
    api: String,
    http: reqwest::Client,
    zone_id: tokio::sync::OnceCell<String>,
}

impl Cloudflare {
    async fn zone_id(&self) -> Result<&str, DnsError> {
        self.zone_id
            .get_or_try_init(|| async {
                let body = checked(
                    self.http
                        .get(format!("{}/zones", self.api))
                        .query(&[("name", self.zone.as_str())])
                        .bearer_auth(&self.token)
                        .send()
                        .await?,
                )
                .await?;
                body["result"][0]["id"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| {
                        DnsError::Provider(format!("Cloudflare has no zone {}", self.zone))
                    })
            })
            .await
            .map(String::as_str)
    }

    fn fqdn(&self, name: &str) -> String {
        if name == "@" {
            self.zone.clone()
        } else {
            format!("{name}.{}", self.zone)
        }
    }

    async fn records(&self, name: &str, record_type: &str) -> Result<Vec<Value>, DnsError> {
        let zone_id = self.zone_id().await?;
        let body = checked(
            self.http
                .get(format!("{}/zones/{zone_id}/dns_records", self.api))
                .query(&[("type", record_type), ("name", &self.fqdn(name))])
                .bearer_auth(&self.token)
                .send()
                .await?,
        )
        .await?;
        Ok(body["result"].as_array().cloned().unwrap_or_default())
    }
}

#[async_trait]
impl DnsProvider for Cloudflare {
    fn kind(&self) -> &'static str {
        "cloudflare"
    }

    fn zone(&self) -> &str {
        &self.zone
    }

    async fn lookup(&self, name: &str, record_type: &str) -> Result<Vec<DnsValue>, DnsError> {
        Ok(self
            .records(name, record_type)
            .await?
            .iter()
            .filter_map(|record| {
                Some(DnsValue {
                    value: record["content"].as_str()?.to_string(),
                    ttl: record["ttl"].as_u64().unwrap_or(1) as u32,
                    id: record["id"].as_str().map(str::to_string),
                })
            })
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        record_type: &str,
        value: &str,
        ttl: u32,
    ) -> Result<DnsValue, DnsError> {
        let zone_id = self.zone_id().await?.to_string();
        let existing = self.records(name, record_type).await?;
        let body = json!({
            "type": record_type,
            "name": self.fqdn(name),
            "content": value,
            "ttl": ttl,
            "proxied": false,
        });
        // Exactly one record: update the first, remove the rest.
        let result = match existing.first().and_then(|record| record["id"].as_str()) {
            Some(id) => {
                checked(
                    self.http
                        .put(format!("{}/zones/{zone_id}/dns_records/{id}", self.api))
                        .bearer_auth(&self.token)
                        .json(&body)
                        .send()
                        .await?,
                )
                .await?
            }
            None => {
                checked(
                    self.http
                        .post(format!("{}/zones/{zone_id}/dns_records", self.api))
                        .bearer_auth(&self.token)
                        .json(&body)
                        .send()
                        .await?,
                )
                .await?
            }
        };
        for extra in existing.iter().skip(1) {
            if let Some(id) = extra["id"].as_str() {
                checked(
                    self.http
                        .delete(format!("{}/zones/{zone_id}/dns_records/{id}", self.api))
                        .bearer_auth(&self.token)
                        .send()
                        .await?,
                )
                .await?;
            }
        }
        Ok(DnsValue {
            value: value.into(),
            ttl,
            id: result["result"]["id"].as_str().map(str::to_string),
        })
    }

    async fn remove(&self, name: &str, record_type: &str) -> Result<(), DnsError> {
        let zone_id = self.zone_id().await?.to_string();
        for record in self.records(name, record_type).await? {
            if let Some(id) = record["id"].as_str() {
                checked(
                    self.http
                        .delete(format!("{}/zones/{zone_id}/dns_records/{id}", self.api))
                        .bearer_auth(&self.token)
                        .send()
                        .await?,
                )
                .await?;
            }
        }
        Ok(())
    }
}

// ---- File -----------------------------------------------------------------

/// `{ "<name> <type>": { "value": …, "ttl": … } }`
struct FileDns {
    zone: String,
    path: PathBuf,
}

impl FileDns {
    fn read(&self) -> Result<BTreeMap<String, DnsValue>, DnsError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| DnsError::Provider(format!("{}: {error}", self.path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(DnsError::Provider(error.to_string())),
        }
    }

    fn write(&self, records: &BTreeMap<String, DnsValue>) -> Result<(), DnsError> {
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|error| DnsError::Provider(error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| DnsError::Provider(error.to_string()))?;
        }
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|error| DnsError::Provider(error.to_string()))
    }
}

#[async_trait]
impl DnsProvider for FileDns {
    fn kind(&self) -> &'static str {
        "file"
    }

    fn zone(&self) -> &str {
        &self.zone
    }

    async fn lookup(&self, name: &str, record_type: &str) -> Result<Vec<DnsValue>, DnsError> {
        Ok(self
            .read()?
            .get(&format!("{name} {record_type}"))
            .cloned()
            .into_iter()
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        record_type: &str,
        value: &str,
        ttl: u32,
    ) -> Result<DnsValue, DnsError> {
        let mut records = self.read()?;
        let record = DnsValue {
            value: value.into(),
            ttl,
            id: Some(format!("{name} {record_type}")),
        };
        records.insert(format!("{name} {record_type}"), record.clone());
        self.write(&records)?;
        Ok(record)
    }

    async fn remove(&self, name: &str, record_type: &str) -> Result<(), DnsError> {
        let mut records = self.read()?;
        if records.remove(&format!("{name} {record_type}")).is_some() {
            self.write(&records)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_relative_to_their_zone() {
        assert_eq!(relative_name("example.com", "example.com").unwrap(), "@");
        assert_eq!(
            relative_name("API.Example.com.", "example.com").unwrap(),
            "api"
        );
        assert_eq!(
            relative_name("a.b.example.com", "example.com").unwrap(),
            "a.b"
        );
        assert!(relative_name("example.org", "example.com").is_err());
        assert!(relative_name("badexample.com", "example.com").is_err());
    }

    #[tokio::test]
    async fn the_file_provider_holds_exactly_one_value() {
        let root = tempfile::tempdir().unwrap();
        let provider = DnsProviderConfig::File {
            zone: "example.com".into(),
            path: root.path().join("zone.json"),
        }
        .build()
        .unwrap();
        assert!(provider.lookup("www", "A").await.unwrap().is_empty());
        provider.apply("www", "A", "192.0.2.1", 300).await.unwrap();
        provider.apply("www", "A", "192.0.2.2", 300).await.unwrap();
        let values = provider.lookup("www", "A").await.unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, "192.0.2.2");
        provider.remove("www", "A").await.unwrap();
        assert!(provider.lookup("www", "A").await.unwrap().is_empty());
    }

    #[test]
    fn a_missing_token_is_reported_by_name() {
        let error = DnsProviderConfig::Hetzner {
            zone: "example.com".into(),
            token_env: "COMPUTE_TEST_NO_SUCH_TOKEN".into(),
            api_url: None,
        }
        .build()
        .err()
        .unwrap();
        assert!(error.to_string().contains("COMPUTE_TEST_NO_SUCH_TOKEN"));
    }
}
