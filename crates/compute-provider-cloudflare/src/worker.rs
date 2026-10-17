//! The Worker Compute deploys: the shared harness plus one WASM module.
//!
//! Everything here is a pure function of the module, the settings, and the
//! credential, so a retried deployment produces the same Worker with the
//! same name: retries never create a second deployment.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::CloudflareSettings;

pub const HARNESS_SOURCE: &str = include_str!("../worker/harness.mjs");
pub const ENTRY_SOURCE: &str = include_str!("../worker/worker.mjs");
pub const HARNESS_VERSION: &str = "compute-harness/1";
pub const INVOKE_PROTOCOL: &str = "compute.cloudflare.invoke@1";

pub const BINDING_TOKEN: &str = "COMPUTE_INVOKE_TOKEN";
pub const BINDING_WORKLOAD: &str = "COMPUTE_WORKLOAD_DIGEST";
pub const BINDING_DEPLOYMENT: &str = "COMPUTE_DEPLOYMENT";

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Content identity of the harness (both JavaScript modules).
pub fn harness_digest() -> String {
    let mut hasher = Sha256::new();
    for part in [HARNESS_VERSION, HARNESS_SOURCE, ENTRY_SOURCE] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// A Worker as Compute would deploy it. It holds no credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerPlan {
    /// `<prefix><32 hex>`: a function of everything deployed.
    pub worker: String,
    /// Identity of the deployment: harness, module, compatibility date.
    pub deployment_id: String,
    pub workload_digest: String,
    pub harness_digest: String,
    pub harness_version: String,
    pub compatibility_date: String,
    pub module_bytes: u64,
}

impl WorkerPlan {
    pub fn new(settings: &CloudflareSettings, module: &[u8]) -> Self {
        let workload_digest = sha256(module);
        let harness_digest = harness_digest();
        #[derive(Serialize)]
        struct Identity<'a> {
            format: &'a str,
            harness: &'a str,
            workload: &'a str,
            compatibility_date: &'a str,
        }
        let deployment_id = sha256(
            &serde_json::to_vec(&Identity {
                format: "compute.cloudflare.deployment@1",
                harness: &harness_digest,
                workload: &workload_digest,
                compatibility_date: &settings.compatibility_date,
            })
            .expect("serializable"),
        );
        let digest = &deployment_id["sha256:".len().."sha256:".len() + 32];
        Self {
            worker: format!("{}{digest}", settings.worker_prefix),
            deployment_id,
            workload_digest,
            harness_digest,
            harness_version: HARNESS_VERSION.into(),
            compatibility_date: settings.compatibility_date.clone(),
            module_bytes: module.len() as u64,
        }
    }

    /// The deployment metadata without the secret: what the manifest
    /// identity covers and what evidence may show.
    pub fn public_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "main_module": "worker.mjs",
            "compatibility_date": self.compatibility_date,
            "bindings": [
                { "type": "secret_text", "name": BINDING_TOKEN },
                { "type": "plain_text", "name": BINDING_WORKLOAD, "text": self.workload_digest },
                { "type": "plain_text", "name": BINDING_DEPLOYMENT, "text": self.deployment_id },
            ],
        })
    }

    pub fn manifest_id(&self) -> String {
        sha256(&serde_json::to_vec(&self.public_metadata()).expect("serializable"))
    }

    /// The multipart upload Cloudflare's script API takes: metadata, the
    /// two harness modules, and the workload module. Returns the content
    /// type (with its boundary) and the body.
    pub fn upload(&self, module: &[u8], invoke_token: &str) -> (String, Vec<u8>) {
        let mut metadata = self.public_metadata();
        metadata["bindings"][0]["text"] = serde_json::Value::String(invoke_token.into());
        let metadata = serde_json::to_vec(&metadata).expect("serializable");
        let parts: [(&str, &str, &[u8]); 4] = [
            ("metadata", "application/json", &metadata),
            (
                "worker.mjs",
                "application/javascript+module",
                ENTRY_SOURCE.as_bytes(),
            ),
            (
                "harness.mjs",
                "application/javascript+module",
                HARNESS_SOURCE.as_bytes(),
            ),
            ("workload.wasm", "application/wasm", module),
        ];
        // A boundary that occurs in no part.
        let mut boundary = format!("compute-{}", &self.deployment_id["sha256:".len()..]);
        while parts
            .iter()
            .any(|(_, _, data)| contains(data, boundary.as_bytes()))
        {
            boundary.push('x');
        }
        let mut body = Vec::with_capacity(module.len() + HARNESS_SOURCE.len() + 4096);
        for (name, content_type, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            if name == "metadata" {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
                );
            } else {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\n"
                    )
                    .as_bytes(),
                );
            }
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The invocation secret of one Worker: HMAC-SHA256 of the account, the
/// Worker, and the protocol, keyed with the API token. Derived, never
/// stored: Compute keeps no secret of its own, and a rotated API token
/// (deliberately) stops existing Workers from accepting invocations until
/// they are redeployed.
pub fn invoke_token(api_token: &str, account_id: &str, worker: &str) -> String {
    let message = format!("{INVOKE_PROTOCOL}\0{account_id}\0{worker}");
    format!(
        "{:x}",
        hmac_sha256(api_token.as_bytes(), message.as_bytes())
    )
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> impl std::fmt::LowerHex {
    const BLOCK: usize = 64;
    let mut block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CloudflareSettings {
        CloudflareSettings::new("0123456789abcdef0123456789abcdef")
    }

    #[test]
    fn plans_are_a_function_of_their_content() {
        let one = WorkerPlan::new(&settings(), b"\0asm module one");
        assert_eq!(one, WorkerPlan::new(&settings(), b"\0asm module one"));
        let two = WorkerPlan::new(&settings(), b"\0asm module two");
        assert_ne!(one.worker, two.worker);
        assert!(one.worker.starts_with("compute-") && one.worker.len() == 40);
        let mut later = settings();
        later.compatibility_date = "2027-01-01".into();
        assert_ne!(
            one.deployment_id,
            WorkerPlan::new(&later, b"\0asm module one").deployment_id
        );
    }

    #[test]
    fn the_secret_is_in_the_upload_only() {
        let plan = WorkerPlan::new(&settings(), b"\0asm");
        let token = invoke_token("api-token", "acct", &plan.worker);
        let (content_type, body) = plan.upload(b"\0asm", &token);
        assert!(content_type.starts_with("multipart/form-data; boundary=compute-"));
        assert!(contains(&body, token.as_bytes()));
        assert!(!plan.public_metadata().to_string().contains(&token));
        assert!(!serde_json::to_string(&plan).unwrap().contains(&token));
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231, test case 2.
        assert_eq!(
            format!(
                "{:x}",
                hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            ),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_ne!(
            invoke_token("token-a", "acct", "w"),
            invoke_token("token-b", "acct", "w")
        );
    }
}
