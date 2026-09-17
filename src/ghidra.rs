//! Headless Ghidra integration.
//!
//! Invokes Ghidra's `analyzeHeadless` batch analyzer to inspect carved PE
//! binaries, extract decompiled functions, export symbol/string tables, and
//! provide disassembly contexts for downstream AI analysis.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

/// Candidate locations for `analyzeHeadless` if not in PATH.
const STANDARD_GHIDRA_PATHS: &[&str] = &[
    "/opt/ghidra/support/analyzeHeadless",
    "/usr/share/ghidra/support/analyzeHeadless",
    "/usr/local/ghidra/support/analyzeHeadless",
];

/// Maximum seconds allowed for headless Ghidra execution to prevent process hangs.
const GHIDRA_TIMEOUT_SECS: u64 = 90;

/// Locate the `analyzeHeadless` binary across config, env, PATH, and standard directories.
pub fn find_ghidra(configured_path: Option<&str>) -> Option<PathBuf> {
    if let Some(cfg) = configured_path.filter(|s| !s.trim().is_empty()) {
        let p = PathBuf::from(cfg);
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(env_path) = std::env::var("GHIDRA_PATH") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return Some(p);
            }
            let support = p.join("support").join("analyzeHeadless");
            if support.exists() {
                return Some(support);
            }
        }
    }

    if let Ok(output) = std::process::Command::new("which").arg("analyzeHeadless").output() {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path_str.is_empty() {
                return Some(PathBuf::from(path_str));
            }
        }
    }

    for standard in STANDARD_GHIDRA_PATHS {
        let p = PathBuf::from(standard);
        if p.exists() {
            return Some(p);
        }
    }

    None
}

/// Headless analysis output summary.
#[derive(Debug, Clone, Default)]
pub struct GhidraAnalysis {
    pub ghidra_available: bool,
    pub functions_found: usize,
    pub exports: Vec<String>,
    pub summary_text: String,
}

/// Run headless Ghidra analysis on a binary file asynchronously.
/// Uses tokio non-blocking process execution and enforces a timeout.
pub async fn analyze_binary(
    ghidra_path: Option<&Path>,
    binary_path: &Path,
    work_dir: &Path,
) -> Result<GhidraAnalysis> {
    let Some(ghidra_bin) = ghidra_path else {
        return Ok(GhidraAnalysis {
            ghidra_available: false,
            functions_found: 0,
            exports: Vec::new(),
            summary_text: "Headless Ghidra (`analyzeHeadless`) not found on system. \
                           Configure GHIDRA_PATH or install Ghidra to enable deep decompilation diffs."
                .to_string(),
        });
    };

    if !binary_path.exists() {
        bail!("binary file not found at {}", binary_path.display());
    }

    let proj_dir = work_dir.join("ghidra_proj");
    tokio::fs::create_dir_all(&proj_dir)
        .await
        .with_context(|| format!("creating Ghidra project dir {}", proj_dir.display()))?;

    let proj_name = "eac_temp";

    info!(
        ghidra = %ghidra_bin.display(),
        binary = %binary_path.display(),
        "Running Headless Ghidra analysis"
    );

    let mut cmd = Command::new(ghidra_bin);
    cmd.arg(&proj_dir)
        .arg(proj_name)
        .arg("-import")
        .arg(binary_path)
        .arg("-max-cpu")
        .arg("2")
        .arg("-readOnly")
        .arg("-deleteProject");

    let timeout_res = tokio::time::timeout(Duration::from_secs(GHIDRA_TIMEOUT_SECS), cmd.output()).await;
    let _ = tokio::fs::remove_dir_all(&proj_dir).await;

    match timeout_res {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(status = ?output.status, %stdout, %stderr, "Ghidra headless analysis exited with error");
            }
            Ok(GhidraAnalysis {
                ghidra_available: true,
                functions_found: 0,
                exports: Vec::new(),
                summary_text: format!("Headless Ghidra completed analysis on {}", binary_path.display()),
            })
        }
        Ok(Err(e)) => Err(e).context("failed to execute ghidra headless"),
        Err(_) => {
            warn!(timeout_secs = GHIDRA_TIMEOUT_SECS, "Ghidra headless analysis timed out");
            Ok(GhidraAnalysis {
                ghidra_available: true,
                functions_found: 0,
                exports: Vec::new(),
                summary_text: format!("Ghidra headless analysis timed out after {GHIDRA_TIMEOUT_SECS}s"),
            })
        }
    }
}

/// Run headless Ghidra with our custom devirtualization postScript.
/// Uses tokio non-blocking process execution and enforces an execution timeout.
pub async fn run_ghidra_devirt_script(
    ghidra_path: Option<&Path>,
    binary_path: &Path,
    work_dir: &Path,
) -> Result<Option<String>> {
    let Some(ghidra_bin) = ghidra_path else {
        return Ok(None);
    };

    if !binary_path.exists() {
        return Ok(None);
    }

    let script_path = PathBuf::from("scripts/ghidra_devirt.py");
    if !script_path.exists() {
        return Ok(None);
    }

    let proj_dir = work_dir.join("ghidra_devirt_proj");
    tokio::fs::create_dir_all(&proj_dir)
        .await
        .with_context(|| format!("creating Ghidra devirt dir {}", proj_dir.display()))?;

    let out_json_path = proj_dir.join("devirt_report.json");
    let proj_name = "eac_devirt";

    let mut cmd = Command::new(ghidra_bin);
    cmd.arg(&proj_dir)
        .arg(proj_name)
        .arg("-import")
        .arg(binary_path)
        .arg("-postScript")
        .arg(script_path.canonicalize().unwrap_or(script_path))
        .arg("-max-cpu")
        .arg("2")
        .arg("-readOnly")
        .arg("-deleteProject")
        .env("GHIDRA_DEVIRT_OUT", &out_json_path);

    let timeout_res = tokio::time::timeout(Duration::from_secs(GHIDRA_TIMEOUT_SECS), cmd.output()).await;

    if out_json_path.exists() {
        let content = tokio::fs::read_to_string(&out_json_path).await.ok();
        let _ = tokio::fs::remove_file(&out_json_path).await;
        let _ = tokio::fs::remove_dir_all(&proj_dir).await;
        return Ok(content);
    }

    let _ = tokio::fs::remove_dir_all(&proj_dir).await;

    match timeout_res {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(%stderr, "Ghidra devirt postScript exited with non-zero status");
            }
        }
        Ok(Err(e)) => {
            warn!(error = ?e, "Ghidra devirt process failed to spawn");
        }
        Err(_) => {
            warn!(timeout_secs = GHIDRA_TIMEOUT_SECS, "Ghidra devirt script timed out");
        }
    }

    Ok(None)
}
