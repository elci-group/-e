// SPDX-License-Identifier: MIT
//! Deterministic planning from bounded candidates and a versioned Padagonia slice.
use colony_core::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swarm {
    pub id: String,
    pub members: Vec<String>,
    pub topology: String,
    pub rationale: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub graph: WorkGraph,
    pub ontology: OntologySlice,
    pub swarms: Vec<Swarm>,
    pub critical_path: Vec<String>,
    pub serial_estimate_ms: u64,
    pub critical_path_ms: u64,
    pub peak_ready: usize,
    pub warnings: Vec<String>,
}

pub fn plan(mut graph: WorkGraph, ontology: OntologySlice) -> Result<Plan> {
    graph.validate()?;
    ensure(
        ontology
            .relations
            .iter()
            .all(|r| score(r.strength) && !r.from.is_empty() && !r.to.is_empty()),
        FailureKind::ContractViolation,
        "invalid ontology relation",
    )?;
    let mut total = Budget::default();
    for n in &graph.nodes {
        colony_policy::validate_unit(n, &graph.policy)?;
        total = total.checked_add(n.budget)?;
    }
    ensure(
        total.fits(graph.policy.budget),
        FailureKind::BudgetExhausted,
        "sum of work ceilings exceeds colony budget",
    )?;
    let count = graph.nodes.len();
    let mut groups: Vec<usize> = (0..count).collect();
    let mut warnings = Vec::new();
    for i in 0..count {
        for j in (i + 1)..count {
            let a = &graph.nodes[i];
            let b = &graph.nodes[j];
            let has = |n: &WorkUnit, e: &str| n.context.ontology_entities.iter().any(|x| x == e);
            let related = ontology.relations.iter().any(|r| {
                r.strength >= 0.5
                    && !matches!(r.kind, RelationKind::Independent)
                    && ((has(a, &r.from) && has(b, &r.to)) || (has(b, &r.from) && has(a, &r.to)))
            });
            let path_collision = match (&a.mutation, &b.mutation) {
                (Some(am), Some(bm)) => {
                    am.branch == bm.branch
                        || am
                            .allowed_paths
                            .iter()
                            .any(|x| bm.allowed_paths.iter().any(|y| overlaps(x, y)))
                }
                _ => false,
            };
            let shared_entity = a
                .context
                .ontology_entities
                .iter()
                .any(|e| b.context.ontology_entities.contains(e));
            if related
                || path_collision
                || shared_entity
                || a.relational_density >= 0.7
                || b.relational_density >= 0.7
            {
                let (old, new) = (groups[j], groups[i]);
                for g in &mut groups {
                    if *g == old {
                        *g = new;
                    }
                }
            }
        }
    }
    // Semantic dependencies become control-plane edges, then undergo full DAG validation.
    let original = graph.nodes.clone();
    for a in &mut graph.nodes {
        for b in &original {
            if a.id == b.id {
                continue;
            }
            let depends = ontology.relations.iter().any(|r| {
                matches!(r.kind, RelationKind::DependsOn)
                    && a.context.ontology_entities.contains(&r.from)
                    && b.context.ontology_entities.contains(&r.to)
            });
            if depends && !a.dependencies.contains(&b.id) {
                a.dependencies.push(b.id.clone());
            }
        }
    }
    let order = graph.validate()?;
    let mut swarm_members: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for id in &order {
        let group = graph
            .nodes
            .iter()
            .position(|n| &n.id == id)
            .and_then(|index| groups.get(index))
            .copied()
            .ok_or_else(|| missing(id))?;
        swarm_members.entry(group).or_default().push(id.clone());
    }
    // Explicit serial contracts inside coupled groups avoid hidden shared-state concurrency.
    for members in swarm_members.values() {
        for pair in members.windows(2) {
            let [previous, next] = pair else { continue };
            let node = graph
                .nodes
                .iter_mut()
                .find(|n| &n.id == next)
                .ok_or_else(|| missing(next))?;
            if !node.dependencies.contains(previous) {
                node.dependencies.push(previous.clone());
            }
        }
    }
    let order = graph.validate()?;
    for n in &graph.nodes {
        if n.context
            .ontology_entities
            .iter()
            .any(|e| ontology.locked_entities.contains(e))
        {
            warnings.push(format!(
                "{} intersects active ontology lock; mutation dispatch blocked until replanned",
                n.id
            ));
        }
    }
    let mut finish: BTreeMap<String, u64> = BTreeMap::new();
    let mut paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut serial = 0u64;
    let mut critical: Option<(u64, Vec<String>)> = None;
    for id in &order {
        let n = graph.unit(id)?;
        // Dependencies precede `id` in topological order, so their finish times are known.
        let parent = n
            .dependencies
            .iter()
            .filter_map(|d| finish.get(d).map(|ms| (*ms, d)))
            .max_by_key(|(ms, _)| *ms);
        let start = parent.map_or(0, |(ms, _)| ms);
        let end = start
            .checked_add(n.estimated_ms)
            .ok_or_else(|| Error::new(FailureKind::InvalidGraph, "duration overflow"))?;
        serial = serial
            .checked_add(n.estimated_ms)
            .ok_or_else(|| Error::new(FailureKind::InvalidGraph, "duration overflow"))?;
        let mut path = parent
            .and_then(|(_, d)| paths.get(d).cloned())
            .unwrap_or_default();
        path.push(id.clone());
        // Ties resolve to the latest unit in topological order (`max_by_key` semantics).
        if critical.as_ref().is_none_or(|(ms, _)| end >= *ms) {
            critical = Some((end, path.clone()));
        }
        paths.insert(id.clone(), path);
        finish.insert(id.clone(), end);
    }
    let (critical_path_ms, critical_path) =
        critical.ok_or_else(|| Error::new(FailureKind::InvalidGraph, "empty work graph"))?;
    let mut done = BTreeSet::new();
    let mut peak = 0;
    while done.len() < count {
        let ready: Vec<_> = graph
            .nodes
            .iter()
            .filter(|n| !done.contains(&n.id) && n.dependencies.iter().all(|d| done.contains(d)))
            .map(|n| n.id.clone())
            .collect();
        ensure(
            !ready.is_empty(),
            FailureKind::InvalidGraph,
            "dependency cycle",
        )?;
        peak = peak.max(ready.len());
        done.extend(ready);
    }
    let swarms = swarm_members
        .into_iter()
        .enumerate()
        .map(|(i, (_, members))| Swarm {
            id: format!("swarm-{}", i + 1),
            topology: if members.len() > 1 {
                "pipeline"
            } else {
                "single"
            }
            .into(),
            rationale: "semantic coupling, shared ownership and relational density; \
                        independent groups may fan out"
                .into(),
            members,
        })
        .collect();
    peak = peak.min(graph.policy.max_workers);
    Ok(Plan {
        graph,
        ontology,
        swarms,
        critical_path,
        serial_estimate_ms: serial,
        critical_path_ms,
        peak_ready: peak,
        warnings,
    })
}

fn missing(id: &str) -> Error {
    Error::new(
        FailureKind::InvalidGraph,
        format!("validated node {id} missing"),
    )
}
