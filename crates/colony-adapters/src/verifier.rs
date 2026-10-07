// SPDX-License-Identifier: MIT
//! Trusted host verifier. Worker prose is never evidence.
use std::fs;
use std::path::PathBuf;

use colony_core::{
    valid_path, CheckResult, Error, Evidence, FailureKind, Result, WorkUnit, WorkerResult,
};
use colony_runtime::Verifier;

use crate::hash::sha256_hex;

/// Checks artifact bytes on disk against the unit contract.
#[derive(Debug, Clone)]
pub struct HostVerifier {
    root: PathBuf,
}

impl HostVerifier {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Verifier for HostVerifier {
    fn verify(&mut self, unit: &WorkUnit, result: &WorkerResult) -> Result<Evidence> {
        let mut digests = Vec::new();
        let mut parsed = Vec::new();
        for artifact in &result.artifacts {
            let bytes = fs::read(&artifact.reference).map_err(|error| {
                Error::new(
                    FailureKind::DeterministicValidationFailure,
                    format!("cannot read artifact {}: {error}", artifact.name),
                )
            })?;
            digests.push(bytes.clone());
            parsed.push(parse_findings(&artifact.name, &bytes)?);
        }
        let mut passed = std::collections::BTreeSet::new();
        for check in &unit.verification.deterministic_checks {
            match check.as_str() {
                "report-schema" => {
                    if parsed.iter().any(|findings| findings.is_empty()) {
                        return Err(Error::new(
                            FailureKind::DeterministicValidationFailure,
                            "report-schema: findings must be a non-empty array",
                        ));
                    }
                }
                "source-references" => {
                    for findings in &parsed {
                        for finding in findings {
                            if !valid_path(&finding.reference)
                                || !unit
                                    .context
                                    .relevant_paths
                                    .iter()
                                    .any(|path| path == &finding.reference)
                                || !self.root.join(&finding.reference).is_file()
                            {
                                return Err(Error::new(
                                    FailureKind::DeterministicValidationFailure,
                                    format!(
                                        "source-references: `{}` is not a file in the pinned manifest",
                                        finding.reference
                                    ),
                                ));
                            }
                        }
                    }
                }
                other => {
                    return Err(Error::new(
                        FailureKind::DeterministicValidationFailure,
                        format!("unknown deterministic check `{other}`"),
                    ))
                }
            }
            passed.insert(check.as_str());
        }
        let mut acceptance = std::collections::BTreeSet::new();
        for criterion in &unit.acceptance {
            if covers(criterion, &passed) {
                acceptance.insert(criterion.clone());
            } else if unit.verification.semantic_review {
                return Err(Error::new(
                    FailureKind::ConvergenceFailure,
                    format!("no deterministic gate covers acceptance criterion `{criterion}`"),
                ));
            }
        }
        let semantic_approved = !unit.verification.semantic_review
            || unit
                .acceptance
                .iter()
                .all(|criterion| acceptance.contains(criterion));
        let chunks: Vec<&[u8]> = digests.iter().map(Vec::as_slice).collect();
        let digest = sha256_hex(&chunks);
        Ok(Evidence {
            work_unit: unit.id.clone(),
            verifier: "colony-verifier".into(),
            checks: unit
                .verification
                .deterministic_checks
                .iter()
                .map(|name| CheckResult {
                    name: name.clone(),
                    passed: true,
                    evidence_ref: digest.clone(),
                })
                .collect(),
            acceptance,
            consistent: true,
            semantic_approved,
            timestamp_ms: 0,
        })
    }
}

struct Finding {
    reference: String,
}

fn parse_findings(name: &str, bytes: &[u8]) -> Result<Vec<Finding>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        Error::new(
            FailureKind::DeterministicValidationFailure,
            format!("report-schema: {name} is not JSON ({error})"),
        )
    })?;
    let findings = value
        .get("findings")
        .and_then(|item| item.as_array())
        .ok_or_else(|| {
            Error::new(
                FailureKind::DeterministicValidationFailure,
                format!("report-schema: {name} needs a findings array"),
            )
        })?;
    let mut parsed = Vec::new();
    for finding in findings {
        let reference = finding
            .get("reference")
            .and_then(|item| item.as_str())
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| {
                Error::new(
                    FailureKind::DeterministicValidationFailure,
                    format!("report-schema: {name} finding is missing a reference"),
                )
            })?;
        let detail = finding
            .get("detail")
            .and_then(|item| item.as_str())
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| {
                Error::new(
                    FailureKind::DeterministicValidationFailure,
                    format!("report-schema: {name} finding is missing a detail"),
                )
            })?;
        let _ = detail;
        parsed.push(Finding {
            reference: reference.to_string(),
        });
    }
    Ok(parsed)
}

fn covers(criterion: &str, passed: &std::collections::BTreeSet<&str>) -> bool {
    let text = criterion.to_ascii_lowercase();
    if text.contains("reference") {
        return passed.contains("source-references");
    }
    if text.contains("schema") || text.contains("json") {
        return passed.contains("report-schema");
    }
    false
}
