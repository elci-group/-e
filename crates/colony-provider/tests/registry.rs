// SPDX-License-Identifier: MIT
//! Registry contracts, capability provenance and integer cost estimation.
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

fn contract_violation(edit: impl FnOnce(&mut Resource)) {
    let mut r = request().registry;
    edit(&mut r.resources[0]);
    assert_eq!(
        r.validate().unwrap_err().kind,
        FailureKind::ContractViolation
    );
}

fn caps(v: f64) -> Capabilities {
    Capabilities {
        reasoning: v,
        coding: v,
        research: v,
        structured_output: v,
        tool_use: v,
    }
}

#[test]
fn example_registry_is_valid() {
    assert!(request().registry.validate().is_ok());
}

#[test]
fn identity_must_be_nonempty_and_unique() {
    contract_violation(|r| r.provider = " ".into());
    contract_violation(|r| r.model.clear());
    let mut r = request().registry;
    let duplicate = r.resources[0].clone();
    r.resources.push(duplicate);
    assert_eq!(
        r.validate().unwrap_err().kind,
        FailureKind::ContractViolation
    );
}

#[test]
fn limits_must_be_positive_and_shadow_price_at_least_one() {
    contract_violation(|r| r.context_window = 0);
    contract_violation(|r| r.max_output = 0);
    contract_violation(|r| r.concurrency = 0);
    contract_violation(|r| r.latency_ms = 0);
    contract_violation(|r| r.shadow_price = 0.99);
    contract_violation(|r| r.shadow_price = f64::NAN);
    contract_violation(|r| r.shadow_price = f64::INFINITY);
}

#[test]
fn capabilities_must_be_scores() {
    contract_violation(|r| r.declared.coding = 1.5);
    contract_violation(|r| r.benchmarked = Some(caps(f64::NAN)));
    contract_violation(|r| {
        r.observed = Some(Observed {
            capabilities: caps(-0.1),
            samples: 1,
            validated_success_rate: 1.0,
        })
    });
    contract_violation(|r| {
        r.observed = Some(Observed {
            capabilities: caps(0.5),
            samples: 1,
            validated_success_rate: 2.0,
        })
    });
}

#[test]
fn lookup_by_provider_and_model() {
    let r = request().registry;
    assert_eq!(r.get("example-local", "code").unwrap().model, "code");
    assert_eq!(r.resources[0].key(), "example-local/research");
    assert_eq!(
        r.get("example-local", "absent").unwrap_err().kind,
        FailureKind::ProviderUnavailable
    );
}

#[test]
fn capability_provenance_and_observed_blend() {
    let mut r = request().registry.resources[0].clone();
    r.declared = caps(0.2);
    r.benchmarked = None;
    let (c, confidence, source) = r.effective();
    assert_eq!((c.coding, confidence, source), (0.2, 0.5, "declared"));

    r.benchmarked = Some(caps(0.6));
    let (c, confidence, source) = r.effective();
    assert_eq!((c.coding, confidence, source), (0.6, 0.8, "benchmarked"));

    // Zero samples carry no evidence and fall back to the benchmark.
    r.observed = Some(Observed {
        capabilities: caps(1.0),
        samples: 0,
        validated_success_rate: 1.0,
    });
    assert_eq!(r.effective().2, "benchmarked");

    // weight = 10 / (10 + 10) = 0.5 → 0.6·0.5 + 1.0·0.5 = 0.8
    r.observed = Some(Observed {
        capabilities: caps(1.0),
        samples: 10,
        validated_success_rate: 0.5,
    });
    let (c, confidence, source) = r.effective();
    assert!((c.reasoning - 0.8).abs() < 1e-12);
    assert_eq!((confidence, source), (0.75, "observed blend"));

    // Weight is capped at 0.95 so priors never vanish entirely.
    r.observed = Some(Observed {
        capabilities: caps(1.0),
        samples: u64::MAX,
        validated_success_rate: 1.0,
    });
    assert!((r.effective().0.tool_use - (0.6 * 0.05 + 0.95)).abs() < 1e-12);
}

#[test]
fn cost_is_integer_rounded_up_and_overflow_checked() {
    let request = request();
    let unit = &request.graph.nodes[0];
    let mut r = request.registry.resources[0].clone();
    // 2000 input · 1000/M + 1000 output · 2000/M = 2 + 2 = 4 micros.
    assert_eq!(r.cost(unit).unwrap(), 4);
    r.input_micros_per_million = 0;
    r.output_micros_per_million = 0;
    assert_eq!(r.cost(unit).unwrap(), 0);
    r.output_micros_per_million = 1;
    assert_eq!(r.cost(unit).unwrap(), 1);

    let mut huge = unit.clone();
    huge.context.input_tokens = u64::MAX;
    huge.demand.output_tokens = u64::MAX;
    r.input_micros_per_million = u64::MAX;
    r.output_micros_per_million = u64::MAX;
    assert_eq!(
        r.cost(&huge).unwrap_err().kind,
        FailureKind::BudgetExhausted
    );
}
