//! Discovering the management UI a registered service contributes.
//!
//! Compute does not implement any service's domain UI. A registered service
//! that declares the `AppPort/ui/1` capability is asked for its contribution at
//! `GET {endpoint}/v1/ui` (the discovery path the AppPort protocol defines:
//! `@appport/protocol`, `UI_DISCOVERY_PATH`), the answer is validated as
//! untrusted data, and Compute reports **links to the service's own pages**.
//! Nothing is rendered from the document except plain text and those links, and
//! the service keeps authenticating its own users: Compute forwards no
//! credential, cookie or token to it.
//!
//! The validation mirrors `validateUiContribution` in the protocol package
//! (`packages/protocol/src/ui.ts` in the AppPort repository), which owns the
//! schema; the fixtures in `tests/fixtures/appport-ui/` pin the two together.
//! Nothing here is stored: every call reads the current registration.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use compute_state::ServiceRecord;

/// The capability a registered service declares to say it contributes a UI. It
/// is the protocol's own identifier, not a name Compute invented.
pub const UI_CAPABILITY: &str = "AppPort/ui/1";
/// The protocol's discovery path.
pub const DISCOVERY_PATH: &str = "/v1/ui";

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BYTES: usize = 256 * 1024;
const MAX_LINKS: usize = 64;
const MAX_TEXT: usize = 512;
/// Composition requirements a host may be asked to supply (protocol
/// `UiCompositionRequirement`).
const REQUIREMENTS: [&str; 4] = ["identity", "tenant", "application", "environment"];
const RESERVED_SEGMENTS: [&str; 12] = [
    "http",
    "https",
    "ws",
    "wss",
    "websocket",
    "webtransport",
    "ipc",
    "electron",
    "tauri",
    "rest",
    "grpc",
    "inprocess",
];

/// Where discovery ended. Only `Available` carries links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiStatus {
    /// The service declares no UI contribution. Not an error.
    None,
    /// A valid contribution with at least one link.
    Available,
    /// The service declares one but advertises nothing now (`404`, or no
    /// navigation).
    Empty,
    /// Could not be reached, or timed out.
    Unreachable,
    /// The service refused an anonymous discovery request (`401`/`403`).
    Unauthorized,
    /// The answer was not a usable `AppPort/ui/1` document.
    Invalid,
    /// The contribution needs context Compute does not supply (`identity`,
    /// `tenant`, `application`, `environment`), so a conforming host does not
    /// compose it.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UiLink {
    pub id: String,
    pub label: String,
    pub group: String,
    pub order: i64,
    /// An absolute `http(s)` URL on the service's own origin, built from the
    /// registered endpoint and the surface's route.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceUi {
    pub service: String,
    pub status: UiStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product: Option<UiProduct>,
    pub links: Vec<UiLink>,
    /// Why discovery ended as it did. Plain text; never markup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UiProduct {
    pub id: String,
    pub version: String,
}

impl ServiceUi {
    fn of(service: &str, status: UiStatus, message: impl Into<Option<String>>) -> Self {
        Self {
            service: service.into(),
            status,
            product: None,
            links: vec![],
            message: message.into(),
        }
    }
}

/// Whether the registration declares a UI contribution.
pub fn declares_ui(record: &ServiceRecord) -> bool {
    record
        .capabilities
        .iter()
        .any(|capability| capability == UI_CAPABILITY)
}

