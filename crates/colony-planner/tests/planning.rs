// SPDX-License-Identifier: MIT
//! Swarm coupling, semantic edge injection, critical path and admission checks.
use colony_core::*;
use colony_planner::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Request {
    graph: WorkGraph,
    ontology: OntologySlice,
}

fn request() -> Request {
    serde_json::from_str(include_str!("../../../examples/request.json")).unwrap()
}

fn planned(edit: impl FnOnce(&mut WorkGraph, &mut OntologySlice)) -> Result<Plan> {
    let Request {
        mut graph,
        mut ontology,
    } = request();
    edit(&mut graph, &mut ontology);
    plan(graph, ontology)
}

fn swarm_sizes(p: &Plan) -> Vec<usize> {
    let mut sizes: Vec<_> = p.swarms.iter().map(|s| s.members.len()).collect();
    sizes.sort_unstable();
    sizes
}

fn relation(kind: RelationKind, strength: f64) -> Relation {
    Relation {
        from: "dependency-audit".into(),
        to: "platform-audit".into(),
        kind,
        strength,
    }
}

fn mutation(path: &str, branch: &str) -> Option<Mutation> {
    Some(Mutation {
        allowed_paths: vec![path.into()],
        forbidden_paths: vec![],
        branch: branch.into(),
    })
}

#[test]
fn example_plan_estimates() {
    let p = planned(|_, _| {}).unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 1, 1]);
    assert!(p.swarms.iter().all(|s| s.topology == "single"));
    assert_eq!(p.serial_estimate_ms, 4000);
    assert_eq!(p.critical_path_ms, 2000);
    assert_eq!(p.critical_path, ["test-analysis", "synthesis"]);
    assert_eq!(p.peak_ready, 3);
    assert!(p.warnings.is_empty());
}

#[test]
fn planning_is_deterministic() {
    let a = serde_json::to_string(&planned(|_, _| {}).unwrap()).unwrap();
    let b = serde_json::to_string(&planned(|_, _| {}).unwrap()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn invalid_graph_and_ontology_rejected() {
    let err = planned(|g, _| g.nodes.clear()).unwrap_err();
    assert_eq!(err.kind, FailureKind::InvalidGraph);
    for bad in [
        relation(RelationKind::SharesState, f64::NAN),
        relation(RelationKind::SharesState, 1.5),
        Relation {
            from: String::new(),
            ..relation(RelationKind::Tests, 1.0)
        },
        Relation {
            to: String::new(),
            ..relation(RelationKind::Tests, 1.0)
        },
    ] {
        let err = planned(|_, o| o.relations.push(bad)).unwrap_err();
        assert_eq!(err.kind, FailureKind::ContractViolation);
    }
}

#[test]
fn units_must_be_admissible_and_sum_within_colony_budget() {
    let err = planned(|g, _| g.policy.max_classification = Classification::Public).unwrap_err();
    assert_eq!(err.kind, FailureKind::PolicyViolation);
    let err = planned(|g, _| g.policy.budget.calls = 3).unwrap_err();
    assert_eq!(err.kind, FailureKind::BudgetExhausted);
    assert!(planned(|g, _| g.policy.budget.calls = 4).is_ok());
}

#[test]
fn ontology_relation_couples_at_threshold() {
    let p = planned(|_, o| o.relations.push(relation(RelationKind::SharesState, 0.5))).unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 2]);
    let p = planned(|_, o| o.relations.push(relation(RelationKind::SharesState, 0.49))).unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 1, 1]);
    let p = planned(|_, o| o.relations.push(relation(RelationKind::Independent, 1.0))).unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 1, 1]);
}

#[test]
fn shared_entity_couples() {
    let p = planned(|g, _| {
        g.nodes[0].context.ontology_entities.push("lockfile".into());
        g.nodes[1].context.ontology_entities.push("lockfile".into());
    })
    .unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 2]);
}

#[test]
fn mutation_path_or_branch_overlap_couples() {
    let with = |a: Option<Mutation>, b: Option<Mutation>| {
        planned(|g, _| {
            g.policy.allow_mutation = true;
            g.nodes[0].mutation = a;
            g.nodes[1].mutation = b;
        })
        .unwrap()
    };
    let p = with(
        mutation("src", "colony/a"),
        mutation("src/lib.rs", "colony/b"),
    );
    assert_eq!(swarm_sizes(&p), [1, 1, 2]);
    let p = with(mutation("src", "colony/a"), mutation("docs", "colony/a"));
    assert_eq!(swarm_sizes(&p), [1, 1, 2]);
    let p = with(mutation("src", "colony/a"), mutation("srcs", "colony/b"));
    assert_eq!(swarm_sizes(&p), [1, 1, 1, 1]);
}

#[test]
fn high_relational_density_couples_and_serializes() {
    let p = planned(|g, _| g.nodes[0].relational_density = 0.7).unwrap();
    assert_eq!(swarm_sizes(&p), [4]);
    assert_eq!(p.swarms[0].topology, "pipeline");
    assert_eq!(p.peak_ready, 1);
    assert_eq!(p.critical_path_ms, p.serial_estimate_ms);
    let p = planned(|g, _| g.nodes[0].relational_density = 0.69).unwrap();
    assert_eq!(swarm_sizes(&p), [1, 1, 1, 1]);
}

#[test]
fn coupled_members_are_serialized_in_topological_order() {
    let p = planned(|_, o| o.relations.push(relation(RelationKind::Implements, 0.9))).unwrap();
    let deps = &p.graph.unit("platform-audit").unwrap().dependencies;
    assert_eq!(deps, &["dependency-audit"]);
    assert_eq!(p.peak_ready, 2);
}

#[test]
fn semantic_depends_on_injects_edges_and_rejects_cycles() {
    let p = planned(|_, o| o.relations.push(relation(RelationKind::DependsOn, 1.0))).unwrap();
    let deps = &p.graph.unit("dependency-audit").unwrap().dependencies;
    assert!(deps.contains(&"platform-audit".to_string()));
    let err = planned(|_, o| {
        o.relations.push(Relation {
            from: "dependency-audit".into(),
            to: "synthesis".into(),
            kind: RelationKind::DependsOn,
            strength: 1.0,
        })
    })
    .unwrap_err();
    assert_eq!(err.kind, FailureKind::InvalidGraph);
}

#[test]
fn locked_entities_produce_warnings() {
    let p = planned(|_, o| {
        o.locked_entities.insert("synthesis".into());
    })
    .unwrap();
    assert_eq!(p.warnings.len(), 1);
    assert!(p.warnings[0].starts_with("synthesis "));
}

#[test]
fn peak_ready_capped_by_max_workers() {
    let p = planned(|g, _| g.policy.max_workers = 2).unwrap();
    assert_eq!(p.peak_ready, 2);
}

#[test]
fn duration_overflow_rejected() {
    let err = planned(|g, _| {
        g.nodes[0].estimated_ms = u64::MAX;
        g.nodes[3].estimated_ms = 1;
    })
    .unwrap_err();
    assert_eq!(err.kind, FailureKind::InvalidGraph);
    let err = planned(|g, _| {
        g.nodes[0].estimated_ms = u64::MAX;
        g.nodes[1].estimated_ms = u64::MAX;
    })
    .unwrap_err();
    assert_eq!(err.kind, FailureKind::InvalidGraph);
}
