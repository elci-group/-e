//! Control-plane state machine. Mesut owns scheduling, provider calls, tools and isolation.
use colony_allocator::{allocate,Assignment};
use colony_core::*;
use colony_planner::{plan,Plan};
use colony_policy::Ledger;
use colony_provider::Registry;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize)]
#[serde(rename_all="snake_case")]
pub enum State { Pending, Running, AwaitingVerification, Validated, Rejected, Cancelled }
#[derive(Debug,Clone,Serialize)]
pub struct Dispatch { pub colony_id:String, pub attempt:u64, pub unit:WorkUnit, pub assignment:Assignment, pub deadline_ms:u64, pub temperature:f64, pub dependency_results:Vec<WorkerResult>, pub repair_result:Option<WorkerResult>, pub repair_events:Vec<Event> }
/// An adapter must enforce the contract's budget, immutable snapshot, isolated mutation
/// environment and scope. Cancellation ACK means all descendant calls/tools have stopped.
/// Submit errors must mean no job was accepted; adapters must use idempotent attempt IDs.
pub trait Mesut {
 fn submit(&mut self,dispatch:&Dispatch)->Result<String>;
 fn cancel(&mut self,handle:&str)->Result<()>;
}
/// Deterministic gates must run first. Implementations must not treat worker prose as evidence.
pub trait Verifier { fn verify(&mut self,unit:&WorkUnit,result:&WorkerResult)->Result<Evidence>; }
#[derive(Debug,Clone)]
struct Active { handle:String, dispatch:Dispatch }
#[derive(Debug,Clone,Serialize)]
pub struct Snapshot { pub colony_id:String, pub states:BTreeMap<String,State>, pub reserved:Budget, pub completed:bool, pub cancelling:bool, pub events:Vec<Event> }

