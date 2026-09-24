use std::path::{Component, Path, PathBuf};

use compute_core::{
    ComputeError, ExecutionReceipt, OutputCollectionStatus, Result, sha256_file_identity,
};

pub fn inspect(path: &Path, json: bool) -> Result<()> {
    let receipt = load(path)?;
    receipt.verify()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        println!("Execution");
        println!("  ID:          {}", receipt.execution_id);
        println!("  Status:      {}", status_name(&receipt.execution.status));
        if let Some(code) = receipt.execution.exit_code {
            println!("  Exit code:   {code}");
        }
        println!("Workload");
        println!("  ID:          {}", receipt.workload);
        if let Some(bundle) = &receipt.bundle {
            println!("Bundle");
            println!("  ID:          {bundle}");
        }
        if let Some(provider) = &receipt.provider {
            println!("Provider");
            match provider {
                compute_core::ProviderIdentity::Local { id } => println!("  Local:       {id}"),
                compute_core::ProviderIdentity::Remote { id, .. } => {
                    println!("  Remote:      {id}")
                }
            }
            if let Some(protocol) = &receipt.provider_protocol {
                println!("  Protocol:    {protocol}");
            }
        }
        if let Some(placement) = &receipt.placement {
            println!("Placement");
            println!("  ID:          {}", placement.placement_id);
            println!("  Provider:    {}", placement.provider_id);
            println!("  Mode:        {}", placement.selection_mode);
            println!(
                "  Priority:    {}",
                placement.selection_reason.selection_priority
            );
        }
        println!("Distribution");
        println!("  ID:          {}", receipt.distribution.id);
        println!("  Platform:    {}", receipt.distribution.platform);
        println!("Runtime");
        println!("  {} {}", receipt.runtime.observed, receipt.runtime.version);
        println!("Policy");
        println!("  Network:     {}", receipt.policy.network);
        println!("Isolation");
        println!("  Requested:   {}", receipt.isolation.requested);
        println!("  Effective:   {}", receipt.isolation.effective);
        println!("  Filesystem:  {:?}", receipt.isolation.filesystem);
        println!("  Network:     {:?}", receipt.isolation.network);
        println!("Inputs:        {} verified", receipt.inputs.len());
        println!("Outputs:       {} recorded", receipt.outputs.len());
        println!("Receipt");
        println!("  ID:          {}", receipt.receipt_hash);
        println!("  Valid:       yes");
    }
    Ok(())
}

pub fn verify(
    path: &Path,
    distribution: Option<&Path>,
    artifacts: Option<&Path>,
    json: bool,
) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&bytes).map_err(|error| {
        ComputeError::InvalidReceipt(format!("invalid canonical encoding: {error}"))
    })?;
    receipt.verify()?;
    if receipt.encoded_bytes()? != bytes {
        return Err(ComputeError::InvalidReceipt(
            "invalid canonical encoding".into(),
        ));
    }

    let distribution_status = if let Some(root) = distribution {
        if !root.join("runtime-manifest.json").is_file() {
            return Err(ComputeError::InvalidReceipt(
                "distribution: unavailable".into(),
            ));
        }
        let report = crate::distribution::verify_root(root);
        if !report.passed {
            return Err(ComputeError::InvalidReceipt("distribution: invalid".into()));
        }
        if report.distribution_id.as_deref() != Some(receipt.distribution.id.as_str()) {
            return Err(ComputeError::InvalidReceipt(
                "distribution identity mismatch".into(),
            ));
        }
        "verified"
    } else {
        "not_requested"
    };

    let mut verified_outputs = 0_usize;
    if let Some(root) = artifacts {
        if !root.is_dir() {
            return Err(ComputeError::InvalidReceipt(
                "artifacts: unavailable".into(),
            ));
        }
        for output in &receipt.outputs {
            if !matches!(output.collection_status, OutputCollectionStatus::Collected) {
                continue;
            }
            let candidate = safe_artifact_path(root, &output.path)?;
            if !candidate.is_file() {
                return Err(ComputeError::InvalidReceipt(format!(
                    "missing output artifact: {}",
                    output.path.display()
                )));
            }
            let actual = sha256_file_identity(&candidate)?;
            if output.sha256.as_deref() != Some(actual.as_str()) {
                return Err(ComputeError::InvalidReceipt(format!(
                    "output digest mismatch: {}",
                    output.path.display()
                )));
            }
            let size = std::fs::metadata(&candidate)?.len();
            if output.size != Some(size) {
                return Err(ComputeError::InvalidReceipt(format!(
                    "output size mismatch: {}",
                    output.path.display()
                )));
            }
            verified_outputs += 1;
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "valid": true, "receipt_hash": receipt.receipt_hash,
                "distribution": distribution_status,
                "artifacts": if artifacts.is_some() { "verified" } else { "not_requested" },
                "verified_outputs": verified_outputs,
            }))?
        );
    } else {
        println!("Receipt: {}", receipt.receipt_hash);
        println!("receipt hash: verified");
        println!("canonical serialization: verified");
        println!("distribution: {distribution_status}");
        println!(
            "artifacts: {}",
            if artifacts.is_some() {
                "verified"
            } else {
                "not requested"
            }
        );
        println!("Valid: yes");
    }
    Ok(())
}

fn load(path: &Path) -> Result<ExecutionReceipt> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ComputeError::InvalidReceipt(format!("invalid receipt JSON: {error}")))
}

fn safe_artifact_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ComputeError::InvalidReceipt(format!(
            "invalid output path: {}",
            relative.display()
        )));
    }
    Ok(root.join(relative))
}

fn status_name(status: &compute_core::ExecutionStatus) -> &'static str {
    use compute_core::ExecutionStatus::*;
    match status {
        Created => "created",
        Resolved => "resolved",
        Prepared => "prepared",
        Started => "started",
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
        TimedOut => "timed_out",
        Killed => "killed",
    }
}
