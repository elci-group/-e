// SPDX-License-Identifier: MIT
//! Lifecycle gates, failure-kind precision and event taxonomy of the control plane.
use colony_core::*;
use colony_planner::{plan, Plan};
use colony_provider::Registry;
use colony_runtime::*;
use serde::Deserialize;
use FailureKind::*;

#[derive(Deserialize)]
struct Request {
    graph: WorkGraph,
    ontology: OntologySlice,
    registry: Registry,
}

fn request() -> Request {
    serde_json::from_str(include_str!("../../../examples/request.json")).unwrap()
}

fn colony_with(edit: impl FnOnce(&mut Request)) -> Colony {
    let mut r = request();
    edit(&mut r);
    Colony::new(plan(r.graph, r.ontology).unwrap(), r.registry).unwrap()
}

fn colony() -> Colony {
    colony_with(|_| {})
}

fn mutating(r: &mut Request) {
    r.graph.policy.allow_mutation = true;
    r.graph.nodes[0].mutation = Some(Mutation {
        allowed_paths: vec!["src".into()],
        forbidden_paths: vec![],
        branch: "colony/audit".into(),
    });
}

#[derive(Default)]
struct Executor {
    submitted: Vec<u64>,
    cancelled: Vec<String>,
    reject_submit: bool,
    reject_cancel: bool,
}

impl Mesut for Executor {
    fn submit(&mut self, d: &Dispatch) -> Result<String> {
        if self.reject_submit {
            return Err(Error::new(InfrastructureFailure, "queue offline"));
        }
        self.submitted.push(d.attempt);
        Ok(format!("job-{}", d.attempt))
    }
    fn cancel(&mut self, handle: &str) -> Result<()> {
        if self.reject_cancel {
            return Err(Error::new(InfrastructureFailure, "cancel unavailable"));
        }
        self.cancelled.push(handle.into());
        Ok(())
    }
}

fn evidence(u: &WorkUnit) -> Evidence {
    Evidence {
        work_unit: u.id.clone(),
        verifier: "trusted".into(),
        checks: u
            .verification
            .deterministic_checks
            .iter()
            .map(|n| CheckResult {
                name: n.clone(),
                passed: true,
                evidence_ref: format!("ci://{n}"),
            })
            .collect(),
        acceptance: u.acceptance.iter().cloned().collect(),
        consistent: true,
        semantic_approved: true,
        timestamp_ms: 0,
    }
}

/// Produces trusted evidence, then lets a test tamper with it.
struct Checker(fn(&mut Evidence));

impl Verifier for Checker {
    fn verify(&mut self, u: &WorkUnit, _: &WorkerResult) -> Result<Evidence> {
        let mut e = evidence(u);
        (self.0)(&mut e);
        Ok(e)
    }
}

fn trusted() -> Checker {
    Checker(|_| {})
}

struct Broken;

impl Verifier for Broken {
    fn verify(&mut self, _: &WorkUnit, _: &WorkerResult) -> Result<Evidence> {
        Err(Error::new(InfrastructureFailure, "verifier crashed"))
    }
}

