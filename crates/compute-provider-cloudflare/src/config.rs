//! Provider configuration: the `[providers.<id>.cloudflare]` table of a pool
//! file. It names an account, a profile, a naming policy, and limits. It
//! never holds a credential: the API token is read from the environment
//! variable (or file) the pool member names with `token_env`/`token_file`.

use serde::{Deserialize, Serialize};

use crate::profile::{CloudflareProfile, WORKERS_FREE};

pub const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";
pub const DEFAULT_WORKER_PREFIX: &str = "compute-";
pub const DEFAULT_COMPATIBILITY_DATE: &str = "2026-10-01";
pub const DEFAULT_TOKEN_ENV: &str = "CLOUDFLARE_API_TOKEN";

const MIB: u64 = 1024 * 1024;

/// Limits Compute enforces before and during each operation. Each is
/// bounded by the profile; none can exceed what Cloudflare provides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionLimits {
    /// Wall-clock limit of one invocation, measured by Compute. A workload
    /// may ask for less (`resources.timeout_ms`), never more.
    pub timeout_ms: u64,
    /// Largest WASM module Compute deploys.
    pub max_module_bytes: u64,
    /// Largest standard input Compute sends with one invocation.
    pub max_stdin_bytes: u64,
    /// Largest stdout and stderr (each) the harness returns.
    pub max_output_bytes: u64,
    /// Invocations this Compute process runs at once through this member.
    /// A request beyond it fails with `quota_exhausted`; it never queues.
    pub max_concurrent_invocations: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_module_bytes: 10 * MIB,
            max_stdin_bytes: MIB,
            max_output_bytes: MIB,
            max_concurrent_invocations: 6,
        }
    }
}

/// Upper bounds of Compute's own limits, independent of the profile.
pub const MAX_TIMEOUT_MS: u64 = 15 * 60 * 1000;
pub const MAX_STDIN_BYTES: u64 = 32 * MIB;
pub const MAX_OUTPUT_BYTES: u64 = 32 * MIB;
pub const MAX_CONCURRENT_INVOCATIONS: u64 = 100;
/// Room the harness and its metadata take inside one Worker.
pub const HARNESS_ALLOWANCE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareSettings {
    /// The Cloudflare account identifier (32 hexadecimal characters). An
    /// identifier, not a credential.
    pub account_id: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Workers Compute deploys are named `<prefix><content digest>`. The
    /// prefix is how Compute recognizes, lists, and deletes its own Workers;
    /// it never touches a Worker without it.
    #[serde(default = "default_prefix")]
    pub worker_prefix: String,
    /// The Workers compatibility date the harness is deployed with.
    #[serde(default = "default_compatibility_date")]
    pub compatibility_date: String,
    #[serde(default)]
    pub limits: ExecutionLimits,
    /// Cloudflare API base. Only for tests against a local fake: anything
    /// but the default must be `https://` or loopback `http://`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    /// Base URL invocations are sent to instead of
    /// `https://<worker>.<subdomain>.workers.dev`. Only for tests against a
    /// local fake, under the same rule as `api_base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers_base: Option<String>,
}

fn default_profile() -> String {
    WORKERS_FREE.into()
}

fn default_prefix() -> String {
    DEFAULT_WORKER_PREFIX.into()
}

fn default_compatibility_date() -> String {
    DEFAULT_COMPATIBILITY_DATE.into()
}

