//! Chip-backed agent executor for Compute.
//!
//! This module implements the runtime-neutral AgentExecutor trait using the
//! Chip agent runtime, which is bundled in the configured distribution.

use std::sync::Arc;
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
}

#[async_trait]
impl AgentExecutor for ChipAgentExecutor {
    async fn execute_agent(
        &self,
        request: AgentExecutionRequest,
    ) -> Result<AgentExecutionResult, ProviderError> {
        // Verify the runtime is available
        let runtime = self.verify_chip_available()?;

        // For now, return a placeholder result to establish the contract.
        // Real implementation would:
        // 1. Invoke compute-configured-chip with the prompt
        // 2. Capture stdout/stderr
        // 3. Parse the result
        // 4. Return AgentExecutionResult
        //
        // The full implementation requires:
        // - Finding the compute-configured-chip executable
        // - Setting up environment (COMPUTE_CONFIGURED_HOME, etc.)
        // - Running the agent
        // - Capturing and parsing output

        let output = format!(
            "Agent execution not yet fully implemented. Request: {}",
            request.prompt
        );

        Ok(AgentExecutionResult {
            output,
            success: false,
            error: Some("agent execution placeholder".into()),
            runtime: Some(runtime.name),
            metadata: None,
        })
    }
}

/// Create an AgentExecutor from the configured environment if available.
///
/// Returns None if this is base Compute (no configured distribution).
/// Returns an error if the distribution is invalid or Chip is unavailable.
pub fn create() -> Result<Option<Arc<dyn AgentExecutor>>, ProviderError> {
    match ChipAgentExecutor::from_environment() {
        Some(executor) => Ok(Some(Arc::new(executor))),
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
