//! The GitHub side of the adapter: repository identity and the one API call
//! the adapter makes, behind a trait so tests never need GitHub.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::WorkerError;
use crate::secret::{Secret, scrub};

/// `OWNER/NAME`, validated. Nothing else about a repository is accepted, so
/// the value is safe to place in a URL and in a shell environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    pub fn parse(value: &str) -> Result<Self, WorkerError> {
        let invalid = || WorkerError::InvalidRepository(value.to_owned());
        let (owner, name) = value.split_once('/').ok_or_else(invalid)?;
        let owner_ok = !owner.is_empty()
            && owner.len() <= 39
            && !owner.starts_with('-')
            && !owner.ends_with('-')
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if owner_ok && name_ok {
            Ok(Self {
                owner: owner.to_owned(),
                name: name.to_owned(),
            })
        } else {
            Err(invalid())
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Repository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// How the adapter obtains a runner registration token.
#[async_trait]
pub trait GitHubApi: Send + Sync {
    /// A short-lived token that lets one runner register with `repository`.
    /// `credential` authenticates the request and is never part of the result.
    async fn registration_token(
        &self,
        repository: &Repository,
        credential: &Secret,
    ) -> Result<Secret, WorkerError>;
}

/// The GitHub REST API (`POST /repos/{owner}/{repo}/actions/runners/registration-token`).
pub struct RestGitHubApi {
    api_url: String,
    client: reqwest::Client,
}

impl RestGitHubApi {
    pub const DEFAULT_API_URL: &'static str = "https://api.github.com";

    pub fn new(api_url: impl Into<String>) -> Result<Self, WorkerError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("compute-worker-github/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| WorkerError::Unreachable(error.without_url().to_string()))?;
        Ok(Self {
            api_url: api_url.into().trim_end_matches('/').to_owned(),
            client,
        })
    }
}

#[async_trait]
impl GitHubApi for RestGitHubApi {
    async fn registration_token(
        &self,
        repository: &Repository,
        credential: &Secret,
    ) -> Result<Secret, WorkerError> {
        let url = format!(
            "{}/repos/{}/{}/actions/runners/registration-token",
            self.api_url,
            repository.owner(),
            repository.name()
        );
        let response = self
            .client
            .post(url)
            .bearer_auth(credential.expose())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|error| {
                WorkerError::Unreachable(scrub(&error.without_url().to_string(), &[credential]))
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| WorkerError::Unreachable(scrub(&error.to_string(), &[credential])))?;
        if status.is_success() {
            let token = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["token"].as_str().map(str::to_owned))
                .filter(|token| !token.is_empty())
                .ok_or(WorkerError::MalformedResponse)?;
            return Ok(Secret::new(token));
        }
        // GitHub's own message, never the request: it carries no credential,
        // but scrub anyway.
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        let message = scrub(&message, &[credential]);
        Err(match status.as_u16() {
            404 => WorkerError::RepositoryNotFound(repository.to_string()),
            401 | 403 => WorkerError::Unauthorized {
                status: status.as_u16(),
                message,
            },
            code => WorkerError::Rejected {
                status: code,
                message,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_are_validated() {
        for good in ["rkendel1/compute", "a/b", "org-1/my_repo.rs", "o/.github"] {
            assert!(Repository::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "", "x", "/x", "x/", "a/b/c", "a b/c", "a/b c", "-a/b", "a-/b", "a/..", "a/.",
            "a;rm/b", "a/b$(x)", "a/b\"", "o_/r",
        ] {
            assert!(Repository::parse(bad).is_err(), "{bad}");
        }
    }
}