fn result(d: &Dispatch) -> WorkerResult {
    WorkerResult {
        work_unit: d.unit.id.clone(),
        provider: d.assignment.provider.clone(),
        model: d.assignment.model.clone(),
        usage: Budget {
            money_micros: 4,
            tokens: 3000,
            calls: 1,
        },
        artifacts: d
            .unit
            .expected_outputs
            .iter()
            .map(|name| Artifact {
                name: name.clone(),
                reference: format!("store://{name}"),
                source_sha: d.unit.context.source_sha.clone(),
                result_sha: d.unit.mutation.as_ref().map(|_| "c".repeat(40)),
                changed_paths: d
                    .unit
                    .mutation
                    .as_ref()
                    .map(|_| vec!["src/lib.rs".into()])
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

const ID: &str = "dependency-audit";

type ResultEdit = fn(&mut WorkerResult);
type EvidenceEdit = fn(&mut Evidence);

fn running(c: &mut Colony, x: &mut Executor) -> Dispatch {
    c.dispatch(ID, 1, 3, true, x).unwrap()
}

fn awaiting(c: &mut Colony, x: &mut Executor) -> Dispatch {
    let d = running(c, x);
    c.receive(d.attempt, result(&d), 2).unwrap();
    d
}

fn kinds(c: &Colony) -> Vec<String> {
    c.snapshot().events.into_iter().map(|e| e.kind).collect()
}

fn run_to_completion(c: &mut Colony) {
    let mut x = Executor::default();
    let mut now = 1;
    while !c.complete() {
        for id in c.ready() {
            let d = c.dispatch(&id, now, 3, false, &mut x).unwrap();
            c.receive(d.attempt, result(&d), now).unwrap();
            c.verify(&id, now, &mut trusted()).unwrap();
            now += 1;
        }
    }
}

#[test]
fn full_lifecycle_emits_ordered_event_taxonomy() {
    let mut c = colony();
    run_to_completion(&mut c);
    let s = c.snapshot();
    assert!(s.completed && !s.cancelling);
    assert!(s.states.values().all(|st| *st == State::Validated));
    assert_eq!(s.reserved.calls, 4);
    assert_eq!(c.evidence().len(), 4);
    assert_eq!(c.results().len(), 4);
    assert!(s
        .events
        .iter()
        .enumerate()
        .all(|(i, e)| e.sequence == i as u64));
    assert!(s
        .events
        .windows(2)
        .all(|w| w[0].timestamp_ms <= w[1].timestamp_ms));
    let k = kinds(&c);
    assert_eq!(k.first().map(String::as_str), Some("ColonyCreated"));
    assert_eq!(k.last().map(String::as_str), Some("ColonyCompleted"));
    for (kind, count) in [
        ("WorkerAssigned", 4),
        ("ArtifactProduced", 4),
        ("ValidationStarted", 4),
        ("WorkerCompleted", 4),
        ("ColonyCompleted", 1),
    ] {
        assert_eq!(k.iter().filter(|x| *x == kind).count(), count, "{kind}");
    }
}

#[test]
fn lifecycle_is_deterministic() {
    let mut a = colony();
    let mut b = colony();
    run_to_completion(&mut a);
    run_to_completion(&mut b);
    assert_eq!(
        serde_json::to_string(&a.snapshot()).unwrap(),
        serde_json::to_string(&b.snapshot()).unwrap()
    );
}

#[test]
fn admission_revalidates_plan_and_registry() {
    let mut r = request();
    r.registry.resources[0].concurrency = 0;
    let p = plan(r.graph, r.ontology).unwrap();
    assert_eq!(
        Colony::new(p, r.registry).err().unwrap().kind,
        ContractViolation
    );
    let r = request();
    let mut p: Plan = plan(r.graph, r.ontology).unwrap();
    p.graph.nodes[0].dependencies.push("synthesis".into());
    assert_eq!(Colony::new(p, r.registry).err().unwrap().kind, InvalidGraph);
}

#[test]
fn dispatch_gates_report_precise_kinds() {
    let mut x = Executor::default();
    let mut c = colony();
    assert_eq!(
        c.dispatch("synthesis", 1, 3, false, &mut x)
            .unwrap_err()
            .kind,
        DependencyFailure
    );
    assert_eq!(
        c.dispatch("ghost", 1, 3, false, &mut x).unwrap_err().kind,
        DependencyFailure
    );
    assert_eq!(
        c.dispatch(ID, 1, 0, false, &mut x).unwrap_err().kind,
        ProviderThrottled
    );
    let mut c = colony_with(|r| r.graph.policy.max_workers = 1);
    c.dispatch(ID, 1, 3, false, &mut x).unwrap();
    assert_eq!(
        c.dispatch("platform-audit", 1, 3, false, &mut x)
            .unwrap_err()
            .kind,
        ProviderThrottled
    );
    let mut c = colony_with(|r| r.graph.policy.deadline_ms = 10);
    assert_eq!(
        c.dispatch(ID, 10, 3, false, &mut x).unwrap_err().kind,
        Cancelled
    );
    assert!(
        x.submitted.len() == 1,
        "only the admitted dispatch reached Mesut"
    );
}

#[test]
fn mutation_gates() {
    let mut x = Executor::default();
    let mut c = colony_with(mutating);
    assert_eq!(
        c.dispatch(ID, 1, 3, false, &mut x).unwrap_err().kind,
        PolicyViolation
    );
    let mut c = colony_with(|r| {
        mutating(r);
        r.graph.policy.require_human_approval = false;
    });
    assert!(c.dispatch(ID, 1, 3, false, &mut x).is_ok());
    let mut c = colony_with(|r| {
        mutating(r);
        r.ontology.locked_entities.insert(ID.into());
    });
    assert_eq!(
        c.dispatch(ID, 1, 3, true, &mut x).unwrap_err().kind,
        SemanticConflict
    );
    // Locks only gate mutation; read-only work on a locked entity proceeds.
    let mut c = colony_with(|r| {
        r.ontology.locked_entities.insert(ID.into());
    });
    assert!(c.dispatch(ID, 1, 3, false, &mut x).is_ok());
}

#[test]
fn per_resource_concurrency_spreads_load() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| {
        for res in &mut r.registry.resources {
            res.concurrency = 1;
        }
    });
    let models: std::collections::BTreeSet<_> =
        ["dependency-audit", "platform-audit", "test-analysis"]
            .into_iter()
            .map(|id| {
                c.dispatch(id, 1, 3, false, &mut x)
                    .unwrap()
                    .assignment
                    .model
            })
            .collect();
    assert_eq!(models.len(), 3);
}

#[test]
fn allocation_failure_reserves_nothing() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| {
        r.registry.resources.truncate(1);
        r.registry.resources[0].concurrency = 1;
    });
    c.dispatch(ID, 1, 3, false, &mut x).unwrap();
    assert_eq!(
        c.dispatch("platform-audit", 1, 3, false, &mut x)
            .unwrap_err()
            .kind,
        ProviderUnavailable
    );
    assert_eq!(c.snapshot().reserved.calls, 1);
    assert_eq!(c.snapshot().states["platform-audit"], State::Pending);
}

