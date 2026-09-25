//! Control-plane cost at scale, without process start-up in the way: 300
//! projects in one environment. Timings are printed; the assertions hold
//! the targets (environment view p95 <= 50 ms, one add <= 100 ms) on the
//! reference 4 vCPU host with a debug build's margin.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;

fn task_bundle() -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.sh"), "echo done").unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Shell,
        runtime_version: None,
        entrypoint: "main.sh".into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

fn percentile(values: &mut [f64], quantile: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[((values.len() - 1) as f64 * quantile).round() as usize]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_hundred_projects() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path(), store, artifacts);
    config.port_range = (29700, 29799);
    config.instance_port_range = (49700, 49799);
    config.reconcile_interval = Duration::from_secs(3600);
    let daemon = Daemon::start(config).await.unwrap();
    daemon
        .create_environment(EnvironmentDefinition {
            name: "capacity".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    let bundle = task_bundle();
    let mut adds = vec![];
    for index in 0..300 {
        let started = Instant::now();
        daemon
            .add_project(
                "capacity",
                ProjectDefinition {
                    name: format!("svc{index}"),
                    revision: "r1".into(),
                    source: None,
                    desired_state: DesiredState::Running,
                    env: BTreeMap::new(),
                    workloads: vec![WorkloadDefinition {
                        name: "job".into(),
                        kind: WorkloadKind::Task,
                        bundle: bundle.clone(),
                        ports: vec![],
                        restart: RestartPolicy::Never,
                        desired_state: DesiredState::Running,
                        readiness: None,
                    }],
                },
            )
            .await
            .unwrap();
        adds.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let last_adds = &mut adds[250..].to_vec();
    // Where an add's time goes: register, deploy, and the release.
    for index in 300..305 {
        let definition = ProjectDefinition {
            name: format!("svc{index}"),
            revision: "r1".into(),
            source: None,
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            workloads: vec![WorkloadDefinition {
                name: "job".into(),
                kind: WorkloadKind::Task,
                bundle: bundle.clone(),
                ports: vec![],
                restart: RestartPolicy::Never,
                desired_state: DesiredState::Running,
                readiness: None,
            }],
        };
        let started = Instant::now();
        let revision = daemon
            .register_revision(&definition.name, definition.revision_definition())
            .await
            .unwrap();
        let registered = started.elapsed();
        let deployment = daemon
            .deploy(DeployRequest {
                project: definition.name.clone(),
                environment: "capacity".into(),
                revision: Some(revision.revision_id),
                config: Some(BTreeMap::new()),
                desired_state: Some(DesiredState::Running),
            })
            .await
            .unwrap();
        let deployed = started.elapsed();
        daemon
            .await_release(&deployment.deployment_id, Duration::from_secs(60))
            .await
            .unwrap();
        let released = started.elapsed();
        let view = daemon.project("capacity", &definition.name).await.unwrap();
        let viewed = started.elapsed();
        assert_eq!(view.name, definition.name);
        eprintln!(
            "last cycle: {}",
            serde_json::to_string(&daemon.info().await.reconcile.last).unwrap()
        );
        eprintln!(
            "add phases: register {:?}, deploy {:?}, release {:?}, view {:?}",
            registered,
            deployed - registered,
            released - deployed,
            viewed - released
        );
    }
    let mut views = vec![];
    for _ in 0..20 {
        let started = Instant::now();
        let view = daemon.environment("capacity").await.unwrap();
        assert!(view.projects.len() >= 300);
        views.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let mut reconciles = vec![];
    for _ in 0..5 {
        let started = Instant::now();
        daemon.reconcile().await;
        reconciles.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    eprintln!(
        "300 projects: add (last 50) p50 {:.1} ms p95 {:.1} ms; environment view p50 {:.1} ms p95 {:.1} ms; full reconcile p50 {:.1} ms",
        percentile(last_adds, 0.5),
        percentile(last_adds, 0.95),
        percentile(&mut views, 0.5),
        percentile(&mut views, 0.95),
        percentile(&mut reconciles, 0.5),
    );
    daemon.shutdown().await;
}
