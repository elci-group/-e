// SPDX-License-Identifier: MIT
//! Authority narrowing, unit admission and hierarchical ledgers.
use colony_core::*;
use colony_policy::*;
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

fn budget(money_micros: u64, tokens: u64, calls: u64) -> Budget {
    Budget {
        money_micros,
        tokens,
        calls,
    }
}

#[test]
fn unit_admission() {
    let g = graph();
    let unit = &g.nodes[0];
    assert!(validate_unit(unit, &g.policy).is_ok());

    let mut p = g.policy.clone();
    p.max_classification = Classification::Public;
    assert_eq!(
        validate_unit(unit, &p).unwrap_err().kind,
        FailureKind::PolicyViolation
    );

    let mut mutating = unit.clone();
    mutating.mutation = Some(Mutation {
        allowed_paths: vec!["src".into()],
        forbidden_paths: vec![],
        branch: "colony/x".into(),
    });
    assert_eq!(
        validate_unit(&mutating, &g.policy).unwrap_err().kind,
        FailureKind::PolicyViolation
    );
    let mut p = g.policy.clone();
    p.allow_mutation = true;
    assert!(validate_unit(&mutating, &p).is_ok());

    let mut p = g.policy.clone();
    p.budget.tokens = unit.budget.tokens - 1;
    assert_eq!(
        validate_unit(unit, &p).unwrap_err().kind,
        FailureKind::BudgetExhausted
    );
}

fn parent() -> Policy {
    let mut p = graph().policy;
    p.max_depth = 2;
    p.allow_mutation = true;
    p.allowed_providers.insert("second".into());
    p
}

fn narrowed() -> Policy {
    let mut c = parent();
    c.max_depth = 1;
    c
}

#[test]
fn inherit_accepts_strict_narrowing() {
    let mut c = narrowed();
    assert!(inherit(&parent(), &c).is_ok());
    c.allowed_providers.remove("second");
    c.max_classification = Classification::Public;
    c.allow_mutation = false;
    c.max_workers = 1;
    c.deadline_ms = 1;
    c.budget = Budget::default();
    assert!(inherit(&parent(), &c).is_ok());
}

#[test]
fn inherit_rejects_every_broadening_clause() {
    let broaden: [fn(&mut Policy, &mut Policy); 9] = [
        |_, c| {
            c.allowed_providers.insert("rogue".into());
        },
        |p, c| {
            p.max_classification = Classification::Internal;
            c.max_classification = Classification::Restricted;
        },
        |p, c| {
            p.allow_mutation = false;
            c.allow_mutation = true;
        },
        |p, c| {
            p.require_human_approval = true;
            c.require_human_approval = false;
        },
        |_, c| c.max_workers = 0,
        |p, c| c.max_workers = p.max_workers + 1,
        |p, c| c.max_depth = p.max_depth,
        |p, c| c.deadline_ms = p.deadline_ms + 1,
        |p, c| c.budget.calls = p.budget.calls + 1,
    ];
    for (i, edit) in broaden.into_iter().enumerate() {
        let (mut p, mut c) = (parent(), narrowed());
        edit(&mut p, &mut c);
        assert_eq!(
            inherit(&p, &c).unwrap_err().kind,
            FailureKind::PolicyViolation,
            "clause {i}"
        );
    }
}

#[test]
fn ledger_reserves_up_to_limit_without_refunds() {
    let mut l = Ledger::new(budget(10, 10, 2));
    l.reserve(budget(5, 5, 1)).unwrap();
    l.reserve(budget(5, 5, 1)).unwrap();
    assert_eq!(l.reserved(), budget(10, 10, 2));
    let err = l.reserve(budget(0, 0, 1)).unwrap_err();
    assert_eq!(err.kind, FailureKind::BudgetExhausted);
    assert_eq!(l.reserved(), budget(10, 10, 2), "failed reserve is atomic");
}

#[test]
fn ancestors_reserve_all_or_nothing() {
    let mut root = Ledger::new(budget(10, 10, 10));
    let mut child = Ledger::new(budget(5, 5, 5));
    reserve_ancestors(&mut [&mut root, &mut child], budget(5, 5, 5)).unwrap();
    assert_eq!(root.reserved(), budget(5, 5, 5));
    assert_eq!(child.reserved(), budget(5, 5, 5));
    assert_eq!(
        reserve_ancestors(&mut [&mut root, &mut child], budget(1, 0, 0))
            .unwrap_err()
            .kind,
        FailureKind::BudgetExhausted
    );
    assert_eq!(root.reserved(), budget(5, 5, 5));

    let mut overflow = Ledger::new(budget(u64::MAX, 0, 0));
    overflow.reserve(budget(u64::MAX, 0, 0)).unwrap();
    assert_eq!(
        reserve_ancestors(&mut [&mut root, &mut overflow], budget(1, 0, 0))
            .unwrap_err()
            .kind,
        FailureKind::BudgetExhausted
    );
    assert_eq!(root.reserved(), budget(5, 5, 5));
}

#[test]
fn ancestors_required() {
    assert_eq!(
        reserve_ancestors(&mut [], Budget::default())
            .unwrap_err()
            .kind,
        FailureKind::PolicyViolation
    );
}
