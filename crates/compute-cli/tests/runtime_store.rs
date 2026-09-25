//! Several Compute processes share one runtime store: they prepare the same
//! runtime at the same time while callers read their capabilities, and
//! every read is a coherent snapshot of the store.
//!
//! Capabilities combine several reads of the store (the inventory, each
//! runtime's lifecycle, and its artifact identity). A read that interleaves
//! with another process's preparation must still describe one state: the
//! runtime is either not prepared yet, with the catalog's identity, or
//! ready, with the prepared payload's. It is never failed and never half of
//! each.
//!
//! This exercises the real processes; it does not reliably land a read
//! inside a publish. The deterministic proofs that a read waits for a
//! publish are in `compute-provider`'s `runtime` tests.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use compute_core::ProviderRuntimeRequirement;
use compute_core::{RuntimeKind, RuntimeLifecycleStatus};
use compute_provider::{ComputeProvider, RemoteProvider};

const BIN: &str = env!("CARGO_BIN_EXE_compute");
const PROCESSES: usize = 3;
const ROUNDS: usize = 3;

struct Servers(Vec<Child>);

impl Drop for Servers {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What one capabilities read said about the shell runtime.
#[derive(Debug, Clone, PartialEq)]
struct Observation {
    lifecycle: Option<RuntimeLifecycleStatus>,
    artifact: Option<String>,
}

async fn observe(provider: &RemoteProvider) -> Observation {
    let capabilities = provider.capabilities().await.expect("capabilities");
    let entry = capabilities
        .inventory
        .runtimes
        .iter()
        .find(|entry| entry.id == RuntimeKind::Shell)
        .expect("the catalog's shell runtime is offered");
    Observation {
        lifecycle: entry.lifecycle,
        artifact: capabilities
            .runtime_artifacts
            .get(&RuntimeKind::Shell)
            .cloned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn processes_sharing_a_runtime_store_prepare_it_and_read_coherent_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let catalog = compute_provider::testing::host_fixture_catalog(&root.path().join("catalog"))
        .expect("fixture runtime catalog")
        .path;
    let store = root.path().join("store");
    let mut servers = Servers(vec![]);
    let mut providers = vec![];
    for index in 0..PROCESSES {
        let port = free_port();
        servers.0.push(
            Command::new(BIN)
                .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
                .arg("--job-store")
                .arg(root.path().join(format!("jobs-{index}")))
                .env("COMPUTE_RUNTIME_CATALOG", &catalog)
                .env("COMPUTE_RUNTIME_STORE", &store)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        providers.push(RemoteProvider::new(format!("http://127.0.0.1:{port}")));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    for provider in &providers {
        while provider.health().await.is_err() {
            assert!(Instant::now() < deadline, "compute serve did not answer");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let resolution = providers[0]
        .resolve_runtime(ProviderRuntimeRequirement {
            runtime: RuntimeKind::Shell,
            version: None,
            platform: None,
        })
        .await
        .unwrap();
    let distribution = resolution.distribution.expect("the catalog resolves shell");
    let unprepared = observe(&providers[0]).await;
    assert_eq!(
        unprepared.lifecycle,
        Some(RuntimeLifecycleStatus::Available)
    );

    let mut prepared: Option<Observation> = None;
    for round in 0..ROUNDS {
        // Every process prepares at once while every process is read.
        let preparing = providers.iter().map(|provider| {
            let provider = provider.clone();
            let distribution = distribution.clone();
            tokio::spawn(async move { provider.prepare_runtime(distribution).await })
        });
        let preparing = preparing.collect::<Vec<_>>();
        let reading = providers
            .iter()
            .map(|provider| {
                let provider = provider.clone();
                tokio::spawn(async move {
                    let mut seen = vec![];
                    for _ in 0..25 {
                        seen.push(observe(&provider).await);
                    }
                    seen
                })
            })
            .collect::<Vec<_>>();
        for preparation in preparing {
            let preparation = preparation.await.unwrap().expect("preparation succeeds");
            assert_eq!(preparation.status, RuntimeLifecycleStatus::Ready);
            assert!(preparation.verified);
        }
        let ready = observe(&providers[round % PROCESSES]).await;
        assert_eq!(ready.lifecycle, Some(RuntimeLifecycleStatus::Ready));
        assert_ne!(ready.artifact, unprepared.artifact);
        if let Some(prepared) = &prepared {
            assert_eq!(&ready, prepared, "the same payload every round");
        }
        prepared = Some(ready.clone());
        for reader in reading {
            for observation in reader.await.unwrap() {
                assert!(
                    observation == unprepared || observation == ready,
                    "round {round}: an incoherent read {observation:?}; \
                     expected {unprepared:?} or {ready:?}"
                );
            }
        }
        // Forget the preparation (no process is using the store now), so
        // the next round prepares and publishes again.
        std::fs::remove_file(store.join("runtime-manifest.json")).unwrap();
        std::fs::remove_dir_all(store.join("runtimes")).unwrap();
        assert_eq!(observe(&providers[0]).await, unprepared);
    }
}
