//! The network control plane: domains, DNS records, certificates, and the
//! ingress routes that follow them.
//!
//! Each resource reports what is wanted, what is, its status, the last
//! error, and when it was last reconciled. A domain belongs to one
//! environment and routes to one workload port of one project there; the
//! ingress routes a host only to that endpoint.
//!
//! Secrets stay out of control state: DNS credentials come from the
//! environment variables configuration names, and certificate keys are in
//! the node's secret store, referenced by `secret_reference`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use compute_network::acme::{self, StoredCertificate};
use compute_network::dns::relative_name;
use compute_network::{Ingress, IngressRoute};
use compute_state::events;
use compute_state::{
    CertificateRecord, DnsRecordRecord, DomainRecord, Reconciliation, Stored, ids,
};
use serde_json::json;

use super::{Change, Daemon, NetworkConfig, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// How soon a DNS record that is not yet healthy is checked again.
const DNS_RETRY: Duration = Duration::from_secs(5);

/// Live network state the daemon keeps in memory.
#[derive(Default)]
pub(crate) struct NetworkRuntime {
    pub readiness_checked: BTreeMap<String, Instant>,
    pub dns_checked: BTreeMap<String, Instant>,
    pub dns_forced: bool,
    pub issuing: BTreeSet<String>,
    pub certificate_attempted: BTreeMap<String, Instant>,
    pub certificate_forced: BTreeSet<String>,
    /// Certificates loaded into the ingress: domain → fingerprint.
    pub loaded: BTreeMap<String, String>,
}

/// Start the node's ingress listeners, when configured.
pub(crate) async fn start_ingress(
    config: &NetworkConfig,
) -> Result<(Option<Arc<Ingress>>, Option<SocketAddr>, Option<SocketAddr>), EnvironmentError> {
    if config.ingress_http.is_none() && config.ingress_https.is_none() {
        return Ok((None, None, None));
    }
    let ingress = Arc::new(Ingress::new());
    let listen = |address: SocketAddr, what: &str, error: std::io::Error| {
        EnvironmentError::Invalid(format!(
            "ingress {what} cannot listen on {address}: {error}"
        ))
    };
    let http = match config.ingress_http {
        Some(address) => Some(
            ingress
                .listen_http(address)
                .await
                .map_err(|error| listen(address, "HTTP", error))?,
        ),
        None => None,
    };
    let https = match config.ingress_https {
        Some(address) => Some(
            ingress
                .listen_https(address)
                .await
                .map_err(|error| listen(address, "HTTPS", error))?,
        ),
        None => None,
    };
    Ok((Some(ingress), http, https))
}

/// A DNS name Compute will manage: lowercase letters, digits, and
/// hyphens, in at least two labels.
fn validate_domain(name: &str) -> Result<String, EnvironmentError> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let labels = name.split('.').collect::<Vec<_>>();
    let valid = name.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        });
    if !valid {
        return Err(EnvironmentError::Invalid(format!(
            "{name:?} is not a domain name Compute can manage"
        )));
    }
    Ok(name)
}

fn reconciliation(status: &str) -> Reconciliation {
    Reconciliation {
        status: status.into(),
        ..Reconciliation::default()
    }
}

fn same_state(left: &Reconciliation, right: &Reconciliation) -> bool {
    left.status == right.status
        && left.desired == right.desired
        && left.actual == right.actual
        && left.last_error == right.last_error
}

impl Daemon {
    // ---- Domains ------------------------------------------------------------