/// A configuration problem, named precisely enough to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl CloudflareSettings {
    pub fn new(account_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            profile: default_profile(),
            worker_prefix: default_prefix(),
            compatibility_date: default_compatibility_date(),
            limits: ExecutionLimits::default(),
            api_base: None,
            workers_base: None,
        }
    }

    /// Validate the settings and resolve their profile. Every limit is
    /// checked against the profile, so a configuration never promises what
    /// Cloudflare would refuse.
    pub fn validate(&self) -> Result<CloudflareProfile, ConfigError> {
        let fail = |message: String| Err(ConfigError(message));
        if self.account_id.len() != 32
            || !self
                .account_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return fail(
                "account_id must be the 32 lowercase hexadecimal characters of a Cloudflare account identifier".into(),
            );
        }
        let Some(profile) = CloudflareProfile::named(&self.profile) else {
            return fail(format!(
                "unknown Cloudflare profile {:?}; known profiles: {}",
                self.profile,
                CloudflareProfile::names().join(", ")
            ));
        };
        let prefix = &self.worker_prefix;
        if prefix.is_empty()
            || prefix.len() > 24
            || !prefix.as_bytes()[0].is_ascii_lowercase()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return fail(
                "worker_prefix must be 1-24 lowercase letters, digits, or '-', starting with a letter".into(),
            );
        }
        let date = self.compatibility_date.as_bytes();
        if date.len() != 10
            || date[4] != b'-'
            || date[7] != b'-'
            || !date
                .iter()
                .enumerate()
                .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        {
            return fail("compatibility_date must be YYYY-MM-DD".into());
        }
        let limits = &self.limits;
        let bounded = |name: &str, value: u64, max: u64, why: &str| {
            if value == 0 {
                Err(ConfigError(format!("limits.{name} must be positive")))
            } else if value > max {
                Err(ConfigError(format!(
                    "limits.{name} = {value} exceeds {max} ({why})"
                )))
            } else {
                Ok(())
            }
        };
        bounded(
            "timeout_ms",
            limits.timeout_ms,
            MAX_TIMEOUT_MS,
            "Compute's bound on one request-driven invocation",
        )?;
        bounded(
            "max_module_bytes",
            limits.max_module_bytes,
            profile
                .limits
                .worker_size_bytes
                .saturating_sub(HARNESS_ALLOWANCE_BYTES),
            "the profile's Worker size limit, less the harness",
        )?;
        bounded(
            "max_stdin_bytes",
            limits.max_stdin_bytes,
            MAX_STDIN_BYTES.min(profile.limits.request_body_bytes / 2),
            "the request body limit, after encoding",
        )?;
        bounded(
            "max_output_bytes",
            limits.max_output_bytes,
            MAX_OUTPUT_BYTES.min(profile.limits.memory_bytes / 4),
            "the isolate's memory, which holds the output",
        )?;
        bounded(
            "max_concurrent_invocations",
            limits.max_concurrent_invocations,
            MAX_CONCURRENT_INVOCATIONS,
            "Compute's bound on in-flight invocations per member",
        )?;
        for (name, value) in [
            ("api_base", &self.api_base),
            ("workers_base", &self.workers_base),
        ] {
            if let Some(value) = value {
                validate_base(name, value)?;
            }
        }
        Ok(profile)
    }

    pub fn api_base(&self) -> &str {
        self.api_base
            .as_deref()
            .unwrap_or(DEFAULT_API_BASE)
            .trim_end_matches('/')
    }
}

fn validate_base(name: &str, value: &str) -> Result<(), ConfigError> {
    let fail = |message: &str| Err(ConfigError(format!("{name} {message}")));
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return fail("must be a URL without whitespace");
    }
    let rest = if let Some(rest) = value.strip_prefix("https://") {
        rest
    } else if let Some(rest) = value.strip_prefix("http://") {
        let host = rest.split('/').next().unwrap_or_default();
        let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return fail("may use http:// only on loopback (it exists for local fakes)");
        }
        rest
    } else {
        return fail("must be an https:// URL");
    };
    if rest
        .split('/')
        .next()
        .is_some_and(|authority| authority.contains('@') || authority.is_empty())
    {
        return fail("must name a host and must not embed credentials");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn defaults_validate_against_the_free_profile() {
        let profile = CloudflareSettings::new(ACCOUNT).validate().unwrap();
        assert_eq!(profile.name, WORKERS_FREE);
    }

    #[test]
    fn limits_above_the_profile_are_refused() {
        let mut settings = CloudflareSettings::new(ACCOUNT);
        settings.limits.max_module_bytes = 64 * MIB;
        let error = settings.validate().unwrap_err().0;
        assert!(error.contains("max_module_bytes"), "{error}");

        let mut settings = CloudflareSettings::new(ACCOUNT);
        settings.limits.max_concurrent_invocations = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn malformed_settings_are_refused_with_a_reason() {
        for (mutate, expected) in [
            (
                Box::new(|s: &mut CloudflareSettings| s.account_id = "not-an-account".into())
                    as Box<dyn Fn(&mut CloudflareSettings)>,
                "account_id",
            ),
            (
                Box::new(|s| s.profile = "workers-paid".into()),
                "unknown Cloudflare profile",
            ),
            (
                Box::new(|s| s.worker_prefix = "Compute_".into()),
                "worker_prefix",
            ),
            (
                Box::new(|s| s.compatibility_date = "yesterday".into()),
                "compatibility_date",
            ),
            (
                Box::new(|s| s.api_base = Some("http://api.example".into())),
                "loopback",
            ),
            (
                Box::new(|s| s.api_base = Some("https://user:pw@api.example".into())),
                "credentials",
            ),
        ] {
            let mut settings = CloudflareSettings::new(ACCOUNT);
            mutate(&mut settings);
            let error = settings.validate().unwrap_err().0;
            assert!(error.contains(expected), "{error} lacks {expected}");
        }
    }

    #[test]
    fn unknown_keys_are_refused() {
        let error = toml_like(r#"{"account_id":"x","api_token":"secret"}"#);
        assert!(error.contains("api_token"), "{error}");
    }

    fn toml_like(json: &str) -> String {
        serde_json::from_str::<CloudflareSettings>(json)
            .unwrap_err()
            .to_string()
    }
}
