// SPDX-License-Identifier: MIT
use colony_adapters::{execute, ColonyRequest, ElciEndpoint, Host};
use colony_core::*;
use colony_runtime::{Colony, Dispatch, Mesut, Verifier};
use serde::Serialize;
use std::path::PathBuf;
use std::{env, fs, process::ExitCode};
#[derive(Serialize)]
struct Planned {
    mode: &'static str,
    plan: colony_planner::Plan,
    assignments: Vec<colony_allocator::Assignment>,
}
fn load(path: &str) -> std::result::Result<ColonyRequest, Box<dyn std::error::Error>> {
    let data = fs::read(path)?;
    Ok(serde_json::from_slice(&data)?)
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("colony: {e}");
            ExitCode::FAILURE
        }
    }
}
enum Mode {
    Plan,
    Validate,
    Simulate,
}
fn usage() -> String {
    format!(
        "\
Colony (:e) {version}

Usage: colony <plan|validate|simulate|execute> REQUEST.json

plan      Compile bounded work candidates and ontology into an explained JSON plan
validate  Check contracts, policy, semantic DAG and provider eligibility
simulate  Exercise lifecycle with synthetic artifacts; runs no inference or tools
execute   Run the colony on Mesut with ELCI inference and the host verifier

execute REQUEST.json --root DIR [--ontology FILE.pad]
        [--command PROG] [--arg ARG]...
        [--endpoint URL] [--api-key KEY] [--approve]

simulate is a synthetic stand-in. execute is the live host path.
See examples/request.json and docs/architecture.md.
",
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h") {
        print!("{}", usage());
        return Ok(());
    }
    if args == ["--version"] {
        println!("colony {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("execute") {
        return execute_cli(&args[1..]);
    }
    let (mode, path) =
        match args.as_slice() {
            [verb, path] if verb == "plan" => (Mode::Plan, path),
            [verb, path] if verb == "validate" => (Mode::Validate, path),
            [verb, path] if verb == "simulate" => (Mode::Simulate, path),
            _ => return Err(
                "expected plan, validate, simulate or execute followed by a request JSON file; use --help"
                    .into(),
            ),
        };
    let request = load(path)?;
    let plan = colony_planner::plan(request.graph, request.ontology)?;
    request.registry.validate()?;
    // Reserve model call quotas in topological order for the static allocation preview.
    let mut available = request.registry.clone();
    let mut assignments = Vec::new();
    for id in plan.graph.validate()? {
        let assignment =
            colony_allocator::allocate(plan.graph.unit(&id)?, &plan.graph.policy, &available)?;
        let r = available
            .resources
            .iter_mut()
            .find(|r| r.provider == assignment.provider && r.model == assignment.model)
            .ok_or("allocator selected an unknown provider")?;
        r.remaining_calls = r
            .remaining_calls
            .checked_sub(1)
            .ok_or("allocator selected an exhausted provider")?;
        assignments.push(assignment);
    }
    match mode {
        Mode::Plan => println!(
            "{}",
            serde_json::to_string_pretty(&Planned {
                mode: "plan_only",
                plan,
                assignments
            })?
        ),
        Mode::Validate => println!(
            "{{\"valid\":true,\"work_units\":{}}}",
            plan.graph.nodes.len()
        ),
        Mode::Simulate => {
            let mut colony = Colony::new(plan, request.registry)?;
            let mut executor = Simulation;
            let mut verifier = Simulation;
            let mut now = 1;
            while !colony.complete() {
                let ready = colony.ready();
                if ready.is_empty() {
                    return Err("simulation has no runnable work".into());
                }
                for id in ready {
                    let dispatch = colony.dispatch(&id, now, usize::MAX, true, &mut executor)?;
                    let unit = &dispatch.unit;
                    let result = WorkerResult {
                        work_unit: id.clone(),
                        provider: dispatch.assignment.provider.clone(),
                        model: dispatch.assignment.model.clone(),
                        usage: Budget {
                            money_micros: 0,
                            tokens: 0,
                            calls: 1,
                        },
                        artifacts: unit
                            .expected_outputs
                            .iter()
                            .map(|name| Artifact {
                                name: name.clone(),
                                reference: format!("simulation://{id}/{name}"),
                                source_sha: unit.context.source_sha.clone(),
                                result_sha: unit.mutation.as_ref().map(|_| "b".repeat(40)),
                                changed_paths: Vec::new(),
                            })
                            .collect(),
                    };
                    colony.receive(dispatch.attempt, result, now)?;
                    colony.verify(&id, now, &mut verifier)?;
                    now += 1;
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"mode":"simulation_complete","warning":"Synthetic artifacts and verifier checks. No inference, real tests, Git changes or Mesut execution occurred.","snapshot":colony.snapshot(),"evidence":colony.evidence()})
                )?
            );
        }
    }
    Ok(())
}
fn execute_cli(args: &[String]) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() || args[0].starts_with('-') {
        return Err("execute requires a request JSON file before its flags; use --help".into());
    }
    let mut root = None;
    let mut ontology = None;
    let mut command = None;
    let mut command_args = Vec::new();
    let mut endpoint = None;
    let mut api_key = None;
    let mut approve = false;
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = |index: &mut usize, name: &str| -> std::result::Result<String, String> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--root" => root = Some(value(&mut index, "--root")?),
            "--ontology" => ontology = Some(value(&mut index, "--ontology")?),
            "--command" => command = Some(value(&mut index, "--command")?),
            "--arg" => command_args.push(value(&mut index, "--arg")?),
            "--endpoint" => endpoint = Some(value(&mut index, "--endpoint")?),
            "--api-key" => api_key = Some(value(&mut index, "--api-key")?),
            "--approve" => approve = true,
            other => return Err(format!("unknown execute flag {other}").into()),
        }
        index += 1;
    }
    let root = root.ok_or("execute requires --root")?;
    if api_key.is_some() && endpoint.is_none() {
        return Err("--api-key applies to --endpoint".into());
    }
    if !command_args.is_empty() && command.is_none() {
        return Err("--arg applies to --command".into());
    }
    let endpoint = match (command, endpoint) {
        (Some(program), None) => ElciEndpoint::Command {
            program: PathBuf::from(program),
            args: command_args,
        },
        (None, Some(base)) => ElciEndpoint::Http { base, api_key },
        (Some(_), Some(_)) => return Err("execute accepts either --command or --endpoint".into()),
        (None, None) => return Err("execute requires --command or --endpoint".into()),
    };
    let request = load(&args[0])?;
    let report = execute(
        request,
        Host {
            root: PathBuf::from(root),
            ontology: ontology.map(PathBuf::from),
            endpoint,
            human_approved: approve,
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

struct Simulation;
impl Mesut for Simulation {
    fn submit(&mut self, d: &Dispatch) -> Result<String> {
        Ok(format!("simulation-{}", d.attempt))
    }
    fn cancel(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}
impl Verifier for Simulation {
    fn verify(&mut self, u: &WorkUnit, _: &WorkerResult) -> Result<Evidence> {
        Ok(Evidence {
            work_unit: u.id.clone(),
            verifier: "simulation-only".into(),
            checks: u
                .verification
                .deterministic_checks
                .iter()
                .map(|name| CheckResult {
                    name: name.clone(),
                    passed: true,
                    evidence_ref: format!("simulation://checks/{name}"),
                })
                .collect(),
            acceptance: u.acceptance.iter().cloned().collect(),
            consistent: true,
            semantic_approved: true,
            timestamp_ms: 0,
        })
    }
}