    pub async fn add_domain(
        self: &Arc<Self>,
        definition: DomainDefinition,
    ) -> Result<DomainView, EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        let name = validate_domain(&definition.name)?;
        self.refresh().await?;
        let desired = self.inner.lock().await.desired.clone();
        if desired.domains.contains_key(&name) {
            return Err(EnvironmentError::Conflict(format!(
                "domain {name} already exists; a domain routes to exactly one place"
            )));
        }
        let environment = desired
            .environment(&definition.environment)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!("environment {}", definition.environment))
            })?;
        let env_name = environment.value.name.clone();
        let membership = desired
            .memberships
            .get(&(env_name.clone(), definition.project.clone()))
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "project {} in {env_name}; a domain routes only within its environment",
                    definition.project
                ))
            })?;
        let deployment_id = membership.value.deployment_id.clone().ok_or_else(|| {
            EnvironmentError::Conflict(format!(
                "{} has not been released to {env_name} yet",
                definition.project
            ))
        })?;
        let revision = desired.revision_of(&deployment_id).ok_or_else(|| {
            EnvironmentError::NotFound(format!("the revision of {deployment_id}"))
        })?;
        let services = revision
            .workloads
            .iter()
            .filter(|workload| workload.kind == WorkloadKind::Service && !workload.ports.is_empty())
            .collect::<Vec<_>>();
        let workload = match &definition.workload {
            Some(name) => services
                .iter()
                .find(|workload| workload.name == *name)
                .copied()
                .ok_or_else(|| {
                    EnvironmentError::Invalid(format!(
                        "{name} is not a service with ports in {}",
                        definition.project
                    ))
                })?,
            None if services.len() == 1 => services[0],
            None => {
                return Err(EnvironmentError::Invalid(format!(
                    "{} has {} services with ports; name the workload",
                    definition.project,
                    services.len()
                )));
            }
        };
        let port = match &definition.port {
            Some(port) => workload
                .ports
                .iter()
                .find(|declared| declared.name == *port)
                .ok_or_else(|| {
                    EnvironmentError::Invalid(format!("{} declares no port {port}", workload.name))
                })?,
            None => &workload.ports[0],
        };
        // The DNS provider: named, or the one whose zone holds the domain.
        let provider = match &definition.dns_provider {
            Some(provider) if provider == "none" => None,
            Some(provider) => {
                let config = self.config.network.dns.get(provider).ok_or_else(|| {
                    EnvironmentError::Invalid(format!("no DNS provider {provider} is configured"))
                })?;
                relative_name(&name, config.zone())
                    .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
                Some((provider.clone(), config.zone().to_string()))
            }
            None => self
                .config
                .network
                .dns
                .iter()
                .filter(|(_, config)| relative_name(&name, config.zone()).is_ok())
                .max_by_key(|(_, config)| config.zone().len())
                .map(|(provider, config)| (provider.clone(), config.zone().to_string())),
        };
        let addresses = [
            ("A", self.config.network.public_ipv4.clone()),
            ("AAAA", self.config.network.public_ipv6.clone()),
        ]
        .into_iter()
        .filter_map(|(record_type, value)| value.map(|value| (record_type, value)))
        .collect::<Vec<_>>();
        if provider.is_some() && addresses.is_empty() {
            return Err(EnvironmentError::Invalid(
                "DNS records need the node's public address: set network.public_ipv4 or public_ipv6"
                    .into(),
            ));
        }
        let tls = definition.tls.unwrap_or(self.config.network.acme.is_some());
        if tls && self.config.network.acme.is_none() {
            return Err(EnvironmentError::Invalid(
                "TLS needs certificate issuance: configure network.acme".into(),
            ));
        }
        let now = Utc::now();
        let domain_id = ids::domain(&name);
        let certificate_id = tls.then(|| ids::certificate(&name));
        let record = DomainRecord {
            name: name.clone(),
            environment_id: environment.id.clone(),
            environment: env_name.clone(),
            project_id: membership.value.project_id.clone(),
            project: definition.project.clone(),
            workload: workload.name.clone(),
            port: port.name.clone(),
            dns_provider: provider
                .as_ref()
                .map_or_else(|| "none".into(), |(provider, _)| provider.clone()),
            certificate_id: certificate_id.clone(),
            status: "pending".into(),
            dns: reconciliation(if provider.is_some() {
                "pending"
            } else {
                "unmanaged"
            }),
            tls: reconciliation(if tls { "pending" } else { "disabled" }),
            routing: reconciliation("pending"),
            created_at: now,
        };
        let mut change = Change::new().with(|batch| batch.create(&domain_id, &record));
        if let Some((provider, zone)) = &provider {
            let relative = relative_name(&name, zone)
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
            for (record_type, value) in &addresses {
                let dns = DnsRecordRecord {
                    domain: name.clone(),
                    provider: provider.clone(),
                    zone: zone.clone(),
                    name: relative.clone(),
                    record_type: (*record_type).into(),
                    value: value.clone(),
                    ttl: 300,
                    provider_record_id: None,
                    state: Reconciliation {
                        status: "pending".into(),
                        desired: Some(value.clone()),
                        ..Reconciliation::default()
                    },
                };
                change =
                    change.with(|batch| batch.create(&ids::dns_record(&name, record_type), &dns));
            }
        }
        if let (Some(certificate_id), Some(acme)) = (&certificate_id, &self.config.network.acme) {
            let certificate = CertificateRecord {
                domain: name.clone(),
                issuer: acme.directory.clone(),
                status: "pending".into(),
                renewal_status: "not_due".into(),
                not_before: None,
                expires_at: None,
                fingerprint: None,
                secret_reference: None,
                held_by: None,
                last_error: None,
                last_reconciled_at: None,
            };
            change = change.with(|batch| batch.create(certificate_id, &certificate));
        }
        let endpoint = ids::endpoint(&env_name, &definition.project, &workload.name, &port.name);
        if let Some(assignment) = desired.traffic.get(&endpoint) {
            change = change.with(|batch| {
                batch.update(
                    assignment,
                    json!({ "domains": domain_names(&desired.domains, &endpoint, Some(&name), None) }),
                )
            });
        }
        let change = self.event(
            change,
            events::DOMAIN_CREATED,
            Scope::project(&env_name, &definition.project),
            format!("{name} routes to {endpoint}"),
            json!({ "domain": name, "endpoint": endpoint, "dns_provider": record.dns_provider, "tls": tls }),
        );
        self.apply(change).await?;
        drop(cycle);
        self.changed().await;
        self.domain(&name).await
    }

    /// Remove a domain: its route, its DNS records at the provider, and
    /// its certificate.
    pub async fn remove_domain(self: &Arc<Self>, name: &str) -> Result<(), EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        let name = name.to_ascii_lowercase();
        self.refresh().await?;
        let desired = self.inner.lock().await.desired.clone();
        let domain = desired
            .domains
            .get(&name)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("domain {name}")))?;
        let mut change = Change::new().with(|batch| batch.delete(&domain));
        let mut cleanup_errors = vec![];
        for record in desired
            .dns_records
            .values()
            .filter(|record| record.value.domain == name)
        {
            match self.dns.get(&record.value.provider) {
                Some(Ok(provider)) => {
                    if let Err(error) = provider
                        .remove(&record.value.name, &record.value.record_type)
                        .await
                    {
                        cleanup_errors.push(format!("{}: {error}", record.value.record_type));
                    }
                }
                Some(Err(error)) => cleanup_errors.push(error.clone()),
                None => cleanup_errors.push(format!(
                    "DNS provider {} is not configured",
                    record.value.provider
                )),
            }
            change = change.with(|batch| batch.delete(record));
        }
        if let Some(certificate) = desired.certificates.get(&name) {
            if let Some(reference) = &certificate.value.secret_reference {
                let _ = self.secrets.remove(reference);
            }
            change = change.with(|batch| batch.delete(certificate));
        }
        let endpoint = ids::endpoint(
            &domain.value.environment,
            &domain.value.project,
            &domain.value.workload,
            &domain.value.port,
        );
        if let Some(assignment) = desired.traffic.get(&endpoint) {
            change = change.with(|batch| {
                batch.update(
                    assignment,
                    json!({ "domains": domain_names(&desired.domains, &endpoint, None, Some(&name)) }),
                )
            });
        }
        let change = self.event(
            change,
            events::DOMAIN_REMOVED,
            Scope::project(&domain.value.environment, &domain.value.project),
            format!("{name} no longer routes to {endpoint}"),
            json!({ "domain": name, "dns_cleanup_errors": cleanup_errors }),
        );
        self.apply(change).await?;
        if let Some(ingress) = &self.ingress {
            ingress.remove_certificate(&name);
        }
        self.inner.lock().await.network.loaded.remove(&name);
        drop(cycle);
        self.changed().await;
        Ok(())
    }

    pub async fn domains(&self) -> Result<Vec<DomainView>, EnvironmentError> {
        self.refresh().await?;
        let names = self
            .inner
            .lock()
            .await
            .desired
            .domains
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut views = vec![];
        for name in names {
            views.push(self.domain_view(&name).await?);
        }
        Ok(views)
    }

    pub async fn domain(&self, name: &str) -> Result<DomainView, EnvironmentError> {
        self.refresh().await?;
        self.domain_view(&name.to_ascii_lowercase()).await
    }

    async fn domain_view(&self, name: &str) -> Result<DomainView, EnvironmentError> {
        let inner = self.inner.lock().await;
        let desired = &inner.desired;
        let domain = desired
            .domains
            .get(name)
            .ok_or_else(|| EnvironmentError::NotFound(format!("domain {name}")))?;
        let endpoint = ids::endpoint(
            &domain.value.environment,
            &domain.value.project,
            &domain.value.workload,
            &domain.value.port,
        );
        let assignment = desired.traffic.get(&endpoint);
        Ok(DomainView {
            domain_id: domain.id.clone(),
            record: domain.value.clone(),
            endpoint,
            host_port: assignment.map(|assignment| assignment.value.host_port),
            serving_revision: assignment.map(|assignment| assignment.value.revision.clone()),
            serving_deployment: assignment.map(|assignment| assignment.value.deployment_id.clone()),
            dns_records: desired
                .dns_records
                .values()
                .filter(|record| record.value.domain == name)
                .map(|record| DnsRecordView {
                    record_id: record.id.clone(),
                    record: record.value.clone(),
                })
                .collect(),
            certificate: desired
                .certificates
                .get(name)
                .map(|certificate| certificate_view(certificate, &self.node_id)),
        })
    }

    pub async fn dns_status(&self) -> Result<Vec<DnsRecordView>, EnvironmentError> {
        self.refresh().await?;
        Ok(self
            .inner
            .lock()
            .await
            .desired
            .dns_records
            .values()
            .map(|record| DnsRecordView {
                record_id: record.id.clone(),
                record: record.value.clone(),
            })
            .collect())
    }

    /// Read every DNS record back from its provider now, and repair drift.
    pub async fn reconcile_dns(self: &Arc<Self>) -> Result<Vec<DnsRecordView>, EnvironmentError> {
        self.inner.lock().await.network.dns_forced = true;
        self.changed().await;
        self.dns_status().await
    }

    pub async fn certificates(&self) -> Result<Vec<CertificateView>, EnvironmentError> {
        self.refresh().await?;
        Ok(self
            .inner
            .lock()
            .await
            .desired
            .certificates
            .values()
            .map(|certificate| certificate_view(certificate, &self.node_id))
            .collect())
    }

    /// Renew a domain's certificate now, whatever its expiry.
    pub async fn renew_certificate(
        self: &Arc<Self>,
        domain: &str,
    ) -> Result<CertificateView, EnvironmentError> {
        let domain = domain.to_ascii_lowercase();
        self.refresh().await?;
        let exists = self
            .inner
            .lock()
            .await
            .desired
            .certificates
            .contains_key(&domain);
        if !exists {
            return Err(EnvironmentError::NotFound(format!(
                "a certificate for {domain}"
            )));
        }
        {
            let mut inner = self.inner.lock().await;
            inner.network.certificate_forced.insert(domain.clone());
            inner.network.certificate_attempted.remove(&domain);
        }
        self.changed().await;
        let certificates = self.certificates().await?;
        certificates
            .into_iter()
            .find(|certificate| certificate.record.domain == domain)
            .ok_or_else(|| EnvironmentError::NotFound(format!("a certificate for {domain}")))
    }

    pub async fn network_status(&self) -> NetworkStatus {
        // What the data plane serves right now.
        let served = self
            .data_plane()
            .routes()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|route| (route.port, route))
            .collect::<std::collections::BTreeMap<_, _>>();
        let inner = self.inner.lock().await;
        NetworkStatus {
            endpoint_address: self.config.network.endpoint_address.to_string(),
            ingress_http: self.ingress_http.map(|address| address.to_string()),
            ingress_https: self.ingress_https.map(|address| address.to_string()),
            public_ipv4: self.config.network.public_ipv4.clone(),
            public_ipv6: self.config.network.public_ipv6.clone(),
            dns_providers: self
                .config
                .network
                .dns
                .iter()
                .map(|(name, config)| DnsProviderView {
                    name: name.clone(),
                    zone: config.zone().to_string(),
                    kind: match config {
                        compute_network::dns::DnsProviderConfig::Hetzner { .. } => "hetzner",
                        compute_network::dns::DnsProviderConfig::Cloudflare { .. } => "cloudflare",
                        compute_network::dns::DnsProviderConfig::File { .. } => "file",
                    }
                    .into(),
                    error: self
                        .dns
                        .get(name)
                        .and_then(|provider| provider.as_ref().err().cloned()),
                })
                .collect(),
            acme_directory: self
                .config
                .network
                .acme
                .as_ref()
                .map(|acme| acme.directory.clone()),
            node_id: self.node_id.clone(),
            endpoints: inner
                .desired
                .traffic
                .values()
                .map(|assignment| EndpointView {
                    endpoint: assignment.value.endpoint.clone(),
                    host_port: assignment.value.host_port,
                    instance_id: assignment.value.instance_id.clone(),
                    target_port: assignment.value.target_port,
                    revision: assignment.value.revision.clone(),
                    listening: served
                        .get(&assignment.value.host_port)
                        .is_some_and(|route| route.listening),
                    open_connections: served
                        .get(&assignment.value.host_port)
                        .map(|route| route.open_connections)
                        .unwrap_or(0),
                    error: inner
                        .endpoint_errors
                        .get(&assignment.value.host_port)
                        .cloned(),
                })
                .collect(),
        }
    }

    // ---- Reconciliation -----------------------------------------------------

    /// Converge routes, DNS, and certificates, and record each domain's
    /// state.
    pub(crate) async fn reconcile_network(self: &Arc<Self>) {
        let desired = self.inner.lock().await.desired.clone();
        if desired.domains.is_empty() && desired.dns_records.is_empty() {
            if let Some(ingress) = &self.ingress {
                ingress.set_routes(BTreeMap::new());
            }
            return;
        }
        // Routes.
        let routes = desired
            .domains
            .values()
            .filter_map(|domain| {
                let endpoint = ids::endpoint(
                    &domain.value.environment,
                    &domain.value.project,
                    &domain.value.workload,
                    &domain.value.port,
                );
                desired.traffic.get(&endpoint).map(|assignment| {
                    (
                        domain.value.name.clone(),
                        IngressRoute {
                            endpoint_port: assignment.value.host_port,
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(ingress) = &self.ingress {
            ingress.set_routes(routes.clone());
        }
        let mut change = Change::new();
        change = self
            .reconcile_dns_records(change, &desired.dns_records)
            .await;
        change = self
            .reconcile_certificates(change, &desired.certificates)
            .await;
        if !change.batch.is_empty() {
            if let Err(error) = self.apply(change).await {
                self.inner.lock().await.state_error = Some(error.to_string());
                return;
            }
            if self.refresh().await.is_err() {
                return;
            }
        }
        // Each domain's own state, from its parts.
        let desired = self.inner.lock().await.desired.clone();
        let mut change = Change::new();
        let now = Utc::now();
        for domain in desired.domains.values() {
            let name = &domain.value.name;
            let endpoint = ids::endpoint(
                &domain.value.environment,
                &domain.value.project,
                &domain.value.workload,
                &domain.value.port,
            );
            let routing = match (desired.traffic.get(&endpoint), &self.ingress) {
                (Some(assignment), Some(_)) => Reconciliation {
                    status: "healthy".into(),
                    desired: Some(endpoint.clone()),
                    actual: Some(format!(
                        "port {} → {} ({})",
                        assignment.value.host_port,
                        assignment.value.instance_id,
                        assignment.value.revision
                    )),
                    last_error: None,
                    last_reconciled_at: Some(now),
                },
                (Some(assignment), None) => Reconciliation {
                    status: "degraded".into(),
                    desired: Some(endpoint.clone()),
                    actual: Some(format!("port {}", assignment.value.host_port)),
                    last_error: Some("ingress is not configured on this node".into()),
                    last_reconciled_at: Some(now),
                },
                (None, _) => Reconciliation {
                    status: "pending".into(),
                    desired: Some(endpoint.clone()),
                    actual: None,
                    last_error: Some(format!("nothing serves {endpoint} yet")),
                    last_reconciled_at: Some(now),
                },
            };
            let records = desired
                .dns_records
                .values()
                .filter(|record| record.value.domain == *name)
                .collect::<Vec<_>>();
            let dns = if records.is_empty() {
                Reconciliation {
                    status: "unmanaged".into(),
                    last_reconciled_at: Some(now),
                    ..Reconciliation::default()
                }
            } else {
                let failed = records
                    .iter()
                    .find(|record| record.value.state.status == "failed");
                let healthy = records
                    .iter()
                    .all(|record| record.value.state.status == "healthy");
                Reconciliation {
                    status: if failed.is_some() {
                        "failed"
                    } else if healthy {
                        "healthy"
                    } else {
                        "pending"
                    }
                    .into(),
                    desired: Some(
                        records
                            .iter()
                            .map(|record| {
                                format!("{} {}", record.value.record_type, record.value.value)
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    actual: Some(
                        records
                            .iter()
                            .map(|record| {
                                format!(
                                    "{} {}",
                                    record.value.record_type,
                                    record
                                        .value
                                        .state
                                        .actual
                                        .clone()
                                        .unwrap_or_else(|| "-".into())
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    last_error: failed.and_then(|record| record.value.state.last_error.clone()),
                    last_reconciled_at: Some(now),
                }
            };
            let tls = match desired.certificates.get(name) {
                None => Reconciliation {
                    status: "disabled".into(),
                    last_reconciled_at: Some(now),
                    ..Reconciliation::default()
                },
                Some(certificate) => {
                    let served = self
                        .ingress
                        .as_ref()
                        .and_then(|ingress| ingress.certificate(name));
                    Reconciliation {
                        status: match certificate.value.status.as_str() {
                            "valid" if served.is_some() => "healthy",
                            "valid" => "degraded",
                            "failed" | "expired" => "failed",
                            _ => "pending",
                        }
                        .into(),
                        desired: Some(format!(
                            "a valid certificate from {}",
                            certificate.value.issuer
                        )),
                        actual: certificate.value.expires_at.map(|expires| {
                            format!(
                                "{} until {}",
                                certificate.value.status,
                                expires.format("%Y-%m-%d")
                            )
                        }),
                        last_error: certificate.value.last_error.clone().or_else(|| {
                            (certificate.value.status == "valid" && served.is_none())
                                .then(|| "this node does not hold the certificate's key yet".into())
                        }),
                        last_reconciled_at: Some(now),
                    }
                }
            };
            let parts = [&dns.status, &tls.status, &routing.status];
            let status = if parts.iter().any(|status| *status == "failed") {
                "failed"
            } else if parts
                .iter()
                .all(|status| matches!(status.as_str(), "healthy" | "unmanaged" | "disabled"))
            {
                "healthy"
            } else if parts.iter().any(|status| *status == "degraded") {
                "degraded"
            } else {
                "pending"
            };
            if domain.value.status != status
                || !same_state(&domain.value.dns, &dns)
                || !same_state(&domain.value.tls, &tls)
                || !same_state(&domain.value.routing, &routing)
            {
                change = change.with(|batch| {
                    batch.update(
                        domain,
                        json!({ "status": status, "dns": dns, "tls": tls, "routing": routing }),
                    )
                });
            }
        }
        if !change.batch.is_empty() && self.apply(change).await.is_ok() {
            let _ = self.refresh().await;
        }
    }

    async fn reconcile_dns_records(
        self: &Arc<Self>,
        mut change: Change,
        records: &BTreeMap<String, Stored<DnsRecordRecord>>,
    ) -> Change {
        let now = Instant::now();
        let (forced, checked) = {
            let mut inner = self.inner.lock().await;
            let forced = std::mem::take(&mut inner.network.dns_forced);
            (forced, inner.network.dns_checked.clone())
        };
        for record in records.values() {
            let interval = if record.value.state.status == "healthy" {
                self.config.network.dns_interval
            } else {
                DNS_RETRY
            };
            let due = forced
                || checked
                    .get(&record.id)
                    .is_none_or(|last| now.duration_since(*last) >= interval);
            if !due {
                continue;
            }
            self.inner
                .lock()
                .await
                .network
                .dns_checked
                .insert(record.id.clone(), now);
            let value = &record.value;
            let scope = Scope::default();
            let mut state = Reconciliation {
                status: "healthy".into(),
                desired: Some(value.value.clone()),
                actual: None,
                last_error: None,
                last_reconciled_at: Some(Utc::now()),
            };
            let mut provider_record_id = value.provider_record_id.clone();
            let outcome = match self.dns.get(&value.provider) {
                None => Err(format!("DNS provider {} is not configured", value.provider)),
                Some(Err(error)) => Err(error.clone()),
                Some(Ok(provider)) => {
                    match provider.lookup(&value.name, &value.record_type).await {
                        Err(error) => Err(error.to_string()),
                        Ok(found) => {
                            let actual = found
                                .iter()
                                .map(|found| found.value.clone())
                                .collect::<Vec<_>>();
                            let correct = actual == [value.value.clone()]
                                && found.first().is_some_and(|found| found.ttl == value.ttl);
                            if correct {
                                state.actual = Some(value.value.clone());
                                Ok(None)
                            } else {
                                let before = if actual.is_empty() {
                                    "nothing".to_string()
                                } else {
                                    actual.join(", ")
                                };
                                match provider
                                    .apply(&value.name, &value.record_type, &value.value, value.ttl)
                                    .await
                                {
                                    Ok(applied) => {
                                        state.actual = Some(applied.value.clone());
                                        provider_record_id = applied.id.or(provider_record_id);
                                        Ok(Some(before))
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                        }
                    }
                }
            };
            match outcome {
                Ok(repaired) => {
                    if let Some(before) = repaired {
                        // A record that was right before and is wrong now
                        // drifted; one never applied was just created.
                        if value.state.status == "healthy" || value.provider_record_id.is_some() {
                            change = self.event(
                                change,
                                events::DNS_DRIFTED,
                                scope.clone(),
                                format!(
                                    "{} {} drifted to {before}; restored {}",
                                    value.domain, value.record_type, value.value
                                ),
                                json!({ "domain": value.domain, "record_type": value.record_type, "expected": value.value, "found": before }),
                            );
                        }
                        change = self.event(
                            change,
                            events::DNS_APPLIED,
                            scope,
                            format!(
                                "{} {} → {} at {}",
                                value.domain, value.record_type, value.value, value.provider
                            ),
                            json!({ "domain": value.domain, "record_type": value.record_type, "value": value.value, "provider": value.provider }),
                        );
                    }
                }
                Err(error) => {
                    state.status = "failed".into();
                    state.last_error = Some(error.clone());
                    if value.state.status != "failed" {
                        change = self.event(
                            change,
                            events::DNS_FAILED,
                            scope,
                            format!(
                                "{} {} could not be reconciled at {}: {error}",
                                value.domain, value.record_type, value.provider
                            ),
                            json!({ "domain": value.domain, "record_type": value.record_type, "error": error }),
                        );
                    }
                }
            }
            let mut updated = value.clone();
            updated.state = state;
            updated.provider_record_id = provider_record_id;
            change = change.with(|batch| batch.replace(record, &updated));
        }
        change
    }

    async fn reconcile_certificates(
        self: &Arc<Self>,
        mut change: Change,
        certificates: &BTreeMap<String, Stored<CertificateRecord>>,
    ) -> Change {
        let Some(acme_config) = self.config.network.acme.clone() else {
            return change;
        };
        let now = Utc::now();
        for certificate in certificates.values() {
            let value = &certificate.value;
            let domain = value.domain.clone();
            // Serve what this node holds.
            let held = value
                .secret_reference
                .as_deref()
                .and_then(|reference| StoredCertificate::load(&self.secrets, reference));
            if let (Some(stored), Some(ingress), Some(fingerprint)) =
                (&held, &self.ingress, &value.fingerprint)
            {
                let loaded = self.inner.lock().await.network.loaded.get(&domain).cloned();
                if loaded.as_ref() != Some(fingerprint) {
                    match ingress.set_certificate(
                        &domain,
                        stored.chain_pem.as_bytes(),
                        stored.key_pem.as_bytes(),
                    ) {
                        Ok(()) => {
                            self.inner
                                .lock()
                                .await
                                .network
                                .loaded
                                .insert(domain.clone(), fingerprint.clone());
                        }
                        Err(error) => {
                            change = change.with(|batch| {
                                batch.update(
                                    certificate,
                                    json!({ "last_error": format!("cannot serve the certificate: {error}") }),
                                )
                            });
                        }
                    }
                }
            }
            let expired = value.expires_at.is_some_and(|expires| expires <= now);
            let due = value.expires_at.is_some_and(|expires| {
                expires - chrono::TimeDelta::days(i64::from(acme_config.renew_before_days)) <= now
            });
            let (forced, issuing, attempted) = {
                let inner = self.inner.lock().await;
                (
                    inner.network.certificate_forced.contains(&domain),
                    inner.network.issuing.contains(&domain),
                    inner.network.certificate_attempted.get(&domain).copied(),
                )
            };
            if expired && value.status != "expired" {
                change = change.with(|batch| {
                    batch.update(
                        certificate,
                        json!({ "status": "expired", "last_reconciled_at": now }),
                    )
                });
            } else if due && value.renewal_status == "not_due" {
                change = change.with(|batch| {
                    batch.update(
                        certificate,
                        json!({ "renewal_status": "due", "last_reconciled_at": now }),
                    )
                });
            }
            let wanted = forced || value.status == "pending" || held.is_none() || due || expired;
            let backing_off = !forced
                && attempted.is_some_and(|attempted| {
                    attempted.elapsed() < self.config.network.certificate_retry
                });
            if !wanted || issuing || backing_off {
                continue;
            }
            let Some(ingress) = self.ingress.clone() else {
                if value.last_error.is_none() {
                    change = change.with(|batch| {
                        batch.update(
                            certificate,
                            json!({ "status": "failed", "last_error": "HTTP-01 needs ingress HTTP on this node (network.ingress_http)", "last_reconciled_at": now }),
                        )
                    });
                }
                continue;
            };
            if self.ingress_http.is_none() {
                if value.last_error.is_none() {
                    change = change.with(|batch| {
                        batch.update(
                            certificate,
                            json!({ "status": "failed", "last_error": "HTTP-01 needs ingress HTTP on this node (network.ingress_http)", "last_reconciled_at": now }),
                        )
                    });
                }
                continue;
            }
            {
                let mut inner = self.inner.lock().await;
                inner.network.issuing.insert(domain.clone());
                inner.network.certificate_forced.remove(&domain);
                inner
                    .network
                    .certificate_attempted
                    .insert(domain.clone(), Instant::now());
            }
            let renewing = value.status == "valid" || value.fingerprint.is_some();
            change = change.with(|batch| {
                batch.update(
                    certificate,
                    if renewing {
                        json!({ "renewal_status": "renewing", "last_reconciled_at": now })
                    } else {
                        json!({ "status": "issuing", "last_reconciled_at": now })
                    },
                )
            });
            let daemon = Arc::downgrade(self);
            let acme_config = acme_config.clone();
            tokio::spawn(async move {
                let Some(owner) = daemon.upgrade() else {
                    return;
                };
                let result =
                    acme::issue(&acme_config, &owner.secrets, &domain, ingress.as_ref()).await;
                owner.certificate_issued(&domain, renewing, result).await;
            });
        }
        change
    }

    async fn certificate_issued(
        self: &Arc<Self>,
        domain: &str,
        renewing: bool,
        result: Result<acme::Issued, acme::AcmeError>,
    ) {
        let _cycle = self.reconciling.lock().await;
        self.inner.lock().await.network.issuing.remove(domain);
        let Ok(Some(certificate)) = self
            .control()
            .get::<CertificateRecord>(&ids::certificate(domain))
            .await
        else {
            // The domain went away while its certificate was issued.
            if let Ok(issued) = result {
                let _ = self.secrets.remove(&issued.secret_reference);
            }
            return;
        };
        let now = Utc::now();
        let scope = Scope::default();
        let change = match result {
            Ok(issued) => {
                let previous = certificate.value.secret_reference.clone();
                let updated = CertificateRecord {
                    status: "valid".into(),
                    renewal_status: "not_due".into(),
                    not_before: Some(issued.not_before),
                    expires_at: Some(issued.not_after),
                    fingerprint: Some(issued.fingerprint.clone()),
                    secret_reference: Some(issued.secret_reference.clone()),
                    held_by: Some(self.node_id.clone()),
                    last_error: None,
                    last_reconciled_at: Some(now),
                    ..certificate.value.clone()
                };
                let change = Change::new().with(|batch| batch.replace(&certificate, &updated));
                if let Some(previous) = previous
                    && previous != issued.secret_reference
                {
                    let _ = self.secrets.remove(&previous);
                }
                self.event(
                    change,
                    if renewing {
                        events::CERTIFICATE_RENEWED
                    } else {
                        events::CERTIFICATE_ISSUED
                    },
                    scope,
                    format!(
                        "certificate for {domain} {} until {}",
                        if renewing { "renewed" } else { "issued" },
                        issued.not_after.format("%Y-%m-%d")
                    ),
                    json!({ "domain": domain, "fingerprint": issued.fingerprint, "expires_at": issued.not_after }),
                )
            }
            Err(error) => {
                let message = error.to_string();
                let still_valid = certificate.value.status == "valid"
                    && certificate
                        .value
                        .expires_at
                        .is_some_and(|expires| expires > now);
                let change = Change::new().with(|batch| {
                    batch.update(
                        &certificate,
                        if still_valid {
                            json!({ "renewal_status": "failed", "last_error": message, "last_reconciled_at": now })
                        } else {
                            json!({ "status": "failed", "last_error": message, "last_reconciled_at": now })
                        },
                    )
                });
                self.event(
                    change,
                    events::CERTIFICATE_FAILED,
                    scope,
                    format!("certificate for {domain} could not be issued: {message}"),
                    json!({ "domain": domain, "error": message }),
                )
            }
        };
        if let Err(error) = self.apply(change).await {
            self.inner.lock().await.state_error = Some(error.to_string());
        }
        self.wake();
    }
}

/// The domains routed to an endpoint, after adding or removing one.
fn domain_names(
    domains: &BTreeMap<String, Stored<DomainRecord>>,
    endpoint: &str,
    add: Option<&str>,
    remove: Option<&str>,
) -> Vec<String> {
    let mut names = domains
        .values()
        .filter(|domain| {
            ids::endpoint(
                &domain.value.environment,
                &domain.value.project,
                &domain.value.workload,
                &domain.value.port,
            ) == endpoint
        })
        .map(|domain| domain.value.name.clone())
        .filter(|name| Some(name.as_str()) != remove)
        .collect::<BTreeSet<_>>();
    if let Some(add) = add {
        names.insert(add.to_string());
    }
    names.into_iter().collect()
}

fn certificate_view(certificate: &Stored<CertificateRecord>, node_id: &str) -> CertificateView {
    CertificateView {
        certificate_id: certificate.id.clone(),
        held_here: certificate.value.held_by.as_deref() == Some(node_id),
        record: certificate.value.clone(),
    }
}