#[test]
fn quota_is_consumed_per_submitted_call() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| {
        r.registry.resources.truncate(1);
        r.registry.resources[0].remaining_calls = 1;
    });
    let d = c.dispatch(ID, 1, 3, false, &mut x).unwrap();
    c.fail(ID, d.attempt, Error::new(ProviderUnavailable, "x"), 2)
        .unwrap();
    c.repair(ID).unwrap();
    assert_eq!(
        c.dispatch(ID, 3, 3, false, &mut x).unwrap_err().kind,
        ProviderUnavailable
    );
}

#[test]
fn rejected_submit_keeps_reservation_but_not_quota() {
    let mut x = Executor {
        reject_submit: true,
        ..Executor::default()
    };
    let mut c = colony_with(|r| {
        r.registry.resources.truncate(1);
        r.registry.resources[0].remaining_calls = 1;
    });
    assert_eq!(
        c.dispatch(ID, 1, 3, false, &mut x).unwrap_err().kind,
        InfrastructureFailure
    );
    let s = c.snapshot();
    assert_eq!((s.states[ID], s.reserved.calls), (State::Pending, 1));
    x.reject_submit = false;
    let d = c.dispatch(ID, 2, 3, false, &mut x).unwrap();
    assert_eq!(d.attempt, 2, "attempt ids are never reused");
    assert_eq!(c.snapshot().reserved.calls, 2);
}

#[test]
fn results_must_match_a_running_attempt() {
    let mut x = Executor::default();
    let mut c = colony();
    let d = running(&mut c, &mut x);
    let mut ghost = result(&d);
    ghost.work_unit = "ghost".into();
    assert_eq!(
        c.receive(d.attempt, ghost, 2).unwrap_err().kind,
        ContractViolation
    );
    let mut model = result(&d);
    model.model = "other".into();
    assert_eq!(
        c.receive(d.attempt, model, 2).unwrap_err().kind,
        ContractViolation
    );
    c.receive(d.attempt, result(&d), 2).unwrap();
    assert_eq!(
        c.receive(d.attempt, result(&d), 3).unwrap_err().kind,
        ContractViolation,
        "duplicate delivery"
    );
}

#[test]
fn superseded_attempt_cannot_deliver() {
    let mut x = Executor::default();
    let mut c = colony();
    let first = running(&mut c, &mut x);
    c.fail(ID, first.attempt, Error::new(ProviderThrottled, "429"), 2)
        .unwrap();
    c.repair(ID).unwrap();
    let second = c.dispatch(ID, 3, 3, false, &mut x).unwrap();
    assert!(second.attempt > first.attempt);
    assert_eq!(second.repair_events.len(), 1);
    assert_eq!(second.repair_events[0].kind, "WorkerFailed");
    assert_eq!(
        c.receive(first.attempt, result(&first), 4)
            .unwrap_err()
            .kind,
        ContractViolation
    );
    c.receive(second.attempt, result(&second), 4).unwrap();
}

#[test]
fn late_result_after_deadline_is_cancelled() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| r.graph.policy.deadline_ms = 10);
    let d = running(&mut c, &mut x);
    assert_eq!(
        c.receive(d.attempt, result(&d), 10).unwrap_err().kind,
        Cancelled
    );
}

fn artifact_rejection(edit: fn(&mut WorkerResult), setup: fn(&mut Request)) -> FailureKind {
    let mut x = Executor::default();
    let mut c = colony_with(setup);
    let d = running(&mut c, &mut x);
    let mut r = result(&d);
    edit(&mut r);
    let kind = c.receive(d.attempt, r, 2).unwrap_err().kind;
    assert_eq!(c.snapshot().states[ID], State::Rejected);
    assert_eq!(
        kinds(&c).last().map(String::as_str),
        Some("ValidationFailed")
    );
    kind
}

