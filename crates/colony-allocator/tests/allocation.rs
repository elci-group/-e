// SPDX-License-Identifier: MIT
//! Hard eligibility filters, comparative-advantage scoring and deterministic tie-breaks.
use colony_allocator::*;
use colony_core::*;
use colony_provider::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Request {
    graph: WorkGraph,
    registry: Registry,
}

fn request() -> Request {
    serde_json::from_str(include_str!("../../../examples/request.json")).unwrap()
}

/// Mutates one unit, policy and resource to violate a single eligibility clause.
type Edit = fn(&mut WorkUnit, &mut Policy, &mut Resource);

fn rejection(edit: impl FnOnce(&mut WorkUnit, &mut Policy, &mut Resource)) -> FailureKind {
    let Request {
        mut graph,
        mut registry,
    } = request();
    let resource = &mut registry.resources[0];
    edit(&mut graph.nodes[0], &mut graph.policy, resource);
    eligible(&graph.nodes[0], &graph.policy, resource)
        .unwrap_err()
        .kind
}

#[test]
fn example_resources_are_eligible() {
    let r = request();
    for resource in &r.registry.resources {
        assert!(eligible(&r.graph.nodes[0], &r.graph.policy, resource).is_ok());
    }
}

#[test]
fn every_eligibility_clause_rejects_with_specific_kind() {
    use FailureKind::*;
    let cases: [(Edit, FailureKind); 10] = [
        (
            |_, p, _| p.max_classification = Classification::Public,
            PolicyViolation,
        ),
        (|_, p, _| p.allowed_providers.clear(), PolicyViolation),
        (
            |w, _, r| {
                w.classification = Classification::LocalOnly;
                r.local = false;
            },
            PolicyViolation,
        ),
        (
            |_, _, r| {
                r.classifications.remove(&Classification::Internal);
            },
            PolicyViolation,
        ),
        (|_, _, r| r.available = false, ProviderUnavailable),
        (|_, _, r| r.remaining_calls = 0, ProviderUnavailable),
        (
            |w, _, r| r.context_window = w.context.input_tokens,
            ContextOverflow,
        ),
        (
            |w, _, r| r.max_output = w.demand.output_tokens - 1,
            ContextOverflow,
        ),
        (|w, _, _| w.budget.money_micros = 3, BudgetExhausted),
        (|_, p, _| p.budget.money_micros = 0, BudgetExhausted),
    ];
    for (i, (edit, expected)) in cases.into_iter().enumerate() {
        assert_eq!(rejection(edit), expected, "clause {i}");
    }
}

#[test]
fn exact_fit_is_eligible() {
    let Request {
        mut graph,
        mut registry,
    } = request();
    let (w, r) = (&mut graph.nodes[0], &mut registry.resources[0]);
    r.context_window = w.context.input_tokens + w.demand.output_tokens;
    r.max_output = w.demand.output_tokens;
    w.budget.money_micros = r.cost(w).unwrap();
    assert!(eligible(w, &graph.policy, r).is_ok());
}

#[test]
fn comparative_advantage_selects_best_fit() {
    let r = request();
    let a = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    assert_eq!(
        (a.work_unit.as_str(), a.model.as_str()),
        ("dependency-audit", "research")
    );
    assert_eq!(a.candidates.len(), 3);
    assert_eq!(a.candidates[0].model, "research");
    let scores: Vec<_> = a.candidates.iter().map(|c| c.score.unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    assert!(a
        .candidates
        .iter()
        .all(|c| c.estimated_cost_micros == Some(4)));

    let mut coding = r.graph.nodes[0].clone();
    coding.demand.coding = 1.0;
    coding.demand.research = 0.0;
    let a = allocate(&coding, &r.graph.policy, &r.registry).unwrap();
    assert_eq!(a.model, "code");
}

#[test]
fn scarcity_and_shadow_price_shift_choice() {
    let mut r = request();
    r.registry.resources[0].shadow_price = 10.0;
    let a = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    assert_ne!(a.model, "research");
    let mut r = request();
    r.registry.resources[0].remaining_calls = 1;
    let a = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    assert_ne!(a.model, "research");
}

#[test]
fn zero_demand_is_finite() {
    let mut r = request();
    let w = &mut r.graph.nodes[0];
    w.demand.reasoning = 0.0;
    w.demand.coding = 0.0;
    w.demand.research = 0.0;
    w.demand.structured_output = 0.0;
    w.demand.tool_use = 0.0;
    let a = allocate(w, &r.graph.policy, &r.registry).unwrap();
    assert!(a.candidates.iter().all(|c| c.score.unwrap().is_finite()));
}

#[test]
fn ties_break_by_provider_then_model_regardless_of_order() {
    let mut r = request();
    let template = r.registry.resources[0].clone();
    r.registry.resources = ["zeta", "alpha", "mid"]
        .into_iter()
        .map(|model| Resource {
            model: model.into(),
            ..template.clone()
        })
        .collect();
    let a = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    assert_eq!(a.model, "alpha");
    let order: Vec<_> = a.candidates.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(order, ["alpha", "mid", "zeta"]);
    r.registry.resources.reverse();
    let b = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    assert_eq!(a.model, b.model);
}

#[test]
fn ineligible_candidates_are_explained_and_ranked_last() {
    let mut r = request();
    r.registry.resources[0].available = false;
    let a = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap();
    let last = a.candidates.last().unwrap();
    assert_eq!((last.model.as_str(), last.score), ("research", None));
    assert!(last.reasons[0].contains("unavailable"));
}

#[test]
fn no_eligible_provider_enumerates_reasons() {
    let mut r = request();
    for resource in &mut r.registry.resources {
        resource.available = false;
    }
    let err = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap_err();
    assert_eq!(err.kind, FailureKind::ProviderUnavailable);
    for model in ["research", "code", "reasoning"] {
        assert!(
            err.message.contains(&format!("example-local/{model}")),
            "{model}"
        );
    }
}

#[test]
fn invalid_registry_rejected_before_scoring() {
    let mut r = request();
    r.registry.resources[0].shadow_price = 0.5;
    let err = allocate(&r.graph.nodes[0], &r.graph.policy, &r.registry).unwrap_err();
    assert_eq!(err.kind, FailureKind::ContractViolation);
}
