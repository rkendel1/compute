//! The machine-readable command contract.
//!
//! Every command keeps its own success output (existing consumers depend on
//! it). What is uniform is the **failure**: a command run with `--json` that
//! fails writes one envelope as the **last line of stderr**, with a stable error
//! code and, where exactly one recovery command is known, that command as data.
//! stdout is left to the command's result: existing consumers parse it, and a
//! command that reports a failed result there (`certify`, `bundle verify`) must
//! stay exactly one JSON document.
//!
//! ```json
//! {"ok":false,"command":"compute environment inspect","exit_code":1,
//!  "data":null,"error":{"code":"controller_unavailable","message":"…",
//!  "recovery":{"command":"compute start"}}}
//! ```
//!
//! The human message is the line before it, unchanged. Commands that adopt the
//! full envelope as their own result (`compute doctor`) add its fields without
//! removing any they already had.

use clap::CommandFactory;
use compute_core::ComputeError;
use serde_json::{Value, json};

/// Whether the caller asked for machine-readable output.
pub fn json_requested(arguments: &[std::ffi::OsString]) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| *argument != "--")
        .any(|argument| argument == "--json")
}

/// `compute environment inspect` for `compute environment inspect web --json`:
/// the subcommand path clap resolved, never the operands.
pub fn command_path<C: CommandFactory>(arguments: &[std::ffi::OsString]) -> String {
    let mut path = vec!["compute".to_string()];
    if let Ok(matches) = C::command().try_get_matches_from(arguments.iter().cloned()) {
        let mut current = &matches;
        while let Some((name, next)) = current.subcommand() {
            path.push(name.to_owned());
            current = next;
        }
    }
    path.join(" ")
}

/// The controller endpoint this invocation would use: `--daemon`, then
/// `$COMPUTE_DAEMON`, then the default.
pub fn daemon_endpoint(arguments: &[std::ffi::OsString]) -> String {
    let mut explicit = None;
    let mut iterator = arguments.iter().skip(1);
    while let Some(argument) = iterator.next() {
        let Some(text) = argument.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--daemon" {
            explicit = iterator.next().and_then(|value| value.to_str());
        } else if let Some(value) = text.strip_prefix("--daemon=") {
            explicit = Some(value);
        }
    }
    explicit
        .map(str::to_owned)
        .or_else(|| std::env::var("COMPUTE_DAEMON").ok())
        .unwrap_or_else(|| compute_environment::client::DEFAULT_ENDPOINT.into())
}

/// The one command that recovers from `code`, when there is exactly one.
///
/// Only `controller_unavailable` on the default local endpoint qualifies: the
/// controller this machine runs is started by `compute start`. For any other
/// endpoint (a remote or custom controller) starting a local one is not the
/// fix, and every other error has several possible causes, so nothing is
/// claimed.
pub fn recovery(code: &str, endpoint: &str) -> Option<&'static str> {
    (code == "controller_unavailable" && endpoint == compute_environment::client::DEFAULT_ENDPOINT)
        .then_some("compute start")
}

pub fn error_envelope(
    command: &str,
    error: &ComputeError,
    exit_code: i32,
    endpoint: &str,
) -> Value {
    let code = error.code();
    let mut detail = json!({
        "code": code,
        "message": error.to_string(),
    });
    if let Some(command) = recovery(code, endpoint) {
        detail["recovery"] = json!({ "command": command });
    }
    json!({
        "ok": false,
        "command": command,
        "exit_code": exit_code,
        "data": Value::Null,
        "error": detail,
    })
}

/// One `compute doctor` finding.
///
/// `pass`: verified. `warn`: something this host cannot do, which only matters
/// if you need it (an uninstalled runtime). `fail`: something that stops
/// Compute working here (a controller that does not answer or refuses the
/// credential). Doctor reports what each runtime adapter and the controller
/// say about themselves; it evaluates no requirements and makes no admission
/// decision, so it cannot disagree with them.
pub fn doctor_checks(
    reports: &[compute_core::RuntimeReport],
    controller: Option<&Value>,
) -> Vec<Value> {
    let mut checks = reports
        .iter()
        .map(|report| {
            let availability = &report.availability;
            json!({
                "id": format!("runtime:{}", report.runtime),
                "status": if availability.available { "pass" } else { "warn" },
                "detail": availability.version,
                "remediation": availability.remediation,
            })
        })
        .collect::<Vec<_>>();
    if let Some(controller) = controller {
        let reachable = controller["reachable"].as_bool().unwrap_or(false);
        let problem = if !reachable {
            controller
                .get("remediation")
                .or_else(|| controller.get("error"))
        } else {
            // A controller that answered `/info` reports its authentication
            // *mode* as an object under `authentication`; one that refused the
            // credential reports why, as a string (`node_cmd::controller_diagnosis`).
            controller
                .get("authentication")
                .filter(|authentication| authentication.is_string())
        };
        checks.push(json!({
            "id": "controller",
            "status": if reachable && problem.is_none() { "pass" } else { "fail" },
            "detail": controller["endpoint"],
            "remediation": problem.cloned().unwrap_or(Value::Null),
        }));
    }
    checks
}