/// Ask a registered service for its UI contribution. Never fails: every
/// outcome, including a hostile or broken answer, is a [`ServiceUi`].
pub async fn discover(record: &ServiceRecord) -> ServiceUi {
    let name = record.name.as_str();
    if !declares_ui(record) {
        return ServiceUi::of(name, UiStatus::None, None);
    }
    let Some(endpoint) = record.endpoint.as_deref() else {
        return ServiceUi::of(
            name,
            UiStatus::Invalid,
            "the service declares a UI but has no endpoint".to_string(),
        );
    };
    let base = match parse_base(endpoint) {
        Ok(base) => base,
        Err(reason) => return ServiceUi::of(name, UiStatus::Invalid, reason),
    };
    let body = match fetch(&base).await {
        Ok(Fetched::Body(body)) => body,
        Ok(Fetched::NotAdvertised) => {
            return ServiceUi::of(
                name,
                UiStatus::Empty,
                "the service advertises no UI".to_string(),
            );
        }
        Ok(Fetched::Refused(status)) => {
            return ServiceUi::of(
                name,
                UiStatus::Unauthorized,
                format!("the service refused discovery ({status}); it authenticates its own users"),
            );
        }
        Err(Reached::Failed(reason)) => {
            return ServiceUi::of(name, UiStatus::Unreachable, reason);
        }
        Err(Reached::Invalid(reason)) => return ServiceUi::of(name, UiStatus::Invalid, reason),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return ServiceUi::of(
                name,
                UiStatus::Invalid,
                "the answer was not JSON".to_string(),
            );
        }
    };
    match interpret(name, &base, &value) {
        Ok(view) => view,
        Err(Rejection::Unsupported(reason)) => ServiceUi::of(name, UiStatus::Unsupported, reason),
        Err(Rejection::Invalid(reason)) => ServiceUi::of(name, UiStatus::Invalid, reason),
    }
}

// ---- Fetching ---------------------------------------------------------------

enum Fetched {
    Body(Vec<u8>),
    NotAdvertised,
    Refused(u16),
}

enum Reached {
    Failed(String),
    Invalid(String),
}

/// The registered endpoint, as a URL Compute is willing to contact: `http` or
/// `https`, a host, no credentials in it.
fn parse_base(endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| "the registered endpoint is not a URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("the registered endpoint must be an http(s) URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the registered endpoint must not contain credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the registered endpoint must not carry a query or fragment".into());
    }
    Ok(url)
}

/// `{base}/v1/ui` with the base path preserved.
fn discovery_url(base: &reqwest::Url) -> reqwest::Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}{DISCOVERY_PATH}"));
    url
}

