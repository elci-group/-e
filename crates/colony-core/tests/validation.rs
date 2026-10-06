// SPDX-License-Identifier: MIT
//! Graph contract validation, primitives and wire strictness.
use colony_core::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Request {
    graph: WorkGraph,
}

fn graph() -> WorkGraph {
    let request: Request =
        serde_json::from_str(include_str!("../../../examples/request.json")).unwrap();
    request.graph
}

fn kind(edit: impl FnOnce(&mut WorkGraph)) -> FailureKind {
    let mut g = graph();
    edit(&mut g);
    g.validate().unwrap_err().kind
}

fn invalid(edit: impl FnOnce(&mut WorkGraph)) {
    assert_eq!(kind(edit), FailureKind::InvalidGraph);
}

#[test]
fn example_validates_in_deterministic_topological_order() {
    let order = graph().validate().unwrap();
    assert_eq!(
        order,
        [
            "dependency-audit",
            "platform-audit",
            "test-analysis",
            "synthesis"
        ]
    );
    let mut reversed = graph();
    reversed.nodes.reverse();
    assert_eq!(reversed.validate().unwrap(), order);
}

#[test]
fn graph_identity_and_policy_limits_required() {
    invalid(|g| g.colony_id = " ".into());
    invalid(|g| g.objective.clear());
    invalid(|g| g.nodes.clear());
    invalid(|g| g.policy.max_workers = 0);
    invalid(|g| g.policy.deadline_ms = 0);
    invalid(|g| g.policy.allowed_providers.clear());
}

#[test]
fn ids_must_be_unique_and_nonempty() {
    invalid(|g| g.nodes[1].id = g.nodes[0].id.clone());
    invalid(|g| g.nodes[0].id = "  ".into());
}

#[test]
fn contracts_must_be_bounded() {
    invalid(|g| g.nodes[0].objective.clear());
    invalid(|g| g.nodes[0].rationale.clear());
    invalid(|g| g.nodes[0].acceptance.clear());
    invalid(|g| g.nodes[0].expected_outputs.clear());
    invalid(|g| g.nodes[0].acceptance.push(" ".into()));
    invalid(|g| g.nodes[0].expected_outputs.push(String::new()));
    invalid(|g| {
        let first = g.nodes[0].expected_outputs[0].clone();
        g.nodes[0].expected_outputs.push(first);
    });
}

#[test]
fn scores_must_be_finite_unit_interval() {
    for bad in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        invalid(|g| g.nodes[0].demand.research = bad);
        invalid(|g| g.nodes[0].relational_density = bad);
        invalid(|g| g.nodes[0].uncertainty = bad);
        invalid(|g| g.nodes[0].consequence = bad);
    }
    assert!(score(0.0) && score(1.0) && !score(f64::NAN) && !score(-0.0001));
}

#[test]
fn zero_limits_rejected() {
    invalid(|g| g.nodes[0].budget.calls = 0);
    invalid(|g| g.nodes[0].demand.output_tokens = 0);
    invalid(|g| g.nodes[0].estimated_ms = 0);
}

#[test]
fn context_plus_output_must_fit_token_budget() {
    let mut g = graph();
    let n = &mut g.nodes[0];
    n.budget.tokens = n.context.input_tokens + n.demand.output_tokens;
    assert!(g.validate().is_ok());
    invalid(|g| {
        let n = &mut g.nodes[0];
        n.budget.tokens = n.context.input_tokens + n.demand.output_tokens - 1;
    });
    invalid(|g| g.nodes[0].demand.output_tokens = u64::MAX);
}

#[test]
fn source_sha_must_be_pinned_hex() {
    invalid(|g| g.nodes[0].context.source_sha = "a".repeat(39));
    invalid(|g| g.nodes[0].context.source_sha = "g".repeat(40));
    let mut g = graph();
    g.nodes[0].context.source_sha = "F".repeat(64);
    assert!(g.validate().is_ok());
    assert!(valid_sha(&"0".repeat(40)) && valid_sha(&"a".repeat(64)));
    assert!(!valid_sha("") && !valid_sha(&"a".repeat(41)) && !valid_sha("main"));
}

#[test]
fn path_safety() {
    for ok in ["src", "src/lib.rs", "a/b-c/d_e.f"] {
        assert!(valid_path(ok), "{ok}");
    }
    for bad in [
        "", "/etc", "a\\b", "c:", "a\0b", ".", "..", "a/./b", "a/../b", "a//b", "a/",
    ] {
        assert!(!valid_path(bad), "{bad:?}");
    }
    invalid(|g| g.nodes[0].context.relevant_paths.push("/abs".into()));
}

#[test]
fn verification_contract_required() {
    invalid(|g| g.nodes[0].verification.deterministic_checks.clear());
    invalid(|g| {
        g.nodes[0]
            .verification
            .deterministic_checks
            .push(" ".into())
    });
}

