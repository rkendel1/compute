//! DNS providers against faithful fakes of their APIs: each fake keeps
//! records, checks the credential, and speaks the provider's JSON.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use compute_network::dns::DnsProviderConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    authorization: Option<String>,
    body: Value,
}

type Handler = Arc<dyn Fn(Request) -> (u16, Value) + Send + Sync>;

/// A one-request-per-connection HTTP server.
async fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buffer = vec![];
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let count = stream.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..count]);
                    if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut request_line = lines.next().unwrap().split(' ');
                let method = request_line.next().unwrap().to_string();
                let target = request_line.next().unwrap().to_string();
                let mut length = 0;
                let mut authorization = None;
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        if name == "content-length" {
                            length = value.trim().parse().unwrap();
                        }
                        if name == "authorization" {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                }
                while buffer.len() < head_end + length {
                    let count = stream.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..count]);
                }
                let body = &buffer[head_end..head_end + length];
                let (path, query) = match target.split_once('?') {
                    Some((path, query)) => (
                        path.to_string(),
                        query
                            .split('&')
                            .filter_map(|pair| pair.split_once('='))
                            .map(|(k, v)| (k.to_string(), v.replace("%40", "@")))
                            .collect(),
                    ),
                    None => (target, BTreeMap::new()),
                };
                let (status, response) = handler(Request {
                    method,
                    path,
                    query,
                    authorization,
                    body: serde_json::from_slice(body).unwrap_or(Value::Null),
                });
                let text = response.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}

/// Hetzner Cloud DNS: `/zones/{zone}/rrsets[/{name}/{type}[/actions/…]]`.
fn hetzner(records: Arc<Mutex<BTreeMap<(String, String), Value>>>) -> Handler {
    Arc::new(move |request: Request| {
        if request.authorization.as_deref() != Some("Bearer hetzner-token") {
            return (401, json!({ "error": { "code": "unauthorized" } }));
        }
        let parts = request
            .path
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        let mut records = records.lock().unwrap();
        match (request.method.as_str(), parts.as_slice()) {
            ("POST", ["zones", "example.com", "rrsets"]) => {
                let key = (
                    request.body["name"].as_str().unwrap().to_string(),
                    request.body["type"].as_str().unwrap().to_string(),
                );
                let rrset = json!({
                    "id": format!("{}/{}", key.0, key.1),
                    "name": key.0,
                    "type": key.1,
                    "ttl": request.body["ttl"],
                    "records": request.body["records"],
                });
                records.insert(key, rrset.clone());
                (
                    201,
                    json!({ "rrset": rrset, "action": { "status": "running" } }),
                )
            }
            ("GET", ["zones", "example.com", "rrsets", name, kind]) => {
                match records.get(&(name.to_string(), kind.to_string())) {
                    Some(rrset) => (200, json!({ "rrset": rrset })),
                    None => (404, json!({ "error": { "code": "not_found" } })),
                }
            }
            (
                "POST",
                [
                    "zones",
                    "example.com",
                    "rrsets",
                    name,
                    kind,
                    "actions",
                    action,
                ],
            ) => {
                let Some(rrset) = records.get_mut(&(name.to_string(), kind.to_string())) else {
                    return (404, json!({ "error": { "code": "not_found" } }));
                };
                match *action {
                    "set_records" => rrset["records"] = request.body["records"].clone(),
                    "change_ttl" => rrset["ttl"] = request.body["ttl"].clone(),
                    _ => return (400, json!({})),
                }
                (201, json!({ "action": { "status": "running" } }))
            }
            ("DELETE", ["zones", "example.com", "rrsets", name, kind]) => {
                match records.remove(&(name.to_string(), kind.to_string())) {
                    Some(_) => (201, json!({ "action": { "status": "running" } })),
                    None => (404, json!({ "error": { "code": "not_found" } })),
                }
            }
            _ => (404, json!({ "error": { "code": "not_found" } })),
        }
    })
}

/// Cloudflare: `/zones?name=` and `/zones/{id}/dns_records[/{id}]`.
fn cloudflare(records: Arc<Mutex<BTreeMap<String, Value>>>) -> Handler {
    let next = Arc::new(Mutex::new(0));
    Arc::new(move |request: Request| {
        if request.authorization.as_deref() != Some("Bearer cloudflare-token") {
            return (
                403,
                json!({ "success": false, "errors": [{ "code": 9109 }] }),
            );
        }
        let parts = request
            .path
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        let mut records = records.lock().unwrap();
        match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["zones"])
                if request.query.get("name").map(String::as_str) == Some("example.com") =>
            {
                (
                    200,
                    json!({ "success": true, "result": [{ "id": "zone-1", "name": "example.com" }] }),
                )
            }
            ("GET", ["zones", "zone-1", "dns_records"]) => {
                let found = records
                    .values()
                    .filter(|record| {
                        Some(record["name"].as_str().unwrap())
                            == request.query.get("name").map(String::as_str)
                            && Some(record["type"].as_str().unwrap())
                                == request.query.get("type").map(String::as_str)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                (200, json!({ "success": true, "result": found }))
            }
            ("POST", ["zones", "zone-1", "dns_records"]) => {
                let mut id = next.lock().unwrap();
                *id += 1;
                let mut record = request.body.clone();
                record["id"] = json!(format!("rec-{id}"));
                records.insert(format!("rec-{id}"), record.clone());
                (200, json!({ "success": true, "result": record }))
            }
            ("PUT", ["zones", "zone-1", "dns_records", id]) => {
                let mut record = request.body.clone();
                record["id"] = json!(id);
                records.insert(id.to_string(), record.clone());
                (200, json!({ "success": true, "result": record }))
            }
            ("DELETE", ["zones", "zone-1", "dns_records", id]) => {
                records.remove(*id);
                (200, json!({ "success": true, "result": { "id": id } }))
            }
            _ => (404, json!({ "success": false })),
        }
    })
}

