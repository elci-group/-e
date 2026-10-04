use colony_core::*;
use colony_provider::Registry;
use colony_runtime::{Colony,Dispatch,Mesut,Verifier};
use serde::{Deserialize,Serialize};
use std::{env,fs,process::ExitCode};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { graph:WorkGraph, ontology:OntologySlice, registry:Registry }
#[derive(Serialize)]
struct Planned { mode:&'static str, plan:colony_planner::Plan, assignments:Vec<colony_allocator::Assignment> }
fn load(path:&str)->std::result::Result<Request,Box<dyn std::error::Error>> { let data=fs::read(path)?; Ok(serde_json::from_slice(&data)?) }
fn main()->ExitCode { match run() { Ok(())=>ExitCode::SUCCESS,Err(e)=>{ eprintln!("colony: {e}"); ExitCode::FAILURE } } }
fn run()->std::result::Result<(),Box<dyn std::error::Error>> {
 let args:Vec<_>=env::args().skip(1).collect();
 if args.is_empty() || matches!(args[0].as_str(),"--help"|"-h") { println!("Colony (:e) 0.1.0\n\nUsage: colony <plan|validate|simulate> REQUEST.json\n\nplan      Compile bounded work candidates and ontology into an explained JSON plan\nvalidate  Check contracts, policy, semantic DAG and provider eligibility\nsimulate  Exercise lifecycle with synthetic artifacts; runs no inference or tools\n\nLive execution is available through the Rust Mesut and Verifier adapter traits.\nSee examples/request.json and docs/architecture.md."); return Ok(()); }
 if args==["--version"] { println!("colony 0.1.0"); return Ok(()); }
 if args.len()!=2 || !matches!(args[0].as_str(),"plan"|"validate"|"simulate") { return Err("expected plan, validate or simulate followed by a request JSON file; use --help".into()); }
 let request=load(&args[1])?;
 let plan=colony_planner::plan(request.graph,request.ontology)?;
 request.registry.validate()?;
 // Reserve model call quotas in topological order for the static allocation preview.
 let mut available=request.registry.clone(); let mut assignments=Vec::new();
 for id in plan.graph.validate()? {
  let assignment=colony_allocator::allocate(plan.graph.unit(&id)?,&plan.graph.policy,&available)?;
  let r=available.resources.iter_mut().find(|r|r.provider==assignment.provider && r.model==assignment.model).expect("selected provider"); r.remaining_calls-=1; assignments.push(assignment);
 }
 match args[0].as_str() {
  "plan"=>println!("{}",serde_json::to_string_pretty(&Planned{mode:"plan_only",plan,assignments})?),
  "validate"=>println!("{{\"valid\":true,\"work_units\":{}}}",plan.graph.nodes.len()),
  "simulate"=> {
   let mut colony=Colony::new(plan,request.registry)?; let mut executor=Simulation; let mut verifier=Simulation;
   let mut now=1;
   while !colony.complete() {
    let ready=colony.ready(); if ready.is_empty() { return Err("simulation has no runnable work".into()); }
    for id in ready {
     let dispatch=colony.dispatch(&id,now,usize::MAX,true,&mut executor)?;
     let unit=&dispatch.unit;
     let result=WorkerResult {work_unit:id.clone(),provider:dispatch.assignment.provider.clone(),model:dispatch.assignment.model.clone(),usage:Budget{money_micros:0,tokens:0,calls:1},artifacts:unit.expected_outputs.iter().map(|name|Artifact{name:name.clone(),reference:format!("simulation://{id}/{name}"),source_sha:unit.context.source_sha.clone(),result_sha:unit.mutation.as_ref().map(|_|"b".repeat(40)),changed_paths:Vec::new()}).collect()};
     colony.receive(dispatch.attempt,result,now)?; colony.verify(&id,now,&mut verifier)?; now+=1;
    }
   }
   println!("{}",serde_json::to_string_pretty(&serde_json::json!({"mode":"simulation_complete","warning":"Synthetic artifacts and verifier checks. No inference, real tests, Git changes or Mesut execution occurred.","snapshot":colony.snapshot(),"evidence":colony.evidence()}))?);
  }, _=>unreachable!(),
 }
 Ok(())
}
struct Simulation;
impl Mesut for Simulation {
 fn submit(&mut self,d:&Dispatch)->Result<String> { Ok(format!("simulation-{}",d.attempt)) }
 fn cancel(&mut self,_:&str)->Result<()> { Ok(()) }
}
impl Verifier for Simulation {
 fn verify(&mut self,u:&WorkUnit,_:&WorkerResult)->Result<Evidence> { Ok(Evidence {work_unit:u.id.clone(),verifier:"simulation-only".into(),checks:u.verification.deterministic_checks.iter().map(|name|CheckResult{name:name.clone(),passed:true,evidence_ref:format!("simulation://checks/{name}")}).collect(),acceptance:u.acceptance.iter().cloned().collect(),consistent:true,semantic_approved:true,timestamp_ms:0}) }
}
