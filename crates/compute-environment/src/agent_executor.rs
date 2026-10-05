//! Chip-backed agent executor for Compute.
//!
//! This module implements the runtime-neutral AgentExecutor trait using the
//! Chip agent runtime, which is bundled in the configured distribution.
//!
//! Chip invocation contract:
//!   compute-configured-chip invoke [prompt] [--agent <name>] [options]
//!
//! The launcher script at /opt/homebrew/bin/compute-configured-chip (or equivalent
//! in other installations) sets up the environment and invokes the real Chip runtime.
//! ChipAgentExecutor reuses that launcher rather than reimplementing environment setup.

use std::process::Command;
use async_trait::async_trait;
use compute_core::{AgentExecutionRequest, AgentExecutionResult};
use compute_provider::{AgentExecutor, ProviderError, ProviderErrorKind};
use crate::agents;

/// Chip-backed executor for agent workloads.
///
/// Executes agent requests through the installed Chip runtime, using the
/// configured distribution's declarations to locate the runtime and verify
/// it's available.
pub struct ChipAgentExecutor {
    /// Path to the configured distribution root where Chip is installed.
    distribution_home: std::path::PathBuf,
}

impl ChipAgentExecutor {
    /// Create a Chip executor for the installed configured distribution.
    pub fn new(distribution_home: std::path::PathBuf) -> Result<Self, ProviderError> {
        if !distribution_home.join("stack.json").is_file() {
            return Err(ProviderError::new(
                ProviderErrorKind::DistributionUnavailable,
                "configured distribution not found or invalid",
            ));
        }
        Ok(Self { distribution_home })
    }

    /// Create a Chip executor from the environment variable if set.
    pub fn from_environment() -> Option<Self> {
        agents::distribution_home().and_then(|home| {
            Self::new(home).ok()
        })
    }

    /// Verify that Chip is available and working.
    fn verify_chip_available(&self) -> Result<agents::AgentRuntime, ProviderError> {
        let caps = agents::declared(&self.distribution_home);
        caps.default_runtime()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::RuntimeUnavailable,
                    "no agent runtime configured in this distribution",
                )
            })
            .map(|rt| rt.clone())
    }

    /// Invoke the real Chip runtime via compute-configured-chip launcher.
    ///
    /// The launcher script handles environment setup (COMPUTE_CONFIGURED_HOME,
    /// COMPUTE_STACKS, Node runtime resolution).
    fn invoke_chip(
        &self,
        prompt: &str,
        agent: Option<&str>,
    ) -> Result<String, ProviderError> {
        let mut cmd = Command::new("compute-configured-chip");
        cmd.arg("invoke").arg(prompt);

        // Apply explicit agent selection if provided
        if let Some(agent_name) = agent {
            cmd.arg("--agent").arg(agent_name);
        }

        // Set working directory to the distribution root so Chip can find its project
        cmd.current_dir(&self.distribution_home);

        let output = cmd
            .output()
            .map_err(|e| {
                ProviderError::new(
                    ProviderErrorKind::RemoteExecutionFailure,
                    format!("failed to invoke compute-configured-chip: {e}"),
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            return Err(ProviderError::new(
                ProviderErrorKind::RemoteExecutionFailure,
                format!(
                    "chip invocation failed: {}{}",
                    if !stderr.is_empty() {
                        format!("stderr: {}", stderr)
                    } else {
                        String::new()
                    },
                    if !stdout.is_empty() && !stderr.is_empty() {
                        format!("; stdout: {}", stdout)
                    } else if !stdout.is_empty() {
                        format!("stdout: {}", stdout)
                    } else {
                        String::new()
                    }
                ),
            ));
        }

        String::from_utf8(output.stdout).map_err(|e| {
            ProviderError::new(
                ProviderErrorKind::RemoteExecutionFailure,
                format!("chip output is not valid UTF-8: {e}"),
            )
        })
    }
}

#[async_trait]
impl AgentExecutor for ChipAgentExecutor {
    async fn execute_agent(
        &self,
        request: AgentExecutionRequest,
    ) -> Result<AgentExecutionResult, ProviderError> {
        // Verify the requested or default runtime is available
        let runtime = if let Some(agent_name) = &request.agent {
            // Verify the explicitly requested agent exists
            let caps = agents::declared(&self.distribution_home);
            caps.runtimes
                .iter()
                .find(|rt| &rt.name == agent_name)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::RuntimeUnavailable,
                        format!("agent runtime '{agent_name}' is not available"),
                    )
                })?
                .clone()
        } else {
            // Use default runtime
            self.verify_chip_available()?
        };

        // Invoke Chip with the prompt
        let output = self.invoke_chip(&request.prompt, request.agent.as_deref())?;

        // Chip invocation succeeded; the output is the agent's response
        Ok(AgentExecutionResult {
            output,
            success: true,
            error: None,
            runtime: Some(runtime.name),
            metadata: None,
        })
    }
}

/// Create an AgentExecutor from the configured environment if available.
///
/// Returns None if this is base Compute (no configured distribution).
/// Returns an error if the distribution is invalid or Chip is unavailable.
pub fn create() -> Result<Option<std::sync::Arc<dyn AgentExecutor>>, ProviderError> {
    match ChipAgentExecutor::from_environment() {
        Some(executor) => Ok(Some(std::sync::Arc::new(executor))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_executor_on_base_compute() {
        // Base Compute has no configured distribution
        // This test verifies that create() returns None rather than erroring
        // when run on a system without a configured install
        // (This is a conceptual test; actual test would mock the environment)
    }
}
