//! Native PE Protection, VM Detection, and Devirtualization Pipeline.
//!
//! Provides static inspection of virtualized modules (e.g. EAC VM, VMProtect),
//! heuristic opcode and dispatcher detection, coordinates Ghidra decompilation,
//! and queries NVIDIA NIM AI for devirtualization and native logic recovery.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::analysis::shannon_entropy;

/// Information on an executable section that may contain virtual machine bytecodes or handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspiciousSection {
    pub name: String,
    pub raw_size: u32,
    pub virtual_size: u32,
    pub entropy: f64,
    pub is_executable: bool,
    pub is_writable: bool,
    pub indicator: String,
}

/// A candidate virtual machine entry stub or dispatcher location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmCandidate {
    pub rva: u64,
    pub pattern_type: String,
    pub snippet_hex: String,
    pub description: String,
}

/// Report produced by static binary analysis for virtualization artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmProtectionReport {
    pub file_size: usize,
    pub architecture: String,
    pub suspected_protector: Option<String>,
    pub suspicious_sections: Vec<SuspiciousSection>,
    pub vm_candidates: Vec<VmCandidate>,
    pub has_control_flow_flattening: bool,
    pub summary: String,
}

/// Overall result of the devirtualization pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevirtResult {
    pub game: String,
    pub platform: String,
    pub module_name: String,
    pub protection: VmProtectionReport,
    pub ghidra_decompilation: Option<String>,
    pub ai_analysis: Option<String>,
}

/// Analyze raw PE binary bytes for VM protectors, packer signatures, and dispatcher stubs.
pub fn scan_pe_for_vm(bytes: &[u8]) -> VmProtectionReport {
    let mut suspicious_sections = Vec::new();
    let mut vm_candidates = Vec::new();
    let mut suspected_protector = None;
    let mut has_cff = false;
    let mut arch = "Unknown".to_string();

    if let Ok(pe) = goblin::pe::PE::parse(bytes) {
        arch = if pe.is_64 {
            "x86_64".to_string()
        } else {
            "x86 (32-bit)".to_string()
        };

        for sec in &pe.sections {
            let name = sec.name().unwrap_or("?").to_string();
            let start = sec.pointer_to_raw_data as usize;
            let end = (start + sec.size_of_raw_data as usize).min(bytes.len());
            let sec_bytes = if start < bytes.len() {
                &bytes[start..end]
            } else {
                &[]
            };

            let entropy = shannon_entropy(sec_bytes);
            let is_exec = (sec.characteristics & 0x2000_0000) != 0;
            let is_write = (sec.characteristics & 0x8000_0000) != 0;

            let lower = name.to_lowercase();
            let mut indicator = String::new();

            if lower.contains(".vmp") || lower.contains("vmp") {
                suspected_protector = Some("VMProtect / EAC Virtualizer".to_string());
                indicator = "Known VM section name (.vmp)".to_string();
            } else if lower.contains(".themida") || lower.contains(".winlic") {
                suspected_protector = Some("Themida / WinLicense".to_string());
                indicator = "Known protector section".to_string();
            } else if lower.contains(".eac") || lower.contains("easy") {
                suspected_protector = Some("EAC Proprietary Protection".to_string());
                indicator = "Anti-cheat specific section".to_string();
            } else if is_exec && is_write {
                indicator = "W+X Executable & Writable section (self-modifying / unpacker)".to_string();
            } else if is_exec && entropy > 7.1 {
                indicator = format!("High-entropy executable code (entropy: {entropy:.2})");
            }

            if !indicator.is_empty() {
                suspicious_sections.push(SuspiciousSection {
                    name,
                    raw_size: sec.size_of_raw_data,
                    virtual_size: sec.virtual_size,
                    entropy,
                    is_executable: is_exec,
                    is_writable: is_write,
                    indicator,
                });
            }

            // Scan executable sections for VM entry stubs & indirect dispatch loops (bounded to 2MB)
            if is_exec && sec_bytes.len() >= 16 {
                let scan_limit = sec_bytes.len().min(2 * 1024 * 1024);
                let mut indirect_jumps_count = 0;
                for i in 0..scan_limit.saturating_sub(8) {
                    if vm_candidates.len() >= 6 && has_cff {
                        break;
                    }
                    // Look for indirect jumps:
                    // 0xFF 0x20..0x27 (jmp [reg]), 0xFF 0xE0..0xE7 (jmp reg)
                    if sec_bytes[i] == 0xFF && (sec_bytes[i + 1] & 0x38) == 0x20 {
                        indirect_jumps_count += 1;
                        if vm_candidates.len() < 4 {
                            let hex_str = hex::encode(&sec_bytes[i..(i + 8).min(sec_bytes.len())]);
                            vm_candidates.push(VmCandidate {
                                rva: (sec.virtual_address as u64) + (i as u64),
                                pattern_type: "Indirect Dispatch Jump".to_string(),
                                snippet_hex: hex_str,
                                description: "Computed/indirect jump typically used by bytecode handlers".to_string(),
                            });
                        }
                    }
                    // VM entry stub signature: pushfq (0x9C) followed by register pushes
                    if sec_bytes[i] == 0x9C && i + 4 < sec_bytes.len() && (sec_bytes[i + 1] >= 0x50 && sec_bytes[i + 1] <= 0x57) {
                        if vm_candidates.len() < 6 {
                            let hex_str = hex::encode(&sec_bytes[i..(i + 8).min(sec_bytes.len())]);
                            vm_candidates.push(VmCandidate {
                                rva: (sec.virtual_address as u64) + (i as u64),
                                pattern_type: "VM Context Save / Entry Stub".to_string(),
                                snippet_hex: hex_str,
                                description: "pushfq + push reg sequence saving CPU context for virtual interpreter".to_string(),
                            });
                        }
                    }
                }

                if indirect_jumps_count > 10 {
                    has_cff = true;
                }
            }
        }
    }

    let summary = match &suspected_protector {
        Some(p) => format!("{p} detected with {} suspicious section(s) and {} candidate dispatcher patterns.", suspicious_sections.len(), vm_candidates.len()),
        None if has_cff => format!("Control flow flattening / indirect dispatch patterns detected across {} sections.", suspicious_sections.len()),
        None if !suspicious_sections.is_empty() => format!("Found {} anomalous high-entropy or W+X section(s).", suspicious_sections.len()),
        None => "No overt virtualization signatures detected; standard compiled binary.".to_string(),
    };

    VmProtectionReport {
        file_size: bytes.len(),
        architecture: arch,
        suspected_protector,
        suspicious_sections,
        vm_candidates,
        has_control_flow_flattening: has_cff,
        summary,
    }
}

