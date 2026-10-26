//! The route registry: every operation the Compute API serves, with its
//! stable operation ID, the scope it needs, and the surface it belongs to.
//! It is the one source for authorization ([`crate::auth::required_scope`]),
//! audit operation names, the published contract (`GET /api`), and the
//! UI/API parity tests. A route that is not here is not served, and an
//! unlisted request needs `compute.admin`: new routes fail closed.

use serde::Serialize;

use crate::auth::Scope;
use crate::auth::Scope::{Admin, Deploy, Execute, Operate, Read};

/// Who a route is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// The controller: the authority. A control plane, the UI, the CLI and
    /// AppPort call these.
    Controller,
    /// `compute.remote@1`: this node as a target in another controller's
    /// pool. A target answers and is never an authority; a control plane
    /// reaches what it offers through the controller's own routes.
    Target,
}
use Surface::{Controller, Target};

/// One operation of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Route {
    pub method: &'static str,
    /// The path, with parameters written `{name}`.
    pub path: &'static str,
    /// The scope a caller needs.
    pub scope: Scope,
    /// A stable identifier for the operation, independent of its path.
    pub operation: &'static str,
    pub surface: Surface,
}

const fn route(
    method: &'static str,
    path: &'static str,
    scope: Scope,
    operation: &'static str,
    surface: Surface,
) -> Route {
    Route {
        method,
        path,
        scope,
        operation,
        surface,
    }
}

