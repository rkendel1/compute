//! `compute domain`, `compute dns`, `compute certificate`, and
//! `compute network`: the network control plane, through the Compute API.

use clap::{Args, Subcommand};
use compute_environment::{
    CertificateView, DnsRecordView, DomainDefinition, DomainView, NetworkStatus,
};

use crate::environment_cmd::{DaemonLocation, error, print_json};

#[derive(Args, Debug)]
pub struct DomainCommand {
    #[command(subcommand)]
    pub command: DomainCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum DomainCommands {
    /// Route a domain to a project's service in one environment.
    Add {
        domain: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        project: String,
        /// The service; optional when the project has one with ports.
        #[arg(long)]
        workload: Option<String>,
        /// The port name; defaults to the service's first.
        #[arg(long)]
        port: Option<String>,
        /// A configured DNS provider, or `none` to manage DNS yourself.
        #[arg(long)]
        dns_provider: Option<String>,
        /// Serve plain HTTP only, even when ACME is configured.
        #[arg(long)]
        no_tls: bool,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        domain: String,
        #[arg(long)]
        json: bool,
    },
    /// DNS, TLS, and routing state of each domain.
    Status {
        domain: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Stop routing a domain and remove its DNS records and certificate.
    Remove { domain: String },
}

pub async fn domain(command: DomainCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        DomainCommands::Add {
            domain,
            environment,
            project,
            workload,
            port,
            dns_provider,
            no_tls,
            json,
        } => {
            let definition = DomainDefinition {
                name: domain,
                environment,
                project,
                workload,
                port,
                dns_provider,
                tls: no_tls.then_some(false),
            };
            let view: DomainView = client
                .post("/domains", Some(&definition))
                .await
                .map_err(error)?;
            print_domain(&view, json);
        }
        DomainCommands::List { json } => {
            let domains: Vec<DomainView> = client.get("/domains").await.map_err(error)?;
            if json {
                print_json(&domains);
                return Ok(());
            }
            println!("DOMAIN\tENVIRONMENT\tENDPOINT\tSTATUS\tDNS\tTLS\tROUTING");
            for domain in domains {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    domain.record.name,
                    domain.record.environment,
                    domain.endpoint,
                    domain.record.status,
                    domain.record.dns.status,
                    domain.record.tls.status,
                    domain.record.routing.status
                );
            }
        }
        DomainCommands::Inspect { domain, json } => {
            let view: DomainView = client
                .get(&format!("/domains/{domain}"))
                .await
                .map_err(error)?;
            print_domain(&view, json);
        }
        DomainCommands::Status { domain, json } => {
            let domains: Vec<DomainView> = match domain {
                Some(domain) => vec![
                    client
                        .get(&format!("/domains/{domain}"))
                        .await
                        .map_err(error)?,
                ],
                None => client.get("/domains").await.map_err(error)?,
            };
            if json {
                print_json(&domains);
                return Ok(());
            }
            for domain in &domains {
                println!("{}: {}", domain.record.name, domain.record.status);
                for (what, state) in [
                    ("dns", &domain.record.dns),
                    ("tls", &domain.record.tls),
                    ("routing", &domain.record.routing),
                ] {
                    println!(
                        "  {what:<8}{:<10} desired: {}  actual: {}{}",
                        state.status,
                        state.desired.as_deref().unwrap_or("-"),
                        state.actual.as_deref().unwrap_or("-"),
                        state
                            .last_error
                            .as_ref()
                            .map(|error| format!("  error: {error}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        DomainCommands::Remove { domain } => {
            let _: serde_json::Value = client
                .delete(&format!("/domains/{domain}"))
                .await
                .map_err(error)?;
            println!("Removed {domain}");
        }
    }
    Ok(())
}

fn print_domain(view: &DomainView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    let record = &view.record;
    println!("Domain {} ({})", record.name, record.status);
    println!("Routes to: {}", view.endpoint);
    if let Some(port) = view.host_port {
        println!(
            "Endpoint: port {port}, serving {}",
            view.serving_revision.as_deref().unwrap_or("-")
        );
    }
    println!("DNS provider: {}", record.dns_provider);
    for record in &view.dns_records {
        println!(
            "  {} {} → {} ({})",
            record.record.name,
            record.record.record_type,
            record.record.value,
            record.record.state.status
        );
    }
    if let Some(certificate) = &view.certificate {
        println!(
            "Certificate: {} (renewal {}){}",
            certificate.record.status,
            certificate.record.renewal_status,
            certificate
                .record
                .expires_at
                .map(|expires| format!(", expires {}", expires.format("%Y-%m-%d")))
                .unwrap_or_default()
        );
    }
}

#[derive(Args, Debug)]
pub struct DnsCommand {
    #[command(subcommand)]
    pub command: DnsCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum DnsCommands {
    /// Every DNS record Compute manages: desired, actual, and status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Read every record back from its provider now and repair drift.
    Reconcile {
        #[arg(long)]
        json: bool,
    },
}

pub async fn dns(command: DnsCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let (records, json): (Vec<DnsRecordView>, bool) = match command.command {
        DnsCommands::Status { json } => (client.get("/dns").await.map_err(error)?, json),
        DnsCommands::Reconcile { json } => (
            client
                .post::<(), _>("/dns/reconcile", None)
                .await
                .map_err(error)?,
            json,
        ),
    };
    if json {
        print_json(&records);
        return Ok(());
    }
    println!("DOMAIN\tTYPE\tPROVIDER\tDESIRED\tACTUAL\tSTATUS\tERROR");
    for record in records {
        let record = record.record;
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            record.domain,
            record.record_type,
            record.provider,
            record.value,
            record.state.actual.as_deref().unwrap_or("-"),
            record.state.status,
            record.state.last_error.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct CertificateCommand {
    #[command(subcommand)]
    pub command: CertificateCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum CertificateCommands {
    /// Every certificate: status, expiry, renewal. Keys are never shown.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Renew a domain's certificate now.
    Renew {
        domain: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn certificate(command: CertificateCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let (certificates, json): (Vec<CertificateView>, bool) = match command.command {
        CertificateCommands::Status { json } => {
            (client.get("/certificates").await.map_err(error)?, json)
        }
        CertificateCommands::Renew { domain, json } => (
            vec![
                client
                    .post::<(), _>(&format!("/certificates/{domain}/renew"), None)
                    .await
                    .map_err(error)?,
            ],
            json,
        ),
    };
    if json {
        print_json(&certificates);
        return Ok(());
    }
    println!("DOMAIN\tSTATUS\tRENEWAL\tEXPIRES\tHELD HERE\tFINGERPRINT\tERROR");
    for certificate in certificates {
        let record = &certificate.record;
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            record.domain,
            record.status,
            record.renewal_status,
            record
                .expires_at
                .map(|expires| expires.to_rfc3339())
                .unwrap_or_else(|| "-".into()),
            certificate.held_here,
            record.fingerprint.as_deref().unwrap_or("-"),
            record.last_error.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct NetworkCommand {
    #[command(subcommand)]
    pub command: NetworkCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum NetworkCommands {
    /// Endpoints, ingress, and DNS providers on this node.
    Status {
        #[arg(long)]
        json: bool,
    },
}

pub async fn network(command: NetworkCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let NetworkCommands::Status { json } = command.command;
    let status: NetworkStatus = client.get("/network").await.map_err(error)?;
    if json {
        print_json(&status);
        return Ok(());
    }
    println!("Node: {}", status.node_id);
    println!("Endpoints listen on {}", status.endpoint_address);
    println!(
        "Ingress: http {} · https {}",
        status.ingress_http.as_deref().unwrap_or("off"),
        status.ingress_https.as_deref().unwrap_or("off")
    );
    if let Some(directory) = &status.acme_directory {
        println!("Certificates from {directory}");
    }
    for provider in &status.dns_providers {
        println!(
            "DNS {} ({}, zone {}){}",
            provider.name,
            provider.kind,
            provider.zone,
            provider
                .error
                .as_ref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        );
    }
    println!("\nENDPOINT\tPORT\tINSTANCE\tTARGET\tREVISION\tCONNECTIONS");
    for endpoint in &status.endpoints {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            endpoint.endpoint,
            endpoint.host_port,
            endpoint.instance_id,
            endpoint.target_port,
            endpoint.revision,
            endpoint.open_connections
        );
    }
    Ok(())
}