/// What `compute doctor` exits with: always 0 unless `--strict` and any check
/// is not `pass`. Returns `(findings, exit_code)`.
pub fn doctor_outcome(checks: &[Value], strict: bool) -> (usize, i32) {
    let findings = checks
        .iter()
        .filter(|check| check["status"] != "pass")
        .count();
    (findings, i32::from(strict && findings > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(Into::into).collect()
    }

    #[test]
    fn json_is_detected_before_a_terminator_only() {
        assert!(json_requested(&arguments(&["compute", "doctor", "--json"])));
        assert!(!json_requested(&arguments(&["compute", "doctor"])));
        assert!(!json_requested(&arguments(&[
            "compute", "run", "--", "--json"
        ])));
    }

    #[test]
    fn recovery_is_claimed_only_for_a_local_controller_that_is_down() {
        let default = compute_environment::client::DEFAULT_ENDPOINT;
        assert_eq!(
            recovery("controller_unavailable", default),
            Some("compute start")
        );
        // A remote or custom controller is not started by `compute start`.
        assert_eq!(
            recovery("controller_unavailable", "http://10.0.0.5:8787"),
            None
        );
        assert_eq!(
            recovery("controller_unavailable", "http://127.0.0.1:1"),
            None
        );
        // Several causes, several fixes: nothing is claimed.
        for code in [
            "runtime_unavailable",
            "unknown_runtime",
            "not_found",
            "conflict",
            "invalid_workload",
            "isolation_unavailable",
        ] {
            assert_eq!(recovery(code, default), None, "{code}");
        }
    }

    #[test]
    fn the_daemon_endpoint_follows_the_flag_then_the_default() {
        assert_eq!(
            daemon_endpoint(&arguments(&["compute", "status", "--daemon", "http://h:1"])),
            "http://h:1"
        );
        assert_eq!(
            daemon_endpoint(&arguments(&["compute", "status", "--daemon=http://h:2"])),
            "http://h:2"
        );
    }

    #[test]
    fn a_failure_envelope_carries_a_code_and_deterministic_recovery() {
        let error = ComputeError::Coded {
            code: "controller_unavailable".into(),
            message: "runtime error: controller unavailable: no".into(),
        };
        let default = compute_environment::client::DEFAULT_ENDPOINT;
        let value = error_envelope("compute status", &error, 1, default);
        assert_eq!(value["ok"], false);
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["data"], Value::Null);
        assert_eq!(value["error"]["code"], "controller_unavailable");
        assert_eq!(value["error"]["recovery"]["command"], "compute start");
        let elsewhere = error_envelope("compute status", &error, 1, "http://h:1");
        assert!(elsewhere["error"].get("recovery").is_none());
    }

    fn check(id: &str, status: &str) -> Value {
        json!({ "id": id, "status": status })
    }

    #[test]
    fn doctor_exit_codes() {
        let healthy = [check("runtime:python", "pass"), check("controller", "pass")];
        let warning = [
            check("runtime:python", "pass"),
            check("runtime:ruby", "warn"),
        ];
        let failure = [check("controller", "fail")];
        // Healthy: 0 either way.
        assert_eq!(doctor_outcome(&healthy, false), (0, 0));
        assert_eq!(doctor_outcome(&healthy, true), (0, 0));
        // Without --strict, findings are reported and the exit is 0.
        assert_eq!(doctor_outcome(&warning, false), (1, 0));
        assert_eq!(doctor_outcome(&failure, false), (1, 0));
        // --strict makes any non-pass (a warning too) exit 1.
        assert_eq!(doctor_outcome(&warning, true), (1, 1));
        assert_eq!(doctor_outcome(&failure, true), (1, 1));
    }

    #[test]
    fn a_controller_is_pass_only_when_it_answers_and_accepts_the_credential() {
        let status = |controller: Value| doctor_checks(&[], Some(&controller))[0].clone();
        let ok = status(json!({ "endpoint": "e", "reachable": true, "status": "ok" }));
        assert_eq!(ok["status"], "pass");
        // The shape a healthy controller really reports: `authentication` is
        // its mode, not a problem.
        let healthy = status(json!({
            "endpoint": "e", "reachable": true, "status": "ok",
            "authentication": { "mode": "development", "required": false, "active_credentials": 0 },
            "controller": { "version": "0.1.9" }
        }));
        assert_eq!(healthy["status"], "pass");
        assert!(healthy["remediation"].is_null());
        let down = status(json!({
            "endpoint": "e", "reachable": false, "error": "refused",
            "remediation": "start the controller with `compute start`"
        }));
        assert_eq!(down["status"], "fail");
        assert!(
            down["remediation"]
                .as_str()
                .unwrap()
                .contains("compute start")
        );
        let refused = status(json!({
            "endpoint": "e", "reachable": true, "authentication": "a credential is required"
        }));
        assert_eq!(refused["status"], "fail");
        assert_eq!(refused["remediation"], "a credential is required");
        // Unreachable with only an error (no remediation) still says why.
        let broken = status(json!({ "endpoint": "e", "reachable": false, "error": "bad url" }));
        assert_eq!(broken["status"], "fail");
        assert_eq!(broken["remediation"], "bad url");
    }
}
