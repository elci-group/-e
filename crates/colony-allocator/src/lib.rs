//! Comparative-advantage allocation after hard policy filtering.
use colony_core::*;
use colony_provider::*;
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Candidate { pub provider:String, pub model:String, pub score:Option<f64>, pub reasons:Vec<String>, pub estimated_cost_micros:Option<u64> }
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Assignment { pub work_unit:String, pub provider:String, pub model:String, pub candidates:Vec<Candidate> }
/// Reusable for primary, fallback, hedging and verifier eligibility, before envelope construction.
pub fn eligible(w:&WorkUnit,p:&Policy,r:&Resource)->Result<()> {
 colony_policy::validate_unit(w,p)?;
 ensure(p.allowed_providers.contains(&r.provider),FailureKind::PolicyViolation,"provider excluded by policy")?;
 ensure(w.classification!=Classification::LocalOnly || r.local,FailureKind::PolicyViolation,"LOCAL_ONLY requires local execution")?;
 ensure(r.classifications.contains(&w.classification),FailureKind::PolicyViolation,"provider not approved for classification")?;
 ensure(r.available && r.remaining_calls>0,FailureKind::ProviderUnavailable,"provider unavailable or quota exhausted")?;
 ensure(w.context.input_tokens.checked_add(w.demand.output_tokens).is_some_and(|v|v<=r.context_window) && w.demand.output_tokens<=r.max_output,FailureKind::ContextOverflow,"context/output does not fit")?;
 ensure(r.cost(w)?<=w.budget.money_micros,FailureKind::BudgetExhausted,"estimated call exceeds work budget")
}
pub fn allocate(w:&WorkUnit,p:&Policy,registry:&Registry)->Result<Assignment> {
 registry.validate()?;
 let mut candidates=Vec::new();
 for r in &registry.resources {
  if let Err(e)=eligible(w,p,r) { candidates.push(Candidate{provider:r.provider.clone(),model:r.model.clone(),score:None,reasons:vec![e.to_string()],estimated_cost_micros:None}); continue; }
  let (cap,confidence,source)=r.effective(); let demand=w.demand.vector(); let ability=cap.vector();
  let sum=demand.iter().sum::<f64>();
  let fit=if sum==0.0 { 1.0 } else { demand.iter().zip(ability).map(|(d,a)|d*a).sum::<f64>()/sum };
  let cost=r.cost(w)?;
  let abundance=(r.remaining_calls.min(100) as f64/100.0).max(0.01);
  let coordination=1.0+w.relational_density+w.consequence*w.uncertainty;
  // Floors make local zero-cost inference finite; shadow pricing preserves scarce resources.
  let value=fit*confidence*abundance/((cost.max(1) as f64)*r.shadow_price*(r.latency_ms as f64)*coordination);
  candidates.push(Candidate {provider:r.provider.clone(),model:r.model.clone(),score:Some(value),estimated_cost_micros:Some(cost),reasons:vec![format!("capability source: {source}; fit {fit:.3}; confidence {confidence:.3}"),format!("quota {}; latency {}ms; shadow price {}; coordination {:.3}",r.remaining_calls,r.latency_ms,r.shadow_price,coordination)]});
 }
 candidates.sort_by(|a,b|b.score.unwrap_or(-1.0).total_cmp(&a.score.unwrap_or(-1.0)).then_with(||(&a.provider,&a.model).cmp(&(&b.provider,&b.model))));
 let best=candidates.first().filter(|c|c.score.is_some()).ok_or_else(||Error::new(FailureKind::ProviderUnavailable,format!("no eligible provider for {}: {}",w.id,candidates.iter().map(|c|format!("{}/{}: {}",c.provider,c.model,c.reasons.join(", "))).collect::<Vec<_>>().join("; "))))?;
 Ok(Assignment{work_unit:w.id.clone(),provider:best.provider.clone(),model:best.model.clone(),candidates})
}