#[test]
fn dependencies_unique_known_and_acyclic() {
    invalid(|g| {
        let dep = g.nodes[3].dependencies[0].clone();
        g.nodes[3].dependencies.push(dep);
    });
    invalid(|g| g.nodes[0].dependencies.push("missing".into()));
    invalid(|g| g.nodes[0].dependencies.push("synthesis".into()));
    invalid(|g| g.nodes[0].dependencies.push("dependency-audit".into()));
}

#[test]
fn classification_cannot_downgrade_through_dependency() {
    assert_eq!(
        kind(|g| g.nodes[0].classification = Classification::Restricted),
        FailureKind::PolicyViolation
    );
    let mut g = graph();
    g.nodes[3].classification = Classification::Restricted;
    assert!(g.validate().is_ok());
}

#[test]
fn mutation_scope_must_be_safe() {
    let scope = |allowed: &[&str], forbidden: &[&str], branch: &str| {
        Some(Mutation {
            allowed_paths: allowed.iter().map(|s| s.to_string()).collect(),
            forbidden_paths: forbidden.iter().map(|s| s.to_string()).collect(),
            branch: branch.into(),
        })
    };
    let mut g = graph();
    g.nodes[0].mutation = scope(&["src"], &["src/secret"], "colony/x");
    assert!(g.validate().is_ok());
    invalid(|g| g.nodes[0].mutation = scope(&[], &[], "colony/x"));
    invalid(|g| g.nodes[0].mutation = scope(&["../up"], &[], "colony/x"));
    invalid(|g| g.nodes[0].mutation = scope(&["src"], &["/etc"], "colony/x"));
    invalid(|g| g.nodes[0].mutation = scope(&["src"], &[], "refs:heads"));
}

#[test]
fn unit_lookup() {
    let g = graph();
    assert_eq!(g.unit("synthesis").unwrap().id, "synthesis");
    assert_eq!(g.unit("nope").unwrap_err().kind, FailureKind::InvalidGraph);
}

#[test]
fn path_overlap_is_component_aware() {
    assert!(overlaps("src", "src"));
    assert!(overlaps("src", "src/a"));
    assert!(overlaps("src/a", "src"));
    assert!(!overlaps("src", "srcs"));
    assert!(!overlaps("src/a", "src/b"));
}

#[test]
fn budget_fits_and_checked_add() {
    let b = |m, t, c| Budget {
        money_micros: m,
        tokens: t,
        calls: c,
    };
    assert!(b(1, 1, 1).fits(b(1, 1, 1)));
    assert!(!b(2, 1, 1).fits(b(1, 1, 1)));
    assert!(!b(1, 2, 1).fits(b(1, 1, 1)));
    assert!(!b(1, 1, 2).fits(b(1, 1, 1)));
    assert_eq!(b(1, 2, 3).checked_add(b(4, 5, 6)).unwrap(), b(5, 7, 9));
    for over in [b(u64::MAX, 0, 0), b(0, u64::MAX, 0), b(0, 0, u64::MAX)] {
        assert_eq!(
            over.checked_add(b(1, 1, 1)).unwrap_err().kind,
            FailureKind::BudgetExhausted
        );
    }
}

#[test]
fn ensure_and_error_display() {
    assert!(ensure(true, FailureKind::Cancelled, "x").is_ok());
    let e = ensure(false, FailureKind::Cancelled, "stopped").unwrap_err();
    assert_eq!(e, Error::new(FailureKind::Cancelled, "stopped"));
    assert_eq!(e.to_string(), "Cancelled: stopped");
}

#[test]
fn wire_contracts_reject_unknown_fields() {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("../../../examples/request.json")).unwrap();
    value["graph"]["nodes"][0]["self_certified"] = true.into();
    assert!(serde_json::from_value::<WorkGraph>(value["graph"].clone()).is_err());
    assert!(
        serde_json::from_str::<Budget>(r#"{"money_micros":1,"tokens":1,"calls":1,"x":0}"#).is_err()
    );
    assert!(serde_json::from_str::<Evidence>(
        r#"{"work_unit":"a","verifier":"v","checks":[],"acceptance":[],"consistent":true,"semantic_approved":true,"timestamp_ms":0,"worker_says":"ok"}"#
    )
    .is_err());
}

#[test]
fn failure_kinds_serialize_snake_case() {
    assert_eq!(
        serde_json::to_string(&FailureKind::DeterministicValidationFailure).unwrap(),
        "\"deterministic_validation_failure\""
    );
    assert_eq!(
        serde_json::to_string(&Classification::LocalOnly).unwrap(),
        "\"LOCAL_ONLY\""
    );
}
