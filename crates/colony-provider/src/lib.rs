//! Provider-neutral metadata and transport boundary. Colony owns no HTTP clients.
use colony_core::*;
use serde::{Deserialize,Serialize};
use std::collections::BTreeSet;
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities { pub reasoning:f64, pub coding:f64, pub research:f64, pub structured_output:f64, pub tool_use:f64 }
impl Capabilities { pub fn vector(&self)->[f64;5] { [self.reasoning,self.coding,self.research,self.structured_output,self.tool_use] } }
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observed { pub capabilities:Capabilities, pub samples:u64, pub validated_success_rate:f64 }
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
 pub provider:String, pub model:String, pub local:bool, pub classifications:BTreeSet<Classification>,
 pub declared:Capabilities, pub benchmarked:Option<Capabilities>, pub observed:Option<Observed>,
 pub context_window:u64, pub max_output:u64, pub latency_ms:u64, pub concurrency:usize,
 pub available:bool, pub remaining_calls:u64, pub input_micros_per_million:u64,
 pub output_micros_per_million:u64, pub shadow_price:f64,
}
impl Resource {
 pub fn key(&self)->String { format!("{}/{}",self.provider,self.model) }
 pub fn effective(&self)->(Capabilities,f64,&'static str) {
  let base=self.benchmarked.as_ref().unwrap_or(&self.declared);
  if let Some(obs)=&self.observed { if obs.samples>0 {
   let weight=(obs.samples as f64/(obs.samples as f64+10.0)).min(0.95);
   let a=base.vector(); let b=obs.capabilities.vector(); let mix=|i:usize|a[i]*(1.0-weight)+b[i]*weight;
   return (Capabilities{reasoning:mix(0),coding:mix(1),research:mix(2),structured_output:mix(3),tool_use:mix(4)},0.5+0.5*obs.validated_success_rate,"observed blend");
  }}
  (base.clone(),if self.benchmarked.is_some(){0.8}else{0.5},if self.benchmarked.is_some(){"benchmarked"}else{"declared"})
 }
 pub fn cost(&self,w:&WorkUnit)->Result<u64> {
  let numerator=u128::from(w.context.input_tokens)*u128::from(self.input_micros_per_million)+u128::from(w.demand.output_tokens)*u128::from(self.output_micros_per_million);
  u64::try_from(numerator.div_ceil(1_000_000)).map_err(|_|Error::new(FailureKind::BudgetExhausted,"cost overflow"))
 }
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry { pub resources:Vec<Resource> }
impl Registry {
 pub fn validate(&self)->Result<()> {
  let mut ids=BTreeSet::new();
  for r in &self.resources {
   ensure(!r.provider.trim().is_empty() && !r.model.trim().is_empty() && ids.insert((r.provider.clone(),r.model.clone())),FailureKind::ContractViolation,"duplicate or empty provider/model")?;
   ensure(r.context_window>0 && r.max_output>0 && r.concurrency>0 && r.latency_ms>0 && r.shadow_price.is_finite() && r.shadow_price>=1.0,FailureKind::ContractViolation,"invalid provider limits")?;
   ensure(r.declared.vector().into_iter().all(score) && r.benchmarked.as_ref().is_none_or(|b|b.vector().into_iter().all(score)) && r.observed.as_ref().is_none_or(|o|o.capabilities.vector().into_iter().all(score) && score(o.validated_success_rate)),FailureKind::ContractViolation,"invalid capabilities")?;
  } Ok(())
 }
 pub fn get(&self,provider:&str,model:&str)->Result<&Resource> { self.resources.iter().find(|r|r.provider==provider && r.model==model).ok_or_else(||Error::new(FailureKind::ProviderUnavailable,"unknown provider/model")) }
}
/// Keep policy, contracts and untrusted data in separate typed fields in adapters.
#[derive(Debug,Clone,Serialize)]
pub struct InferenceEnvelope<'a> { pub contract:&'a WorkUnit, pub temperature:f64, pub provider:&'a str, pub model:&'a str }
/// Implemented by the existing ELCI provider layer, not by the allocator.
pub trait InferenceProvider {
 fn resource(&self)->&Resource;
 fn infer(&mut self,envelope:InferenceEnvelope<'_>)->Result<WorkerResult>;
}
