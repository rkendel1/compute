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

pub fn error_envelope(command: &str, error: &ComputeError, exit_code: i32) -> Value {
    let mut detail = json!({
        "code": error.code(),
        "message": error.to_string(),
    });
    if let Some(recovery) = error.recovery() {
        detail["recovery"] = json!({ "command": recovery.command });
    }
    json!({
        "ok": false,
        "command": command,
        "exit_code": exit_code,
        "data": Value::Null,
        "error": detail,
    })
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
    fn a_failure_envelope_carries_a_code_and_deterministic_recovery() {
        let error = ComputeError::Coded {
            code: "controller_unavailable".into(),
            message: "runtime error: controller unavailable: no".into(),
        };
        let value = error_envelope("compute status", &error, 1);
        assert_eq!(value["ok"], false);
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["data"], Value::Null);
        assert_eq!(value["error"]["code"], "controller_unavailable");
        assert_eq!(value["error"]["recovery"]["command"], "compute start");
    }

    #[test]
    fn no_recovery_is_claimed_when_none_is_deterministic() {
        let error = ComputeError::InvalidWorkload("bad".into());
        let value = error_envelope("compute run", &error, 1);
        assert_eq!(value["error"]["code"], "invalid_workload");
        assert!(value["error"].get("recovery").is_none());
    }
}