pub struct Colony {
 plan:Plan, registry:Registry, ledger:Ledger, states:BTreeMap<String,State>, active:BTreeMap<String,Active>,
 results:BTreeMap<String,WorkerResult>, evidence:BTreeMap<String,Evidence>, events:Vec<Event>,
 attempt:u64, cancelling:bool, last_time:u64,
}
impl Colony {
 pub fn new(input:Plan,registry:Registry)->Result<Self> {
  // Never trust serialized derived plan fields as executable authority.
  let plan=plan(input.graph,input.ontology)?; registry.validate()?;
  let states=plan.graph.nodes.iter().map(|n|(n.id.clone(),State::Pending)).collect();
  let ledger=Ledger::new(plan.graph.policy.budget);
  let mut colony=Self {plan,registry,ledger,states,active:BTreeMap::new(),results:BTreeMap::new(),evidence:BTreeMap::new(),events:Vec::new(),attempt:0,cancelling:false,last_time:0};
  colony.emit(None,"ColonyCreated","validated plan admitted"); Ok(colony)
 }
 fn emit(&mut self,id:Option<&str>,kind:&str,detail:&str) { self.events.push(Event{sequence:self.events.len() as u64,timestamp_ms:self.last_time,work_unit:id.map(str::to_owned),kind:kind.into(),detail:detail.into()}); }
 fn clock(&mut self,now:u64)->Result<()> { ensure(now>=self.last_time,FailureKind::ContractViolation,"clock moved backwards")?; self.last_time=now; Ok(()) }
 pub fn snapshot(&self)->Snapshot { Snapshot{colony_id:self.plan.graph.colony_id.clone(),states:self.states.clone(),reserved:self.ledger.reserved(),completed:self.complete(),cancelling:self.cancelling,events:self.events.clone()} }
 pub fn complete(&self)->bool { !self.cancelling && self.states.values().all(|s|*s==State::Validated) && self.evidence.len()==self.states.len() }
 pub fn evidence(&self)->&BTreeMap<String,Evidence> { &self.evidence }
 pub fn results(&self)->&BTreeMap<String,WorkerResult> { &self.results }
 pub fn ready(&self)->Vec<String> { if self.cancelling { return Vec::new(); } self.plan.graph.nodes.iter().filter(|n|self.states[&n.id]==State::Pending && n.dependencies.iter().all(|id|self.states[id]==State::Validated)).map(|n|n.id.clone()).collect() }
 /// safe_width comes from Mesut/Pressure Valve. Zero explicitly stops new dispatch.
 /// Approval is supplied by the authenticated host, never by a worker/model.
 pub fn dispatch(&mut self,id:&str,now:u64,safe_width:usize,human_approved:bool,mesut:&mut impl Mesut)->Result<Dispatch> {
  self.clock(now)?;
  ensure(!self.cancelling && now<self.plan.graph.policy.deadline_ms,FailureKind::Cancelled,"colony cancelled or deadline reached")?;
  ensure(self.ready().iter().any(|x|x==id),FailureKind::DependencyFailure,"work is not dependency-ready")?;
  let in_flight=self.states.values().filter(|s|matches!(s,State::Running|State::AwaitingVerification)).count();
  ensure(in_flight<safe_width.min(self.plan.graph.policy.max_workers),FailureKind::ProviderThrottled,"pressure or verifier backpressure limits dispatch")?;
  let unit=self.plan.graph.unit(id)?.clone();
  ensure(unit.mutation.is_none() || !self.plan.graph.policy.require_human_approval || human_approved,FailureKind::PolicyViolation,"human mutation approval required")?;
  ensure(unit.mutation.is_none() || !unit.context.ontology_entities.iter().any(|e|self.plan.ontology.locked_entities.contains(e)),FailureKind::PolicyViolation,"active semantic ownership lock")?;
  let mut available=self.registry.clone();
  for resource in &mut available.resources {
   let busy=self.active.values().filter(|a|a.dispatch.assignment.provider==resource.provider && a.dispatch.assignment.model==resource.model).count();
   resource.available &= busy<resource.concurrency;
  }
  let assignment=allocate(&unit,&self.plan.graph.policy,&available)?;
  self.ledger.reserve(unit.budget)?;
  self.attempt=self.attempt.checked_add(1).ok_or_else(||Error::new(FailureKind::InfrastructureFailure,"attempt counter overflow"))?;
  let dispatch=Dispatch{colony_id:self.plan.graph.colony_id.clone(),attempt:self.attempt,unit,assignment,deadline_ms:self.plan.graph.policy.deadline_ms,temperature:0.2, dependency_results:self.plan.graph.unit(id)?.dependencies.iter().filter_map(|d|self.results.get(d).cloned()).collect(), repair_result:self.results.get(id).cloned(), repair_events:self.events.iter().filter(|e|e.work_unit.as_deref()==Some(id) && matches!(e.kind.as_str(),"ValidationFailed"|"WorkerFailed")).cloned().collect()};
  // Keep the conservative reservation if submit fails; accounting never assumes a refund.
  let handle=mesut.submit(&dispatch)?;
  self.registry.resources.iter_mut().find(|r|r.provider==dispatch.assignment.provider && r.model==dispatch.assignment.model).expect("allocated resource").remaining_calls-=1;
  self.states.insert(id.into(),State::Running);
  self.active.insert(id.into(),Active{handle,dispatch:dispatch.clone()});
  self.emit(Some(id),"WorkerAssigned",&format!("{}/{}",dispatch.assignment.provider,dispatch.assignment.model));
  Ok(dispatch)
 }
 pub fn receive(&mut self,attempt:u64,result:WorkerResult,now:u64)->Result<()> {
  self.clock(now)?; let id=result.work_unit.clone();
  ensure(!self.cancelling && now<self.plan.graph.policy.deadline_ms,FailureKind::Cancelled,"late result after cancellation/deadline")?;
  ensure(self.states.get(&id)==Some(&State::Running),FailureKind::ContractViolation,"result not for a running unit")?;
  let active=self.active.get(&id).ok_or_else(||Error::new(FailureKind::ContractViolation,"missing active dispatch"))?;
  ensure(active.dispatch.attempt==attempt && active.dispatch.assignment.provider==result.provider && active.dispatch.assignment.model==result.model,FailureKind::ContractViolation,"result provenance or attempt mismatch")?;
  let unit=self.plan.graph.unit(&id)?;
  // The execution adapter signals job termination by delivering this result.
  let validation=validate_artifacts(unit,&result);
  self.active.remove(&id);
  self.results.insert(id.clone(),result);
  if let Err(e)=validation { self.states.insert(id.clone(),State::Rejected); self.emit(Some(&id),"ValidationFailed",&e.to_string()); return Err(e); }
  self.states.insert(id.clone(),State::AwaitingVerification); self.emit(Some(&id),"ArtifactProduced","awaiting independent verification"); Ok(())
 }
 pub fn verify(&mut self,id:&str,now:u64,verifier:&mut impl Verifier)->Result<()> {
  self.clock(now)?;
  ensure(!self.cancelling && now<self.plan.graph.policy.deadline_ms,FailureKind::Cancelled,"verification after cancellation/deadline")?;
  ensure(self.states.get(id)==Some(&State::AwaitingVerification),FailureKind::ContractViolation,"no artifact awaiting verification")?;
  self.emit(Some(id),"ValidationStarted","trusted verifier invoked");
  let unit=self.plan.graph.unit(id)?;
  let verified=verifier.verify(unit,&self.results[id]).and_then(|e| { validate_evidence(unit,&e,now)?; Ok(e) });
  match verified {
   Ok(e)=> { self.evidence.insert(id.into(),e); self.states.insert(id.into(),State::Validated); self.emit(Some(id),"WorkerCompleted","coverage, consistency, validation and evidence passed"); if self.complete() { self.emit(None,"ColonyCompleted","all convergence gates passed"); } Ok(()) },
   Err(e)=> { self.states.insert(id.into(),State::Rejected); self.emit(Some(id),"ValidationFailed",&e.to_string()); Err(e) }
  }
 }
 /// Explicit execution failure, authenticated by the Mesut adapter; stale attempts rejected.
 pub fn fail(&mut self,id:&str,attempt:u64,error:Error,now:u64)->Result<()> {
  self.clock(now)?;
  ensure(!self.cancelling && self.active.get(id).is_some_and(|a|a.dispatch.attempt==attempt),FailureKind::ContractViolation,"failure for inactive attempt")?;
  self.active.remove(id); self.states.insert(id.into(),State::Rejected); self.emit(Some(id),"WorkerFailed",&error.to_string()); Ok(())
 }
 /// Preserve successful siblings. A repair is a new attempt under the original bounded contract.
 pub fn repair(&mut self,id:&str)->Result<()> {
  ensure(!self.cancelling && self.states.get(id)==Some(&State::Rejected),FailureKind::ContractViolation,"repair requires rejected unit")?;
  self.states.insert(id.into(),State::Pending); self.emit(Some(id),"RepairScheduled","prior artifacts and failure events retained; new budget reservation required"); Ok(())
 }
 /// Fail closed immediately; retry this operation if a descendant cancellation ACK fails.
 pub fn cancel(&mut self,now:u64,mesut:&mut impl Mesut)->Result<()> {
  self.clock(now)?; self.cancelling=true;
  for state in self.states.values_mut() { if *state!=State::Validated { *state=State::Cancelled; } }
  let mut error=None;
  for (id,a) in self.active.clone() { match mesut.cancel(&a.handle) { Ok(())=>{self.active.remove(&id);},Err(e)=>error=Some(e) } }
  self.emit(None,if error.is_some(){"CancellationPending"}else{"ColonyCancelled"},"descendant cancellation requested");
  if let Some(e)=error { Err(e) } else { Ok(()) }
 }
 /// Host calls tick from its event loop; Colony creates no executor/timer runtime.
 pub fn tick(&mut self,now:u64,mesut:&mut impl Mesut)->Result<()> { self.clock(now)?; if now>=self.plan.graph.policy.deadline_ms && !self.complete() { self.cancel(now,mesut)?; } Ok(()) }
}
fn validate_artifacts(unit:&WorkUnit,result:&WorkerResult)->Result<()> {
 ensure(result.usage.fits(unit.budget),FailureKind::BudgetExhausted,"reported usage exceeds reservation")?;
 let names:std::collections::BTreeSet<_>=result.artifacts.iter().map(|a|a.name.as_str()).collect();
 ensure(names.len()==result.artifacts.len() && names.len()==unit.expected_outputs.len() && unit.expected_outputs.iter().all(|o|names.contains(o.as_str())),FailureKind::InvalidArtifact,"output coverage mismatch")?;
 for a in &result.artifacts {
  ensure(!a.reference.trim().is_empty() && a.source_sha==unit.context.source_sha,FailureKind::InvalidArtifact,"artifact reference or source revision mismatch")?;
  if let Some(m)=&unit.mutation {
   ensure(a.result_sha.as_deref().is_some_and(valid_sha) && a.changed_paths.iter().all(|p|valid_path(p) && m.allowed_paths.iter().any(|s|p==s || p.strip_prefix(s).is_some_and(|rest|rest.starts_with('/'))) && !m.forbidden_paths.iter().any(|s|overlaps(p,s))),FailureKind::ContractViolation,"mutation outside scope or missing result revision")?;
  } else { ensure(a.changed_paths.is_empty(),FailureKind::ContractViolation,"read-only worker reported mutation")?; }
 } Ok(())
}
fn validate_evidence(unit:&WorkUnit,e:&Evidence,now:u64)->Result<()> {
 ensure(e.work_unit==unit.id && !e.verifier.trim().is_empty() && e.timestamp_ms<=now && e.consistent && (!unit.verification.semantic_review || e.semantic_approved),FailureKind::ConvergenceFailure,"missing consistency, identity or semantic approval")?;
 ensure(unit.acceptance.iter().all(|a|e.acceptance.contains(a)),FailureKind::ConvergenceFailure,"acceptance coverage incomplete")?;
 ensure(unit.verification.deterministic_checks.iter().all(|required|e.checks.iter().any(|c|&c.name==required && c.passed && !c.evidence_ref.trim().is_empty())) && e.checks.iter().all(|c|c.passed),FailureKind::DeterministicValidationFailure,"required deterministic gate failed or missing evidence")
}