/// Every operation of the Compute API.
pub const ROUTES: &[Route] = &[
    route("GET", "/health", Read, "health", Controller),
    route("GET", "/ready", Read, "ready", Controller),
    route("GET", "/info", Read, "info", Controller),
    route("GET", "/status", Read, "status", Controller),
    route("GET", "/api", Read, "api", Controller),
    route("GET", "/auth/whoami", Read, "auth.whoami", Controller),
    route("GET", "/agent", Read, "agent.status", Controller),
    route(
        "POST",
        "/agent/model",
        Operate,
        "agent.model.set",
        Controller,
    ),
    route(
        "POST",
        "/agent/address",
        Operate,
        "agent.address.set",
        Controller,
    ),
    route(
        "POST",
        "/agent/work",
        Operate,
        "agent.work.submit",
        Controller,
    ),
    route(
        "GET",
        "/agent/work/{work}",
        Read,
        "agent.work.get",
        Controller,
    ),
    route(
        "GET",
        "/agent/work/{work}/events",
        Read,
        "agent.work.events",
        Controller,
    ),
    route(
        "POST",
        "/agent/work/{work}/cancel",
        Operate,
        "agent.work.cancel",
        Controller,
    ),
    route(
        "GET",
        "/auth/credentials",
        Admin,
        "auth.credentials.list",
        Controller,
    ),
    route(
        "POST",
        "/auth/credentials",
        Admin,
        "auth.credentials.create",
        Controller,
    ),
    route(
        "POST",
        "/auth/credentials/{credential}/revoke",
        Admin,
        "auth.credential.revoke",
        Controller,
    ),
    route(
        "POST",
        "/auth/credentials/{credential}/rotate",
        Admin,
        "auth.credential.rotate",
        Controller,
    ),
    route("GET", "/audit", Admin, "audit", Controller),
    route("GET", "/metrics", Read, "metrics", Controller),
    route(
        "POST",
        "/node/reconcile",
        Operate,
        "node.reconcile",
        Controller,
    ),
    route("GET", "/node/upgrade", Read, "node.upgrade", Controller),
    route(
        "POST",
        "/node/upgrade",
        Admin,
        "node.upgrade.apply",
        Controller,
    ),
    route("POST", "/node/rollback", Admin, "node.rollback", Controller),
    route("POST", "/shutdown", Admin, "shutdown", Controller),
    route(
        "GET",
        "/environments",
        Read,
        "environments.list",
        Controller,
    ),
    route(
        "POST",
        "/environments",
        Operate,
        "environments.create",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}",
        Read,
        "environment.get",
        Controller,
    ),
    route(
        "DELETE",
        "/environments/{environment}",
        Operate,
        "environment.delete",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/status",
        Read,
        "environment.status",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/start",
        Operate,
        "environment.start",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/stop",
        Operate,
        "environment.stop",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/restart",
        Operate,
        "environment.restart",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects",
        Read,
        "environment.projects.list",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects",
        Deploy,
        "environment.projects.create",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}",
        Read,
        "environment.project.get",
        Controller,
    ),
    route(
        "DELETE",
        "/environments/{environment}/projects/{project}",
        Deploy,
        "environment.project.delete",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}/status",
        Read,
        "environment.project.status",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/start",
        Operate,
        "environment.project.start",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/stop",
        Operate,
        "environment.project.stop",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/restart",
        Operate,
        "environment.project.restart",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}/executions",
        Read,
        "environment.project.executions.list",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}/receipts",
        Read,
        "environment.project.receipts.list",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}/workloads/{workload}",
        Read,
        "environment.project.workload.get",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/start",
        Operate,
        "environment.project.workload.start",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/stop",
        Operate,
        "environment.project.workload.stop",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/restart",
        Operate,
        "environment.project.workload.restart",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/run",
        Execute,
        "environment.project.workload.run",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/projects/{project}/workloads/{workload}/logs",
        Read,
        "environment.project.workload.logs",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/computer",
        Read,
        "environment.computer",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/contents",
        Operate,
        "environment.contents",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/repositories",
        Operate,
        "environment.repositories.create",
        Controller,
    ),
    route(
        "DELETE",
        "/environments/{environment}/repositories/{repository}",
        Operate,
        "environment.repository.delete",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/packages",
        Operate,
        "environment.packages.create",
        Controller,
    ),
    route(
        "DELETE",
        "/environments/{environment}/packages/{package}",
        Operate,
        "environment.package.delete",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/processes",
        Operate,
        "environment.processes.create",
        Controller,
    ),
    route(
        "DELETE",
        "/environments/{environment}/processes/{process}",
        Operate,
        "environment.process.delete",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/processes/{process}/start",
        Operate,
        "environment.process.start",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/processes/{process}/stop",
        Operate,
        "environment.process.stop",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/reconcile",
        Operate,
        "environment.reconcile",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/replace",
        Operate,
        "environment.replace",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/fork",
        Operate,
        "environment.fork",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/checkpoint",
        Operate,
        "environment.checkpoint",
        Controller,
    ),
    route(
        "POST",
        "/checkpoints/{checkpoint}/restore",
        Operate,
        "checkpoint.restore",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/checkpoints",
        Read,
        "environment.checkpoints.list",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/checkpoints/{checkpoint}",
        Read,
        "environment.checkpoint.get",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/workspace/export",
        Execute,
        "environment.workspace.export",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/workspace/seed",
        Operate,
        "environment.workspace.seed",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/workspace/verify",
        Execute,
        "environment.workspace.verify",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/exec",
        Execute,
        "environment.exec",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/connect",
        Execute,
        "environment.connect",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/jobs/{job}",
        Read,
        "environment.job.get",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/jobs/{job}/output",
        Read,
        "environment.job.output",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/jobs/{job}/input",
        Execute,
        "environment.job.input",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/jobs/{job}/cancel",
        Execute,
        "environment.job.cancel",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/logs",
        Read,
        "environment.logs",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/run",
        Execute,
        "environment.run",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/release",
        Deploy,
        "environment.release",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/config",
        Operate,
        "environment.config.set",
        Controller,
    ),
    route(
        "GET",
        "/environments/{environment}/config",
        Read,
        "environment.config",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/config/change",
        Operate,
        "environment.config.change",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/config/import",
        Operate,
        "environment.config.import",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/config/discover",
        Execute,
        "environment.config.discover",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/lifecycle",
        Operate,
        "environment.lifecycle",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/propose",
        Execute,
        "environment.propose",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/sources",
        Deploy,
        "environment.sources",
        Controller,
    ),
    route(
        "POST",
        "/environments/{environment}/processes/{process}/restart",
        Operate,
        "environment.process.restart",
        Controller,
    ),
    route("GET", "/recipes", Read, "recipes.list", Controller),
    route(
        "GET",
        "/recipes/starters",
        Read,
        "recipes.starters",
        Controller,
    ),
    route("POST", "/recipes", Operate, "recipes.create", Controller),
    route(
        "POST",
        "/recipes/resolve",
        Read,
        "recipes.resolve",
        Controller,
    ),
    route("GET", "/recipes/{recipe}", Read, "recipe.get", Controller),
    route(
        "GET",
        "/recipes/{recipe}/resolve",
        Read,
        "recipe.resolve",
        Controller,
    ),
    route("GET", "/targets", Read, "targets", Controller),
    route("GET", "/software", Read, "software.list", Controller),
    route(
        "GET",
        "/software/{project}",
        Read,
        "software.get",
        Controller,
    ),
    route(
        "GET",
        "/software/{project}/versions",
        Read,
        "software.versions.list",
        Controller,
    ),
    route(
        "POST",
        "/software/{project}/versions",
        Deploy,
        "software.versions.create",
        Controller,
    ),
    route(
        "GET",
        "/software/{project}/versions/{version}",
        Read,
        "software.version.get",
        Controller,
    ),
    route(
        "POST",
        "/software/{project}/deploy",
        Deploy,
        "software.deploy",
        Controller,
    ),
    route(
        "GET",
        "/software/{project}/promotion",
        Read,
        "software.promotion",
        Controller,
    ),
    route(
        "POST",
        "/software/{project}/promote",
        Deploy,
        "software.promote",
        Controller,
    ),
    route(
        "POST",
        "/software/{project}/rollback",
        Deploy,
        "software.rollback",
        Controller,
    ),
    route("GET", "/rollouts", Read, "rollouts.list", Controller),
    route(
        "GET",
        "/rollouts/{rollout}",
        Read,
        "rollout.get",
        Controller,
    ),
    route("GET", "/sessions", Read, "sessions.list", Controller),
    route("POST", "/sessions", Operate, "sessions.create", Controller),
    route(
        "GET",
        "/sessions/{session}",
        Read,
        "session.get",
        Controller,
    ),
    route(
        "DELETE",
        "/sessions/{session}",
        Operate,
        "session.delete",
        Controller,
    ),
    route("GET", "/projects", Read, "projects.list", Controller),
    route(
        "GET",
        "/projects/{project}",
        Read,
        "project.get",
        Controller,
    ),
    route(
        "GET",
        "/projects/{project}/status",
        Read,
        "project.status",
        Controller,
    ),
    route(
        "GET",
        "/projects/{project}/revisions",
        Read,
        "project.revisions.list",
        Controller,
    ),
    route(
        "POST",
        "/projects/{project}/revisions",
        Deploy,
        "project.revisions.create",
        Controller,
    ),
    route("GET", "/deployments", Read, "deployments.list", Controller),
    route(
        "POST",
        "/deployments",
        Deploy,
        "deployments.create",
        Controller,
    ),
    route(
        "POST",
        "/deployments/promote",
        Deploy,
        "deployments.promote",
        Controller,
    ),
    route(
        "GET",
        "/deployments/{deployment}",
        Read,
        "deployment.get",
        Controller,
    ),
    route(
        "GET",
        "/deployments/{deployment}/receipt",
        Read,
        "deployment.receipt",
        Controller,
    ),
    route(
        "POST",
        "/deployments/{deployment}/rollback",
        Deploy,
        "deployment.rollback",
        Controller,
    ),
    route("GET", "/domains", Read, "domains.list", Controller),
    route("POST", "/domains", Operate, "domains.create", Controller),
    route("GET", "/domains/{domain}", Read, "domain.get", Controller),
    route(
        "DELETE",
        "/domains/{domain}",
        Operate,
        "domain.delete",
        Controller,
    ),
    route("GET", "/dns", Read, "dns", Controller),
    route(
        "POST",
        "/dns/reconcile",
        Operate,
        "dns.reconcile",
        Controller,
    ),
    route(
        "GET",
        "/certificates",
        Read,
        "certificates.list",
        Controller,
    ),
    route(
        "POST",
        "/certificates/{domain}/renew",
        Operate,
        "certificate.renew",
        Controller,
    ),
    route("GET", "/network", Read, "network", Controller),
    route(
        "GET",
        "/executions/{execution}",
        Read,
        "execution.get",
        Controller,
    ),
    route(
        "GET",
        "/receipts/{receipt}",
        Read,
        "receipt.get",
        Controller,
    ),
    route("GET", "/events", Read, "events", Controller),
    route("GET", "/events/stream", Read, "events.stream", Controller),
    route("GET", "/providers", Read, "providers", Controller),
    route("GET", "/services", Read, "services.list", Controller),
    route(
        "GET",
        "/services/{service}/ui",
        Read,
        "service.ui",
        Controller,
    ),
    route(
        "GET",
        "/applications",
        Read,
        "applications.list",
        Controller,
    ),
    route(
        "GET",
        "/applications/{application}",
        Read,
        "application.get",
        Controller,
    ),
    route(
        "GET",
        "/applications/{application}/deployments",
        Read,
        "application.deployments.list",
        Controller,
    ),
    route(
        "POST",
        "/applications/{application}/deployments",
        Deploy,
        "application.deployments.create",
        Controller,
    ),
    route(
        "GET",
        "/applications/{application}/deployments/{deployment}",
        Read,
        "application.deployment.get",
        Controller,
    ),
    route(
        "GET",
        "/applications/{application}/deployments/{deployment}/receipt",
        Read,
        "application.deployment.receipt",
        Controller,
    ),
    route(
        "POST",
        "/applications/{application}/rollback",
        Deploy,
        "application.rollback",
        Controller,
    ),
    route(
        "POST",
        "/applications/{application}/stop",
        Operate,
        "application.stop",
        Controller,
    ),
    route(
        "GET",
        "/applications/{application}/logs",
        Read,
        "application.logs",
        Controller,
    ),
    route("POST", "/services", Operate, "services.create", Controller),
    route(
        "DELETE",
        "/services/{service}",
        Operate,
        "service.delete",
        Controller,
    ),
    // `compute.remote@1`: this node as a target in a controller's pool.
    route(
        "GET",
        "/compute/capabilities",
        Read,
        "compute.capabilities",
        Target,
    ),
    route("GET", "/compute/health", Read, "compute.health", Target),
    route("GET", "/compute/capacity", Read, "compute.capacity", Target),
    route(
        "POST",
        "/compute/execute",
        Execute,
        "compute.execute",
        Target,
    ),
    route(
        "POST",
        "/compute/admission",
        Execute,
        "compute.admission",
        Target,
    ),
    route(
        "POST",
        "/compute/runtimes/resolve",
        Execute,
        "compute.runtimes.resolve",
        Target,
    ),
    route(
        "POST",
        "/compute/runtimes/prepare",
        Execute,
        "compute.runtimes.prepare",
        Target,
    ),
    route(
        "POST",
        "/compute/runtimes/status",
        Execute,
        "compute.runtimes.status",
        Target,
    ),
    route(
        "POST",
        "/compute/jobs",
        Execute,
        "compute.jobs.create",
        Target,
    ),
    route("GET", "/compute/jobs", Read, "compute.jobs.list", Target),
    route(
        "GET",
        "/compute/jobs/{job}",
        Read,
        "compute.job.get",
        Target,
    ),
    route(
        "GET",
        "/compute/jobs/{job}/result",
        Read,
        "compute.job.result",
        Target,
    ),
    route(
        "GET",
        "/compute/jobs/{job}/receipt",
        Read,
        "compute.job.receipt",
        Target,
    ),
    route(
        "GET",
        "/compute/jobs/{job}/logs",
        Read,
        "compute.job.logs",
        Target,
    ),
    route(
        "POST",
        "/compute/jobs/{job}/cancel",
        Execute,
        "compute.job.cancel",
        Target,
    ),
    route("GET", "/compute/inspect", Read, "compute.inspect", Target),
    route(
        "GET",
        "/compute/jobs/{job}/artifacts",
        Read,
        "compute.job.artifacts",
        Target,
    ),
    route(
        "GET",
        "/compute/jobs/{job}/events",
        Read,
        "compute.job.events",
        Target,
    ),
    route(
        "POST",
        "/compute/jobs/{job}/output",
        Read,
        "compute.job.output",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions",
        Execute,
        "compute.sessions.create",
        Target,
    ),
    route(
        "GET",
        "/compute/sessions",
        Read,
        "compute.sessions.list",
        Target,
    ),
    route(
        "GET",
        "/compute/sessions/{session}",
        Read,
        "compute.session.get",
        Target,
    ),
    route(
        "GET",
        "/compute/sessions/{session}/events",
        Read,
        "compute.session.events",
        Target,
    ),
    route(
        "GET",
        "/compute/sessions/{session}/logs",
        Read,
        "compute.session.logs",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/connect",
        Execute,
        "compute.session.connect",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/exec",
        Execute,
        "compute.session.exec",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/input",
        Execute,
        "compute.session.input",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/stop",
        Execute,
        "compute.session.stop",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/resume",
        Execute,
        "compute.session.resume",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/destroy",
        Execute,
        "compute.session.destroy",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/claim",
        Execute,
        "compute.session.claim",
        Target,
    ),
    route(
        "POST",
        "/compute/sessions/{session}/fork",
        Execute,
        "compute.session.fork",
        Target,
    ),
];