#[tokio::test]
async fn hetzner_records_are_created_corrected_and_removed() {
    let records = Arc::new(Mutex::new(BTreeMap::new()));
    let api = serve(hetzner(records.clone())).await;
    // SAFETY: tests set a variable only they read.
    unsafe { std::env::set_var("COMPUTE_TEST_HETZNER_TOKEN", "hetzner-token") };
    let provider = DnsProviderConfig::Hetzner {
        zone: "example.com".into(),
        token_env: "COMPUTE_TEST_HETZNER_TOKEN".into(),
        api_url: Some(api.clone()),
    }
    .build()
    .unwrap();
    assert!(provider.lookup("www", "A").await.unwrap().is_empty());
    provider.apply("www", "A", "192.0.2.10", 300).await.unwrap();
    let found = provider.lookup("www", "A").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].value, "192.0.2.10");
    assert_eq!(found[0].ttl, 300);

    // Drift: someone changed the record at the provider.
    records
        .lock()
        .unwrap()
        .get_mut(&("www".into(), "A".into()))
        .unwrap()["records"] = json!([{ "value": "198.51.100.1" }, { "value": "198.51.100.2" }]);
    assert_eq!(provider.lookup("www", "A").await.unwrap().len(), 2);
    provider.apply("www", "A", "192.0.2.10", 600).await.unwrap();
    let found = provider.lookup("www", "A").await.unwrap();
    assert_eq!(
        found
            .iter()
            .map(|value| value.value.as_str())
            .collect::<Vec<_>>(),
        vec!["192.0.2.10"]
    );
    assert_eq!(found[0].ttl, 600);
    provider.remove("www", "A").await.unwrap();
    assert!(provider.lookup("www", "A").await.unwrap().is_empty());
    provider.remove("www", "A").await.unwrap();

    // A wrong token is a provider error, not a panic or a silent success.
    unsafe { std::env::set_var("COMPUTE_TEST_HETZNER_WRONG", "nope") };
    let wrong = DnsProviderConfig::Hetzner {
        zone: "example.com".into(),
        token_env: "COMPUTE_TEST_HETZNER_WRONG".into(),
        api_url: Some(api),
    }
    .build()
    .unwrap();
    let error = wrong
        .apply("www", "A", "192.0.2.10", 300)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("401"), "{error}");
    assert!(
        !error.to_string().contains("nope"),
        "the token never appears in errors"
    );
}

#[tokio::test]
async fn cloudflare_keeps_exactly_one_record() {
    let records = Arc::new(Mutex::new(BTreeMap::new()));
    let api = serve(cloudflare(records.clone())).await;
    unsafe { std::env::set_var("COMPUTE_TEST_CLOUDFLARE_TOKEN", "cloudflare-token") };
    let provider = DnsProviderConfig::Cloudflare {
        zone: "example.com".into(),
        token_env: "COMPUTE_TEST_CLOUDFLARE_TOKEN".into(),
        api_url: Some(api),
    }
    .build()
    .unwrap();
    provider.apply("@", "A", "192.0.2.20", 300).await.unwrap();
    provider.apply("api", "A", "192.0.2.21", 300).await.unwrap();
    // A duplicate appears at the provider.
    records.lock().unwrap().insert(
        "rec-x".into(),
        json!({ "id": "rec-x", "type": "A", "name": "api.example.com", "content": "203.0.113.9", "ttl": 300 }),
    );
    assert_eq!(provider.lookup("api", "A").await.unwrap().len(), 2);
    provider.apply("api", "A", "192.0.2.21", 300).await.unwrap();
    let found = provider.lookup("api", "A").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].value, "192.0.2.21");
    assert_eq!(
        provider.lookup("@", "A").await.unwrap()[0].value,
        "192.0.2.20"
    );
    provider.remove("api", "A").await.unwrap();
    assert!(provider.lookup("api", "A").await.unwrap().is_empty());
    assert_eq!(records.lock().unwrap().len(), 1, "only the apex remains");
}