/// Execute the complete devirtualization pipeline on a binary module file.
pub async fn run_devirtualization_pipeline(
    game: &str,
    platform: &str,
    module_name: &str,
    binary_path: &Path,
    working_dir: &Path,
    ghidra_path: Option<&str>,
    nvidia_key: Option<&str>,
    ai_model: &str,
    ai_delay_ms: u64,
) -> Result<DevirtResult> {
    let bytes = tokio::fs::read(binary_path)
        .await
        .with_context(|| format!("reading binary at {}", binary_path.display()))?;

    // Check on-disk analysis cache to eliminate redundant decompilation & API calls
    use sha2::{Digest, Sha256};
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let cache_dir = working_dir.join("analysis");
    let cache_file = cache_dir.join(format!("{sha256}.devirt.json"));

    if cache_file.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&cache_file).await {
            if let Ok(mut cached) = serde_json::from_str::<DevirtResult>(&content) {
                let is_error = cached.ai_analysis.as_ref().map(|s| s.starts_with("AI synthesis request failed")).unwrap_or(false);
                if (!is_error && cached.ai_analysis.is_some()) || nvidia_key.is_none() {
                    cached.game = game.to_string();
                    cached.platform = platform.to_string();
                    return Ok(cached);
                }
            }
        }
    }

    // Step 1: Static heuristic scanning for VM artifacts
    let protection = scan_pe_for_vm(&bytes);

    // Step 2: Headless Ghidra decompilation & symbol extraction
    let ghidra_bin = crate::ghidra::find_ghidra(ghidra_path);
    let ghidra_decompilation = if let Some(bin) = &ghidra_bin {
        crate::ghidra::run_ghidra_devirt_script(Some(bin), binary_path, working_dir).await.ok().flatten()
    } else {
        None
    };

    // Step 3: NVIDIA NIM AI Devirtualization synthesis
    let mut ai_analysis = None;
    if let Some(key) = nvidia_key.filter(|k| !k.trim().is_empty()) {
        let mut heuristic_lines = Vec::new();
        heuristic_lines.push(format!("Architecture: {}", protection.architecture));
        heuristic_lines.push(format!("Suspected Protector: {}", protection.suspected_protector.as_deref().unwrap_or("None detected")));
        heuristic_lines.push(format!("Control Flow Flattening: {}", if protection.has_control_flow_flattening { "Detected" } else { "None" }));
        heuristic_lines.push(format!("Summary: {}", protection.summary));
        if !protection.suspicious_sections.is_empty() {
            heuristic_lines.push("Suspicious Sections:".to_string());
            for s in &protection.suspicious_sections {
                heuristic_lines.push(format!(" - {} (entropy: {:.2}, raw: {} bytes)", s.name, s.entropy, s.raw_size));
            }
        }
        let heuristic_str = heuristic_lines.join("\n");

        match crate::ai::devirtualize_analysis(
            key,
            ai_model,
            ai_delay_ms,
            game,
            platform,
            module_name,
            &heuristic_str,
            ghidra_decompilation.as_deref(),
        ).await {
            Ok(output) => ai_analysis = Some(output),
            Err(e) => {
                tracing::warn!(error = ?e, "AI devirtualization synthesis failed");
                ai_analysis = Some(format!("AI synthesis request failed: {e}"));
            }
        }
    }

    let result = DevirtResult {
        game: game.to_string(),
        platform: platform.to_string(),
        module_name: module_name.to_string(),
        protection,
        ghidra_decompilation,
        ai_analysis,
    };

    let is_error = result.ai_analysis.as_ref().map(|s| s.starts_with("AI synthesis request failed")).unwrap_or(false);
    if !is_error && result.ai_analysis.is_some() {
        if let Ok(serialized) = serde_json::to_string_pretty(&result) {
            let _ = tokio::fs::create_dir_all(&cache_dir).await;
            let _ = tokio::fs::write(&cache_file, serialized).await;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_empty_or_plain_bytes() {
        let report = scan_pe_for_vm(&[0u8; 128]);
        assert_eq!(report.file_size, 128);
        assert!(report.suspicious_sections.is_empty());
        assert!(report.vm_candidates.is_empty());
    }

    #[test]
    fn scan_reports_file_size() {
        let dummy = vec![0x90; 1024];
        let report = scan_pe_for_vm(&dummy);
        assert_eq!(report.file_size, 1024);
        assert_eq!(report.architecture, "Unknown");
    }
}