/// An anonymous, bounded, non-redirecting GET. No Compute credential, cookie or
/// header of the caller is sent: the request carries nothing about who asked.
async fn fetch(base: &reqwest::Url) -> Result<Fetched, Reached> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("compute-environment/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Reached::Failed("could not prepare the request".into()))?;
    let mut response = client
        .get(discovery_url(base))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| {
            Reached::Failed(if error.is_timeout() {
                "the service did not answer in time".to_string()
            } else {
                "the service could not be reached".to_string()
            })
        })?;
    let status = response.status().as_u16();
    match status {
        200 => {}
        404 => return Ok(Fetched::NotAdvertised),
        401 | 403 => return Ok(Fetched::Refused(status)),
        other => {
            return Err(Reached::Failed(format!(
                "the service answered {other} to discovery"
            )));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Reached::Failed("the answer was cut off".into()))?
    {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err(Reached::Invalid(format!(
                "the answer is larger than {MAX_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetched::Body(body))
}

// ---- Validation ---------------------------------------------------------------

enum Rejection {
    Invalid(String),
    Unsupported(String),
}

fn invalid<T>(reason: impl Into<String>) -> Result<T, Rejection> {
    Err(Rejection::Invalid(reason.into()))
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, Rejection> {
    match value.as_str() {
        Some(text) if !text.is_empty() && text.chars().count() <= MAX_TEXT => Ok(text),
        _ => invalid(format!("`{field}` must be a non-empty string")),
    }
}

/// `<namespace>.<operation>`: lowercase ASCII letters and digits, at least two
/// dot-separated segments, none a transport word, at most 128 characters
/// (`validateName`, protocol `names.ts`).
fn valid_capability_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let segments = name.split('.').collect::<Vec<_>>();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            let mut characters = segment.chars();
            characters
                .next()
                .is_some_and(|first| first.is_ascii_lowercase())
                && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && !RESERVED_SEGMENTS.contains(segment)
        })
}

/// A route is a path on the service's own origin: it starts with one `/`, has
/// no scheme or authority, no backslash, whitespace or control character, and no
/// dot segments (`validateUiContribution` rejects `//` and `://`; Compute also
/// refuses what could leave the service's path).
fn valid_route(route: &str) -> bool {
    route.starts_with('/')
        && !route.starts_with("//")
        && !route.contains("://")
        && !route.contains('\\')
        && !route.contains('?')
        && !route.contains('#')
        && !route.chars().any(|c| c.is_control() || c.is_whitespace())
        && !route
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

fn interpret(service: &str, base: &reqwest::Url, value: &Value) -> Result<ServiceUi, Rejection> {
    let Some(document) = value.as_object() else {
        return invalid("the UI document must be an object");
    };
    match document.get("protocol").and_then(Value::as_str) {
        Some(UI_CAPABILITY) => {}
        other => {
            return Err(Rejection::Invalid(format!(
                "unsupported UI protocol {}: expected {UI_CAPABILITY}",
                other.map_or_else(
                    || "(none)".to_string(),
                    |found| found.chars().take(64).collect::<String>()
                )
            )));
        }
    }
    let product = document
        .get("product")
        .and_then(Value::as_object)
        .ok_or_else(|| Rejection::Invalid("`product` must be an object".into()))?;
    let product = UiProduct {
        id: text(product.get("id").unwrap_or(&Value::Null), "product.id")?.to_string(),
        version: text(
            product.get("version").unwrap_or(&Value::Null),
            "product.version",
        )?
        .to_string(),
    };
    let (Some(surfaces), Some(navigation)) = (
        document.get("surfaces").and_then(Value::as_array),
        document.get("navigation").and_then(Value::as_array),
    ) else {
        return invalid("`surfaces` and `navigation` must be arrays");
    };

    let mut routes = std::collections::BTreeMap::new();
    for (index, surface) in surfaces.iter().enumerate() {
        let Some(surface) = surface.as_object() else {
            return invalid(format!("surface {index} must be an object"));
        };
        let id = text(surface.get("id").unwrap_or(&Value::Null), "surface.id")?;
        text(
            surface.get("title").unwrap_or(&Value::Null),
            "surface.title",
        )?;
        let route = text(
            surface.get("route").unwrap_or(&Value::Null),
            "surface.route",
        )?;
        if !valid_route(route) {
            return invalid(format!("surface {id} has a malformed route"));
        }
        let capabilities = surface.get("capabilities").and_then(Value::as_array);
        if !capabilities.is_some_and(|names| {
            names
                .iter()
                .all(|name| name.as_str().is_some_and(valid_capability_name))
        }) {
            return invalid(format!("surface {id} has invalid capability references"));
        }
        if routes.insert(id.to_string(), route.to_string()).is_some() {
            return invalid(format!("duplicate surface id: {id}"));
        }
    }

    let mut seen = BTreeSet::new();
    let mut links = vec![];
    for (index, item) in navigation.iter().enumerate() {
        let Some(item) = item.as_object() else {
            return invalid(format!("navigation {index} must be an object"));
        };
        let id = text(item.get("id").unwrap_or(&Value::Null), "navigation.id")?;
        let label = text(
            item.get("label").unwrap_or(&Value::Null),
            "navigation.label",
        )?;
        let group = text(
            item.get("group").unwrap_or(&Value::Null),
            "navigation.group",
        )?;
        let surface = text(
            item.get("surface").unwrap_or(&Value::Null),
            "navigation.surface",
        )?;
        let Some(order) = item.get("order").and_then(Value::as_i64) else {
            return invalid(format!("navigation {id} order must be an integer"));
        };
        if !seen.insert(id.to_string()) {
            return invalid(format!("duplicate navigation id: {id}"));
        }
        let Some(route) = routes.get(surface) else {
            return invalid("navigation references an unknown surface");
        };
        links.push(UiLink {
            id: id.to_string(),
            label: label.to_string(),
            group: group.to_string(),
            order,
            url: surface_url(base, route)?,
        });
    }

    let requires = document
        .get("composition")
        .and_then(Value::as_object)
        .and_then(|composition| composition.get("requires"))
        .and_then(Value::as_array);
    let Some(requires) = requires else {
        return invalid("`composition.requires` must be an array");
    };
    let mut needed = vec![];
    for requirement in requires {
        match requirement.as_str() {
            Some(name) if REQUIREMENTS.contains(&name) => needed.push(name),
            _ => return invalid("unsupported composition requirements"),
        }
    }
    if !needed.is_empty() {
        // The protocol's own composer skips a contribution whose context the host
        // does not hold. Compute holds none of it for an AppPort product.
        return Err(Rejection::Unsupported(format!(
            "the contribution requires {} from its host, which Compute does not supply",
            needed.join(", ")
        )));
    }

    if links.len() > MAX_LINKS {
        return invalid(format!("more than {MAX_LINKS} navigation entries"));
    }
    links.sort_by(|a, b| {
        (a.group.as_str(), a.order, a.id.as_str()).cmp(&(b.group.as_str(), b.order, b.id.as_str()))
    });
    let status = if links.is_empty() {
        UiStatus::Empty
    } else {
        UiStatus::Available
    };
    Ok(ServiceUi {
        service: service.into(),
        status,
        product: Some(product),
        links,
        message: None,
    })
}

/// The service's page for `route`, on the registered endpoint's origin. Built,
/// never taken from the document verbatim, and checked once more after parsing.
fn surface_url(base: &reqwest::Url, route: &str) -> Result<String, Rejection> {
    let mut url = base.clone();
    let prefix = base.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{route}"));
    if url.origin() != base.origin()
        || !matches!(url.scheme(), "http" | "https")
        || !url.path().starts_with(prefix)
    {
        return invalid("a route does not stay on the service's origin");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> reqwest::Url {
        reqwest::Url::parse("http://127.0.0.1:4100").unwrap()
    }

    fn document() -> Value {
        json!({
            "protocol": "AppPort/ui/1",
            "product": { "id": "demo", "version": "1.0.0" },
            "surfaces": [
                { "id": "keys", "title": "Keys", "route": "/api-keys", "capabilities": ["apikeys.read"] },
                { "id": "jobs", "title": "Jobs", "route": "/jobs", "capabilities": [] }
            ],
            "navigation": [
                { "id": "n.jobs", "label": "Jobs", "group": "Demo", "order": 20, "surface": "jobs" },
                { "id": "n.keys", "label": "Keys", "group": "Demo", "order": 10, "surface": "keys" }
            ],
            "composition": { "requires": [] }
        })
    }

    fn rejected(value: &Value) -> String {
        match interpret("s", &base(), value) {
            Ok(view) => panic!("accepted: {view:?}"),
            Err(Rejection::Invalid(reason) | Rejection::Unsupported(reason)) => reason,
        }
    }

    #[test]
    fn a_valid_document_becomes_ordered_links_on_the_services_origin() {
        let view = interpret("s", &base(), &document()).ok().unwrap();
        assert_eq!(view.status, UiStatus::Available);
        assert_eq!(
            view.links
                .iter()
                .map(|l| l.url.as_str())
                .collect::<Vec<_>>(),
            [
                "http://127.0.0.1:4100/api-keys",
                "http://127.0.0.1:4100/jobs"
            ]
        );
        assert_eq!(view.product.unwrap().id, "demo");
    }

    #[test]
    fn the_base_path_is_kept() {
        let base = reqwest::Url::parse("https://host.example/apps/demo/").unwrap();
        let view = interpret("s", &base, &document()).ok().unwrap();
        assert_eq!(view.links[0].url, "https://host.example/apps/demo/api-keys");
        assert_eq!(
            discovery_url(&base).as_str(),
            "https://host.example/apps/demo/v1/ui"
        );
    }

    #[test]
    fn routes_that_could_leave_the_service_are_rejected() {
        for route in [
            "https://evil.example/",
            "//evil.example/x",
            "javascript:alert(1)",
            "/ok/../../escape",
            "/a\\b",
            "/a b",
            "/a?x=1",
            "relative",
            "",
        ] {
            let mut value = document();
            value["surfaces"][0]["route"] = json!(route);
            rejected(&value);
        }
    }

    #[test]
    fn protocol_violations_are_rejected_not_repaired() {
        type Mutation = fn(&mut Value);
        let cases: [(&str, Mutation); 11] = [
            ("protocol", |v| v["protocol"] = json!("AppPort/ui/2")),
            ("product", |v| v["product"] = json!("x")),
            ("surfaces", |v| v["surfaces"] = json!({})),
            ("capability", |v| {
                v["surfaces"][0]["capabilities"] = json!(["Bad Name"])
            }),
            ("transport word", |v| {
                v["surfaces"][0]["capabilities"] = json!(["a.http"])
            }),
            ("duplicate surface", |v| {
                v["surfaces"][1]["id"] = json!("keys")
            }),
            ("duplicate nav", |v| {
                v["navigation"][1]["id"] = json!("n.jobs")
            }),
            ("unknown surface", |v| {
                v["navigation"][0]["surface"] = json!("nope")
            }),
            ("order", |v| v["navigation"][0]["order"] = json!("first")),
            ("requires", |v| {
                v["composition"]["requires"] = json!(["admin"])
            }),
            ("composition", |v| v["composition"] = json!(null)),
        ];
        for (what, mutate) in cases {
            let mut value = document();
            mutate(&mut value);
            assert!(interpret("s", &base(), &value).is_err(), "{what}");
        }
        assert!(interpret("s", &base(), &json!([])).is_err());
        assert!(interpret("s", &base(), &json!(null)).is_err());
    }

    #[test]
    fn a_contribution_that_needs_context_compute_lacks_is_not_composed() {
        let mut value = document();
        value["composition"]["requires"] = json!(["identity", "tenant"]);
        assert!(rejected(&value).contains("identity, tenant"));
    }

    #[test]
    fn markup_in_text_is_data_and_is_never_interpreted() {
        // The strings come back verbatim as text; the UI renders them with
        // `textContent`. Only the URL is a link, and it is built, not copied.
        let mut value = document();
        value["navigation"][0]["label"] = json!("<img src=x onerror=alert(1)>");
        let view = interpret("s", &base(), &value).ok().unwrap();
        assert!(view.links.iter().any(|l| l.label.starts_with("<img")));
        assert!(
            view.links
                .iter()
                .all(|l| l.url.starts_with("http://127.0.0.1:4100/"))
        );
    }

    #[test]
    fn oversized_text_and_too_many_entries_are_rejected() {
        let mut value = document();
        value["navigation"][0]["label"] = json!("x".repeat(MAX_TEXT + 1));
        rejected(&value);
        let mut value = document();
        let many = (0..=MAX_LINKS)
            .map(|i| json!({ "id": format!("n{i}"), "label": "L", "group": "G", "order": i, "surface": "keys" }))
            .collect::<Vec<_>>();
        value["navigation"] = json!(many);
        rejected(&value);
    }

    #[test]
    fn an_empty_navigation_is_empty_not_an_error() {
        let mut value = document();
        value["navigation"] = json!([]);
        assert_eq!(
            interpret("s", &base(), &value).ok().unwrap().status,
            UiStatus::Empty
        );
    }

    #[test]
    fn endpoints_compute_will_not_contact_are_refused() {
        for endpoint in [
            "ftp://x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://user:pw@host/",
            "http://host/?a=1",
            "http://host/#x",
            "not a url",
        ] {
            assert!(parse_base(endpoint).is_err(), "{endpoint}");
        }
        assert!(parse_base("http://127.0.0.1:4100").is_ok());
        assert!(parse_base("https://svc.example/base").is_ok());
    }
}