#[test]
fn artifact_coverage_and_provenance() {
    let read_only: fn(&mut Request) = |_| {};
    let cases: [(ResultEdit, FailureKind); 6] = [
        (
            |r| {
                let dup = r.artifacts[0].clone();
                r.artifacts.push(dup);
            },
            InvalidArtifact,
        ),
        (
            |r| {
                let mut extra = r.artifacts[0].clone();
                extra.name = "bonus.json".into();
                r.artifacts.push(extra);
            },
            InvalidArtifact,
        ),
        (
            |r| r.artifacts[0].name = "renamed.json".into(),
            InvalidArtifact,
        ),
        (|r| r.artifacts[0].reference = " ".into(), InvalidArtifact),
        (
            |r| r.artifacts[0].source_sha = "d".repeat(40),
            InvalidArtifact,
        ),
        (
            |r| r.artifacts[0].changed_paths = vec!["src/lib.rs".into()],
            ContractViolation,
        ),
    ];
    for (i, (edit, expected)) in cases.into_iter().enumerate() {
        assert_eq!(artifact_rejection(edit, read_only), expected, "case {i}");
    }
}

#[test]
fn mutation_artifacts_need_result_revision_and_scope() {
    let approve = |r: &mut Request| {
        mutating(r);
        r.graph.policy.require_human_approval = false;
    };
    let mut x = Executor::default();
    let mut c = colony_with(approve);
    let d = running(&mut c, &mut x);
    c.receive(d.attempt, result(&d), 2).unwrap();
    for edit in [
        (|r: &mut WorkerResult| r.artifacts[0].result_sha = None) as fn(&mut WorkerResult),
        |r| r.artifacts[0].result_sha = Some("short".into()),
        |r| r.artifacts[0].changed_paths = vec!["/etc/passwd".into()],
        |r| r.artifacts[0].changed_paths = vec!["srcs/lib.rs".into()],
    ] {
        assert_eq!(artifact_rejection(edit, approve), ContractViolation);
    }
}

fn evidence_rejection(tamper: fn(&mut Evidence)) -> FailureKind {
    let mut x = Executor::default();
    let mut c = colony();
    awaiting(&mut c, &mut x);
    let kind = c.verify(ID, 3, &mut Checker(tamper)).unwrap_err().kind;
    assert_eq!(c.snapshot().states[ID], State::Rejected);
    assert!(c.evidence().is_empty());
    kind
}

#[test]
fn evidence_gates() {
    let cases: [(EvidenceEdit, FailureKind); 10] = [
        (|e| e.work_unit = "other".into(), ConvergenceFailure),
        (|e| e.verifier = " ".into(), ConvergenceFailure),
        (|e| e.consistent = false, ConvergenceFailure),
        (|e| e.semantic_approved = false, ConvergenceFailure),
        (|e| e.timestamp_ms = 4, ConvergenceFailure),
        (|e| e.acceptance.clear(), ConvergenceFailure),
        (|e| e.checks.clear(), DeterministicValidationFailure),
        (
            |e| e.checks[0].passed = false,
            DeterministicValidationFailure,
        ),
        (
            |e| e.checks[0].evidence_ref.clear(),
            DeterministicValidationFailure,
        ),
        (
            |e| {
                e.checks.push(CheckResult {
                    name: "extra-lint".into(),
                    passed: false,
                    evidence_ref: "ci://lint".into(),
                })
            },
            DeterministicValidationFailure,
        ),
    ];
    for (i, (tamper, expected)) in cases.into_iter().enumerate() {
        assert_eq!(evidence_rejection(tamper), expected, "case {i}");
    }
}

#[test]
fn semantic_approval_only_required_when_contracted() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| r.graph.nodes[0].verification.semantic_review = false);
    awaiting(&mut c, &mut x);
    c.verify(ID, 3, &mut Checker(|e| e.semantic_approved = false))
        .unwrap();
    assert_eq!(c.snapshot().states[ID], State::Validated);
}

#[test]
fn verifier_errors_propagate_and_reject() {
    let mut x = Executor::default();
    let mut c = colony();
    awaiting(&mut c, &mut x);
    assert_eq!(
        c.verify(ID, 3, &mut Broken).unwrap_err().kind,
        InfrastructureFailure
    );
    assert_eq!(c.snapshot().states[ID], State::Rejected);
}

