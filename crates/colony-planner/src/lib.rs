//! Deterministic planning from bounded candidates and a versioned Padagonia slice.
use colony_core::*;
use serde::{Deserialize,Serialize};
use std::collections::{BTreeMap,BTreeSet};
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Swarm { pub id:String, pub members:Vec<String>, pub topology:String, pub rationale:String }
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Plan { pub graph:WorkGraph, pub ontology:OntologySlice, pub swarms:Vec<Swarm>, pub critical_path:Vec<String>, pub serial_estimate_ms:u64, pub critical_path_ms:u64, pub peak_ready:usize, pub warnings:Vec<String> }

pub fn plan(mut graph:WorkGraph,ontology:OntologySlice)->Result<Plan> {
 graph.validate()?;
 ensure(ontology.relations.iter().all(|r|score(r.strength) && !r.from.is_empty() && !r.to.is_empty()),FailureKind::ContractViolation,"invalid ontology relation")?;
 let mut total=Budget::default();
 for n in &graph.nodes { colony_policy::validate_unit(n,&graph.policy)?; total=total.checked_add(n.budget)?; }
 ensure(total.fits(graph.policy.budget),FailureKind::BudgetExhausted,"sum of work ceilings exceeds colony budget")?;
 let count=graph.nodes.len(); let mut groups:Vec<usize>=(0..count).collect(); let mut warnings=Vec::new();
 for i in 0..count { for j in (i+1)..count {
  let a=&graph.nodes[i]; let b=&graph.nodes[j];
  let has=|n:&WorkUnit,e:&str|n.context.ontology_entities.iter().any(|x|x==e);
  let related=ontology.relations.iter().any(|r|r.strength>=0.5 && !matches!(r.kind,RelationKind::Independent) && ((has(a,&r.from)&&has(b,&r.to))||(has(b,&r.from)&&has(a,&r.to))));
  let path_collision=match (&a.mutation,&b.mutation) { (Some(am),Some(bm))=>am.branch==bm.branch || am.allowed_paths.iter().any(|x|bm.allowed_paths.iter().any(|y|overlaps(x,y))), _=>false };
  let shared_entity=a.context.ontology_entities.iter().any(|e|b.context.ontology_entities.contains(e));
  if related || path_collision || shared_entity || a.relational_density>=0.7 || b.relational_density>=0.7 {
   let old=groups[j]; let new=groups[i]; for g in &mut groups { if *g==old { *g=new; } }
  }
 }}
 // Semantic dependencies become control-plane edges, then undergo full DAG validation.
 let original=graph.nodes.clone();
 for a in &mut graph.nodes { for b in &original { if a.id==b.id { continue; }
  let depends=ontology.relations.iter().any(|r|matches!(r.kind,RelationKind::DependsOn) && a.context.ontology_entities.contains(&r.from) && b.context.ontology_entities.contains(&r.to));
  if depends && !a.dependencies.contains(&b.id) { a.dependencies.push(b.id.clone()); }
 }}
 let order=graph.validate()?;
 let mut swarm_members:BTreeMap<usize,Vec<String>>=BTreeMap::new();
 for id in &order { let index=graph.nodes.iter().position(|n|&n.id==id).expect("validated node"); swarm_members.entry(groups[index]).or_default().push(id.clone()); }
 // Explicit serial contracts inside coupled groups avoid hidden shared-state concurrency.
 for members in swarm_members.values() { for pair in members.windows(2) {
  let node=graph.nodes.iter_mut().find(|n|n.id==pair[1]).expect("validated node");
  if !node.dependencies.contains(&pair[0]) { node.dependencies.push(pair[0].clone()); }
 }}
 let order=graph.validate()?;
 for n in &graph.nodes { if n.context.ontology_entities.iter().any(|e|ontology.locked_entities.contains(e)) {
  warnings.push(format!("{} intersects active ontology lock; mutation dispatch blocked until replanned",n.id));
 }}
 let mut finish:BTreeMap<String,u64>=BTreeMap::new(); let mut paths:BTreeMap<String,Vec<String>>=BTreeMap::new(); let mut serial=0u64;
 for id in &order { let n=graph.unit(id)?;
  let parent=n.dependencies.iter().max_by_key(|d|finish.get(*d).copied().unwrap_or(0));
  let start=parent.map(|d|finish[d]).unwrap_or(0);
  let end=start.checked_add(n.estimated_ms).ok_or_else(||Error::new(FailureKind::InvalidGraph,"duration overflow"))?;
  serial=serial.checked_add(n.estimated_ms).ok_or_else(||Error::new(FailureKind::InvalidGraph,"duration overflow"))?;
  let mut path=parent.map(|d|paths[d].clone()).unwrap_or_default(); path.push(id.clone()); paths.insert(id.clone(),path); finish.insert(id.clone(),end);
 }
 let end=order.iter().max_by_key(|id|finish[*id]).expect("nonempty graph"); let critical_path=paths[end].clone(); let critical_path_ms=finish[end];
 let mut done=BTreeSet::new(); let mut peak=0;
 while done.len()<count { let ready:Vec<_>=graph.nodes.iter().filter(|n|!done.contains(&n.id) && n.dependencies.iter().all(|d|done.contains(d))).map(|n|n.id.clone()).collect(); peak=peak.max(ready.len()); done.extend(ready); }
 let swarms=swarm_members.into_iter().enumerate().map(|(i,(_,members))|Swarm {id:format!("swarm-{}",i+1),topology:if members.len()>1{"pipeline"}else{"single"}.into(),rationale:"semantic coupling, shared ownership and relational density; independent groups may fan out".into(),members}).collect();
 peak=peak.min(graph.policy.max_workers);
 Ok(Plan{graph,ontology,swarms,critical_path,serial_estimate_ms:serial,critical_path_ms,peak_ready:peak,warnings})
}
