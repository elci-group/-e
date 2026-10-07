// SPDX-License-Identifier: MIT
//! Drive a colony through the real host adapters.
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use colony_core::{Error, FailureKind, OntologySlice, Result, SemanticSource, WorkGraph};
use colony_provider::Registry;
use colony_runtime::Colony;
use serde::{Deserialize, Serialize};

use crate::elci::ElciEndpoint;
use crate::executor::{attempt_handle, MesutExecutor};
use crate::preflight::LucidPreflight;
use crate::semantic::PadagoniaSource;
use crate::verifier::HostVerifier;

/// One colony request file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColonyRequest {
    pub graph: WorkGraph,
    pub ontology: OntologySlice,
    pub registry: Registry,
}

/// What the host supplies beyond the request JSON.
pub struct Host {
    pub root: PathBuf,
    pub ontology: Option<PathBuf>,
    pub endpoint: ElciEndpoint,
    pub human_approved: bool,
}

/// Result of a live run. Paths from the attempt directory are not included.
#[derive(Debug, Serialize)]
pub struct Report {
    pub mode: &'static str,
    pub snapshot: colony_runtime::Snapshot,
    pub evidence: std::collections::BTreeMap<String, colony_core::Evidence>,
}

pub fn execute(mut request: ColonyRequest, host: Host) -> Result<Report> {
    if let Some(path) = &host.ontology {
        let source = PadagoniaSource::open(path)?;
        request.ontology = source.slice(&ontology_query(&request.graph))?;
    }
    for unit in &mut request.graph.nodes {
        let Some(max_input) = unit.budget.tokens.checked_sub(unit.demand.output_tokens) else {
            continue;
        };
        let objective = format!("{} {}", request.graph.objective, unit.objective);
        let mut manifest = LucidPreflight::new(&host.root).context_within(&objective, max_input)?;
        manifest.ontology_entities = unit.context.ontology_entities.clone();
        unit.context = manifest;
    }
    let plan = colony_planner::plan(request.graph, request.ontology)?;
    let mut colony = Colony::new(plan, request.registry.clone())?;
    let scratch = std::env::temp_dir().join(format!("colony-run-{}", std::process::id()));
    let _cleanup = Scratch(scratch.clone());
    fs::create_dir_all(&scratch).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("create attempt dir: {error}"),
        )
    })?;
    let mut executor = MesutExecutor::new(&host.root, &scratch, host.endpoint, request.registry)?;
    let mut verifier = HostVerifier::new(&host.root);
    let mut now = 1u64;
    while !colony.complete() {
        let ready = colony.ready();
        if ready.is_empty() {
            return Err(Error::new(
                FailureKind::DependencyFailure,
                "live run has no runnable work",
            ));
        }
        let mut wave = Vec::new();
        for id in ready {
            match colony.dispatch(&id, now, usize::MAX, host.human_approved, &mut executor) {
                Ok(dispatch) => wave.push(dispatch),
                Err(error) if error.kind == FailureKind::ProviderThrottled && !wave.is_empty() => {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        now = now.checked_add(1).ok_or_else(|| {
            Error::new(FailureKind::InfrastructureFailure, "logical clock overflow")
        })?;
        for dispatch in wave {
            let handle = attempt_handle(&dispatch);
            let result = executor.join_timeout(&handle, Duration::from_secs(120))?;
            colony.receive(dispatch.attempt, result, now)?;
            colony.verify(&dispatch.unit.id, now, &mut verifier)?;
            now = now.checked_add(1).ok_or_else(|| {
                Error::new(FailureKind::InfrastructureFailure, "logical clock overflow")
            })?;
        }
    }
    Ok(Report {
        mode: "execution_complete",
        snapshot: colony.snapshot(),
        evidence: colony.evidence().clone(),
    })
}

fn ontology_query(graph: &WorkGraph) -> String {
    let mut query = graph.objective.clone();
    for unit in &graph.nodes {
        query.push(' ');
        query.push_str(&unit.id);
        query.push(' ');
        query.push_str(&unit.objective);
        for entity in &unit.context.ontology_entities {
            query.push(' ');
            query.push_str(entity);
        }
    }
    query
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elci::ElciEndpoint;
    use crate::semantic::sample_store;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn live_run_is_deterministic_and_leaves_the_tree_untouched() {
        let root = std::env::temp_dir().join(format!("colony-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let source = "name = \"portability\"\n";
        fs::write(root.join("Cargo.toml"), source).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn dependency_audit() {}\nfn platform_audit() {}\n",
        )
        .unwrap();
        let ontology = root.join("slice.pad");
        sample_store(&ontology);
        let script = root.join("worker.sh");
        fs::write(
            &script,
            "#!/bin/sh\nset -eu\npath=$(printf '%s\\n' \"$COLONY_PATHS\" | head -n 1)\nprintf '%s\\n' \"$COLONY_OUTPUTS\" | while IFS= read -r name; do\n  [ -n \"$name\" ] || continue\n  printf '{\"findings\":[{\"reference\":\"%s\",\"detail\":\"observed\"}]}\\n' \"$path\" > \"$COLONY_OUT/$name\"\ndone\n",
        )
        .unwrap();
        let mut mode = fs::metadata(&script).unwrap().permissions();
        mode.set_mode(0o755);
        fs::set_permissions(&script, mode).unwrap();
        let request: ColonyRequest =
            serde_json::from_str(include_str!("../../../examples/request.json")).unwrap();
        let host = || Host {
            root: root.clone(),
            ontology: Some(ontology.clone()),
            endpoint: ElciEndpoint::Command {
                program: script.clone(),
                args: Vec::new(),
            },
            human_approved: false,
        };
        let first = execute(request.clone(), host()).unwrap();
        let second = execute(request, host()).unwrap();
        let encoded = serde_json::to_string(&first).unwrap();
        assert_eq!(encoded, serde_json::to_string(&second).unwrap());
        assert_eq!(first.mode, "execution_complete");
        assert!(first.snapshot.completed);
        assert_eq!(first.evidence.len(), 4);
        assert_eq!(fs::read_to_string(root.join("Cargo.toml")).unwrap(), source);
        let _ = fs::remove_dir_all(&root);
    }
}