#[test]
fn verify_requires_pending_artifact_and_live_colony() {
    let mut x = Executor::default();
    let mut c = colony();
    assert_eq!(
        c.verify(ID, 1, &mut trusted()).unwrap_err().kind,
        ContractViolation
    );
    running(&mut c, &mut x);
    assert_eq!(
        c.verify(ID, 2, &mut trusted()).unwrap_err().kind,
        ContractViolation
    );
    let mut c = colony();
    awaiting(&mut c, &mut x);
    c.cancel(3, &mut x).unwrap();
    assert_eq!(c.verify(ID, 4, &mut trusted()).unwrap_err().kind, Cancelled);
}

#[test]
fn fail_and_repair_contracts() {
    let mut x = Executor::default();
    let mut c = colony();
    assert_eq!(
        c.fail(ID, 1, Error::new(ProviderUnavailable, "x"), 1)
            .unwrap_err()
            .kind,
        ContractViolation
    );
    assert_eq!(c.repair(ID).unwrap_err().kind, ContractViolation);
    let d = running(&mut c, &mut x);
    assert_eq!(
        c.fail(ID, d.attempt + 1, Error::new(ProviderUnavailable, "x"), 2)
            .unwrap_err()
            .kind,
        ContractViolation
    );
    c.fail(ID, d.attempt, Error::new(ProviderUnavailable, "x"), 2)
        .unwrap();
    assert_eq!(c.snapshot().states[ID], State::Rejected);
    assert!(!c.ready().contains(&ID.to_string()));
    c.repair(ID).unwrap();
    assert_eq!(
        kinds(&c).last().map(String::as_str),
        Some("RepairScheduled")
    );
    assert!(c.ready().contains(&ID.to_string()));
    c.cancel(3, &mut x).unwrap();
    assert_eq!(c.repair(ID).unwrap_err().kind, ContractViolation);
}

#[test]
fn cancel_preserves_validated_work() {
    let mut x = Executor::default();
    let mut c = colony();
    awaiting(&mut c, &mut x);
    c.verify(ID, 3, &mut trusted()).unwrap();
    let d = c.dispatch("platform-audit", 4, 3, false, &mut x).unwrap();
    c.cancel(5, &mut x).unwrap();
    let s = c.snapshot();
    assert_eq!(s.states[ID], State::Validated);
    assert_eq!(s.states["platform-audit"], State::Cancelled);
    assert_eq!(s.states["synthesis"], State::Cancelled);
    assert_eq!(x.cancelled, [format!("job-{}", d.attempt)]);
    assert_eq!(
        kinds(&c).last().map(String::as_str),
        Some("ColonyCancelled")
    );
}

#[test]
fn deadline_tick_cancels_once_and_retries_pending_acks() {
    let mut x = Executor::default();
    let mut c = colony_with(|r| r.graph.policy.deadline_ms = 10);
    running(&mut c, &mut x);
    c.tick(9, &mut x).unwrap();
    assert!(!c.snapshot().cancelling, "tick before deadline is a no-op");
    x.reject_cancel = true;
    assert_eq!(c.tick(10, &mut x).unwrap_err().kind, InfrastructureFailure);
    assert_eq!(
        kinds(&c).last().map(String::as_str),
        Some("CancellationPending")
    );
    x.reject_cancel = false;
    c.tick(11, &mut x).unwrap();
    for now in 12..20 {
        c.tick(now, &mut x).unwrap();
    }
    let k = kinds(&c);
    assert_eq!(k.iter().filter(|e| *e == "ColonyCancelled").count(), 1);
    assert_eq!(x.cancelled.len(), 1);
}

#[test]
fn completed_colony_is_not_cancelled_by_deadline() {
    let mut c = colony_with(|r| r.graph.policy.deadline_ms = 100);
    run_to_completion(&mut c);
    let mut x = Executor::default();
    c.tick(100, &mut x).unwrap();
    assert!(c.complete() && !c.snapshot().cancelling);
}

#[test]
fn clock_must_be_monotonic_everywhere() {
    let mut x = Executor::default();
    let mut c = colony();
    let d = c.dispatch(ID, 5, 3, false, &mut x).unwrap();
    assert_eq!(
        c.receive(d.attempt, result(&d), 4).unwrap_err().kind,
        ContractViolation
    );
    assert_eq!(
        c.fail(ID, d.attempt, Error::new(ProviderUnavailable, "x"), 4)
            .unwrap_err()
            .kind,
        ContractViolation
    );
    c.receive(d.attempt, result(&d), 6).unwrap();
    assert_eq!(
        c.verify(ID, 5, &mut trusted()).unwrap_err().kind,
        ContractViolation
    );
    assert_eq!(c.cancel(5, &mut x).unwrap_err().kind, ContractViolation);
}
