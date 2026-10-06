// SPDX-License-Identifier: MIT
//! Stable-Rust, std-only benchmarks (`harness = false`): `cargo bench --offline`.
//!
//! Each case runs `ITERS` operations per sample and reports the minimum ns/op over
//! `SAMPLES` samples, which is the most stable estimator on a shared machine.
use colony_core::*;
use colony_provider::Registry;
use colony_runtime::{Colony, Dispatch, Mesut, Verifier};
use serde::Deserialize;
use std::hint::black_box;
use std::time::Instant;

const SAMPLES: usize = 7;
const SIZES: [usize; 3] = [10, 100, 500];
/// Each dense node depends on every predecessor within this window.
const DENSE_WINDOW: usize = 16;

#[derive(Deserialize)]
struct Request {
    graph: WorkGraph,
    registry: Registry,
}

#[derive(Clone, Copy)]
enum Shape {
    /// n0 ← n1 ← … ← n(k-1): no parallelism, longest critical path.
    Chain,
    /// One root, k-2 independent leaves, one sink depending on every leaf.
    FanOut,
    /// Node i depends on max(0, i-16)..i: many edges, bounded width.
    Dense,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Shape::Chain => "chain",
            Shape::FanOut => "fan-out",
            Shape::Dense => "dense",
        }
    }
    fn dependencies(self, i: usize, n: usize) -> Vec<usize> {
        match self {
            Shape::Chain => i.checked_sub(1).into_iter().collect(),
            Shape::FanOut if i == 0 => vec![],
            Shape::FanOut if i == n - 1 => (1..n - 1).collect(),
            Shape::FanOut => vec![0],
            Shape::Dense => (i.saturating_sub(DENSE_WINDOW)..i).collect(),
        }
    }
}

fn fixture() -> Request {
    serde_json::from_str(include_str!("../../../examples/request.json")).unwrap()
}

/// Synthesizes an `n`-node graph from the example's first unit and a registry
/// with enough quota and concurrency that capacity never limits dispatch.
fn generate(shape: Shape, n: usize) -> (WorkGraph, Registry) {
    let Request {
        mut graph,
        mut registry,
    } = fixture();
    let template = graph.nodes[0].clone();
    let id = |i: usize| format!("unit-{i:04}");
    graph.nodes = (0..n)
        .map(|i| {
            let mut unit = template.clone();
            unit.id = id(i);
            unit.context.ontology_entities = vec![id(i)];
            unit.expected_outputs = vec![format!("{}.json", id(i))];
            unit.dependencies = shape.dependencies(i, n).into_iter().map(id).collect();
            unit
        })
        .collect();
    let units = n as u64;
    graph.policy.max_workers = n;
    graph.policy.budget = Budget {
        money_micros: template.budget.money_micros * units,
        tokens: template.budget.tokens * units,
        calls: units,
    };
    for r in &mut registry.resources {
        r.remaining_calls = units;
        r.concurrency = n;
    }
    (graph, registry)
}

struct Simulation;

impl Mesut for Simulation {
    fn submit(&mut self, d: &Dispatch) -> Result<String> {
        Ok(d.attempt.to_string())
    }
    fn cancel(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}

impl Verifier for Simulation {
    fn verify(&mut self, u: &WorkUnit, _: &WorkerResult) -> Result<Evidence> {
        Ok(Evidence {
            work_unit: u.id.clone(),
            verifier: "bench".into(),
            checks: u
                .verification
                .deterministic_checks
                .iter()
                .map(|name| CheckResult {
                    name: name.clone(),
                    passed: true,
                    evidence_ref: "bench://check".into(),
                })
                .collect(),
            acceptance: u.acceptance.iter().cloned().collect(),
            consistent: true,
            semantic_approved: true,
            timestamp_ms: 0,
        })
    }
}

/// Admission, dispatch, delivery and verification of every unit until convergence.
fn lifecycle(graph: &WorkGraph, registry: &Registry) -> Result<Colony> {
    let plan = colony_planner::plan(graph.clone(), OntologySlice::default())?;
    let mut colony = Colony::new(plan, registry.clone())?;
    let mut sim = Simulation;
    let mut now = 1;
    while !colony.complete() {
        let ready = colony.ready();
        ensure(!ready.is_empty(), FailureKind::DependencyFailure, "stalled")?;
        for id in ready {
            let d = colony.dispatch(&id, now, usize::MAX, false, &mut sim)?;
            let result = WorkerResult {
                work_unit: id.clone(),
                provider: d.assignment.provider.clone(),
                model: d.assignment.model.clone(),
                usage: Budget::default(),
                artifacts: d
                    .unit
                    .expected_outputs
                    .iter()
                    .map(|name| Artifact {
                        name: name.clone(),
                        reference: "bench://artifact".into(),
                        source_sha: d.unit.context.source_sha.clone(),
                        result_sha: None,
                        changed_paths: vec![],
                    })
                    .collect(),
            };
            colony.receive(d.attempt, result, now)?;
            colony.verify(&id, now, &mut sim)?;
            now += 1;
        }
    }
    Ok(colony)
}

/// Fewer iterations for larger graphs keep each sample in the tens of milliseconds.
fn iterations(n: usize, heavy: bool) -> usize {
    let base = match n {
        0..=10 => 2000,
        11..=100 => 100,
        _ => 8,
    };
    if heavy {
        (base / 10).max(2)
    } else {
        base
    }
}

fn measure(iters: usize, mut op: impl FnMut()) -> u128 {
    op(); // warm-up
    (0..SAMPLES)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..iters {
                op();
            }
            start.elapsed().as_nanos() / iters as u128
        })
        .min()
        .unwrap_or(0)
}

fn report(bench: &str, shape: Shape, n: usize, iters: usize, ns: u128) {
    println!("{bench:<10} {:<8} {n:>5} {iters:>6} {ns:>14}", shape.name());
}

fn main() {
    // `cargo bench` passes `--bench`; `cargo test --all-targets` does not, so skip there.
    if !std::env::args().any(|a| a == "--bench") {
        return;
    }
    println!(
        "{:<10} {:<8} {:>5} {:>6} {:>14}",
        "bench", "shape", "nodes", "iters", "min ns/op"
    );
    for shape in [Shape::Chain, Shape::FanOut, Shape::Dense] {
        for n in SIZES {
            let (graph, registry) = generate(shape, n);
            let unit = &graph.nodes[n - 1];

            let iters = iterations(n, false);
            let ns = measure(iters, || {
                black_box(black_box(&graph).validate().unwrap());
            });
            report("validate", shape, n, iters, ns);

            let iters = iterations(n, true);
            let ns = measure(iters, || {
                let plan = colony_planner::plan(black_box(graph.clone()), OntologySlice::default());
                black_box(plan.unwrap());
            });
            report("plan", shape, n, iters, ns);

            let iters = iterations(n, false);
            let ns = measure(iters, || {
                let a = colony_allocator::allocate(black_box(unit), &graph.policy, &registry);
                black_box(a.unwrap());
            });
            report("allocate", shape, n, iters, ns);

            let iters = iterations(n, true);
            let ns = measure(iters, || {
                black_box(lifecycle(&graph, &registry).unwrap());
            });
            report("lifecycle", shape, n, iters, ns);
        }
    }
}