impl Route {
    fn template(&self) -> impl Iterator<Item = &'static str> {
        self.path.split('/').filter(|segment| !segment.is_empty())
    }

    /// Whether this route serves `method` on these path segments.
    pub fn matches(&self, method: &str, segments: &[&str]) -> bool {
        self.method == method
            && self.template().count() == segments.len()
            && self
                .template()
                .zip(segments)
                .all(|(expected, actual)| expected.starts_with('{') || expected == *actual)
    }

    fn literals(&self) -> usize {
        self.template()
            .filter(|segment| !segment.starts_with('{'))
            .count()
    }
}

/// The route serving a request: of those that match, the most specific
/// (`/recipes/starters` before `/recipes/{recipe}`).
pub fn find(method: &str, segments: &[&str]) -> Option<&'static Route> {
    ROUTES
        .iter()
        .filter(|route| route.matches(method, segments))
        .max_by_key(|route| route.literals())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_and_operation_is_declared_once() {
        let mut routes = std::collections::BTreeSet::new();
        let mut operations = std::collections::BTreeSet::new();
        for route in ROUTES {
            assert!(
                routes.insert((route.method, route.path)),
                "{} {} twice",
                route.method,
                route.path
            );
            assert!(
                operations.insert(route.operation),
                "{} names two routes",
                route.operation
            );
            assert_eq!(
                route.surface == Target,
                route.path.starts_with("/compute/"),
                "{} {}: the target protocol is exactly /compute/*",
                route.method,
                route.path
            );
        }
    }

    #[test]
    fn the_most_specific_route_wins() {
        assert_eq!(
            find("GET", &["recipes", "starters"]).unwrap().operation,
            "recipes.starters"
        );
        assert_eq!(
            find("GET", &["recipes", "web"]).unwrap().operation,
            "recipe.get"
        );
        assert_eq!(
            find("POST", &["deployments", "promote"]).unwrap().operation,
            "deployments.promote"
        );
        assert!(find("PATCH", &["environments"]).is_none());
    }
}
