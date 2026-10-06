// SPDX-License-Identifier: MIT
//! Transport-independent orchestration contracts. Monetary values are integer micro-units.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidGraph,
    PolicyViolation,
    BudgetExhausted,
    ProviderUnavailable,
    ProviderThrottled,
    ContextOverflow,
    ContractViolation,
    InvalidArtifact,
    DeterministicValidationFailure,
    SemanticConflict,
    DependencyFailure,
    ConvergenceFailure,
    InfrastructureFailure,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub kind: FailureKind,
    pub message: String,
}
impl Error {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn ensure(ok: bool, kind: FailureKind, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::new(kind, message))
    }
}
pub fn score(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Classification {
    Public,
    Internal,
    Confidential,
    Restricted,
    LocalOnly,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub money_micros: u64,
    pub tokens: u64,
    pub calls: u64,
}
impl Budget {
    pub fn fits(self, limit: Self) -> bool {
        self.money_micros <= limit.money_micros
            && self.tokens <= limit.tokens
            && self.calls <= limit.calls
    }
    pub fn checked_add(self, other: Self) -> Result<Self> {
        Ok(Self {
            money_micros: self
                .money_micros
                .checked_add(other.money_micros)
                .ok_or_else(overflow)?,
            tokens: self.tokens.checked_add(other.tokens).ok_or_else(overflow)?,
            calls: self.calls.checked_add(other.calls).ok_or_else(overflow)?,
        })
    }
}
fn overflow() -> Error {
    Error::new(FailureKind::BudgetExhausted, "budget arithmetic overflow")
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub allowed_providers: BTreeSet<String>,
    pub max_classification: Classification,
    pub allow_mutation: bool,
    pub require_human_approval: bool,
    pub max_workers: usize,
    pub max_depth: u32,
    pub deadline_ms: u64,
    pub budget: Budget,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextManifest {
    pub source_sha: String,
    pub relevant_paths: Vec<String>,
    pub symbols: Vec<String>,
    pub ontology_entities: Vec<String>,
    pub architectural_constraints: Vec<String>,
    pub trusted_context: Vec<String>,
    pub retrieved_untrusted: Vec<String>,
    pub input_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Demand {
    pub reasoning: f64,
    pub coding: f64,
    pub research: f64,
    pub structured_output: f64,
    pub tool_use: f64,
    pub output_tokens: u64,
}
impl Demand {
    pub fn vector(&self) -> [f64; 5] {
        [
            self.reasoning,
            self.coding,
            self.research,
            self.structured_output,
            self.tool_use,
        ]
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub allowed_paths: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub branch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationContract {
    pub deterministic_checks: Vec<String>,
    pub semantic_review: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkUnit {
    pub id: String,
    pub objective: String,
    pub rationale: String,
    pub dependencies: Vec<String>,
    pub expected_outputs: Vec<String>,
    pub acceptance: Vec<String>,
    pub context: ContextManifest,
    pub demand: Demand,
    pub classification: Classification,
    pub mutation: Option<Mutation>,
    pub relational_density: f64,
    pub uncertainty: f64,
    pub consequence: f64,
    pub budget: Budget,
    pub estimated_ms: u64,
    pub verification: VerificationContract,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraph {
    pub colony_id: String,
    pub objective: String,
    pub nodes: Vec<WorkUnit>,
    pub policy: Policy,
}
impl WorkGraph {
    pub fn validate(&self) -> Result<Vec<String>> {
        use FailureKind::InvalidGraph as Invalid;
        ensure(
            !self.colony_id.trim().is_empty()
                && !self.objective.trim().is_empty()
                && !self.nodes.is_empty(),
            Invalid,
            "empty graph identity, objective or nodes",
        )?;
        ensure(
            self.policy.max_workers > 0
                && self.policy.deadline_ms > 0
                && !self.policy.allowed_providers.is_empty(),
            Invalid,
            "invalid policy limits",
        )?;
        let mut ids = BTreeSet::new();
        for n in &self.nodes {
            ensure(
                !n.id.trim().is_empty() && ids.insert(n.id.clone()),
                Invalid,
                format!("duplicate or empty id: {}", n.id),
            )?;
            ensure(
                !n.objective.trim().is_empty()
                    && !n.rationale.trim().is_empty()
                    && !n.acceptance.is_empty()
                    && !n.expected_outputs.is_empty(),
                Invalid,
                format!("unbounded contract: {}", n.id),
            )?;
            ensure(
                n.acceptance
                    .iter()
                    .chain(&n.expected_outputs)
                    .all(|s| !s.trim().is_empty())
                    && n.expected_outputs.iter().collect::<BTreeSet<_>>().len()
                        == n.expected_outputs.len(),
                Invalid,
                "empty or duplicate contract entries",
            )?;
            ensure(
                n.demand
                    .vector()
                    .into_iter()
                    .chain([n.relational_density, n.uncertainty, n.consequence])
                    .all(score),
                Invalid,
                "scores must be finite and between zero and one",
            )?;
            ensure(
                n.budget.calls > 0 && n.demand.output_tokens > 0 && n.estimated_ms > 0,
                Invalid,
                "zero work limit",
            )?;
            ensure(
                n.context
                    .input_tokens
                    .checked_add(n.demand.output_tokens)
                    .is_some_and(|v| v <= n.budget.tokens),
                Invalid,
                "context and output exceed unit token budget",
            )?;
            ensure(
                valid_sha(&n.context.source_sha),
                Invalid,
                "context requires immutable source SHA",
            )?;
            ensure(
                n.context.relevant_paths.iter().all(|p| valid_path(p)),
                Invalid,
                "unsafe context path",
            )?;
            ensure(
                !n.verification.deterministic_checks.is_empty()
                    && n.verification
                        .deterministic_checks
                        .iter()
                        .all(|s| !s.trim().is_empty()),
                Invalid,
                "deterministic verification required",
            )?;
            ensure(
                n.dependencies.iter().collect::<BTreeSet<_>>().len() == n.dependencies.len(),
                Invalid,
                "duplicate dependency",
            )?;
            if let Some(m) = &n.mutation {
                ensure(
                    !m.allowed_paths.is_empty()
                        && m.allowed_paths
                            .iter()
                            .chain(&m.forbidden_paths)
                            .all(|p| valid_path(p))
                        && valid_path(&m.branch),
                    Invalid,
                    "invalid mutation scope or branch",
                )?;
            }
        }
        let mut remaining: BTreeMap<_, _> = self
            .nodes
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    n.dependencies.iter().cloned().collect::<BTreeSet<_>>(),
                )
            })
            .collect();
        ensure(
            remaining.values().flatten().all(|d| ids.contains(d)),
            Invalid,
            "unknown dependency",
        )?;
        for n in &self.nodes {
            for dep in &n.dependencies {
                let parent = self.unit(dep)?;
                ensure(
                    n.classification >= parent.classification,
                    FailureKind::PolicyViolation,
                    "dependency data classification cannot be downgraded",
                )?;
            }
        }
        let mut order = Vec::new();
        while !remaining.is_empty() {
            let ready: Vec<_> = remaining
                .iter()
                .filter(|(_, d)| d.is_empty())
                .map(|(id, _)| id.clone())
                .collect();
            ensure(!ready.is_empty(), Invalid, "dependency cycle")?;
            for id in ready {
                remaining.remove(&id);
                for deps in remaining.values_mut() {
                    deps.remove(&id);
                }
                order.push(id);
            }
        }
        Ok(order)
    }
    pub fn unit(&self, id: &str) -> Result<&WorkUnit> {
        self.nodes
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| Error::new(FailureKind::InvalidGraph, format!("unknown work unit {id}")))
    }
}
pub fn valid_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn valid_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', ':', '\0'])
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}
pub fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|p| p.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|p| p.starts_with('/'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    DependsOn,
    SharesState,
    Implements,
    Tests,
    Independent,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relation {
    pub from: String,
    pub to: String,
    pub kind: RelationKind,
    pub strength: f64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OntologySlice {
    pub revision: String,
    pub relations: Vec<Relation>,
    pub locked_entities: BTreeSet<String>,
}
/// Supplied by Padagonia; no semantic store is maintained in Colony.
pub trait SemanticSource {
    fn slice(&self, objective: &str) -> Result<OntologySlice>;
}
/// Supplied by Lucid Preflight; reads and token accounting happen upstream.
pub trait Preflight {
    fn context(&self, objective: &str) -> Result<ContextManifest>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub reference: String,
    pub source_sha: String,
    pub result_sha: Option<String>,
    pub changed_paths: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub work_unit: String,
    pub provider: String,
    pub model: String,
    pub artifacts: Vec<Artifact>,
    pub usage: Budget,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub evidence_ref: String,
}
/// Trusted verifier output, never accepted from a worker as self-certification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub work_unit: String,
    pub verifier: String,
    pub checks: Vec<CheckResult>,
    pub acceptance: BTreeSet<String>,
    pub consistent: bool,
    pub semantic_approved: bool,
    pub timestamp_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub work_unit: Option<String>,
    pub kind: String,
    pub detail: String,
}
