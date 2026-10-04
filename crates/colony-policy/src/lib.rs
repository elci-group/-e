//! Fail-closed authority and hierarchical reservations. No floating-point accounting.
use colony_core::*;

pub fn validate_unit(unit:&WorkUnit,policy:&Policy)->Result<()> {
 ensure(unit.classification<=policy.max_classification,FailureKind::PolicyViolation,"classification exceeds authority")?;
 ensure(unit.mutation.is_none() || policy.allow_mutation,FailureKind::PolicyViolation,"mutation not authorised")?;
 ensure(unit.budget.fits(policy.budget),FailureKind::BudgetExhausted,"unit budget exceeds colony")
}
/// Child policies can narrow but never expand inherited authority.
pub fn inherit(parent:&Policy,child:&Policy)->Result<()> {
 ensure(child.allowed_providers.is_subset(&parent.allowed_providers)
 && child.max_classification<=parent.max_classification
 && (!child.allow_mutation || parent.allow_mutation)
 && (!parent.require_human_approval || child.require_human_approval)
 && child.max_workers>0 && child.max_workers<=parent.max_workers
 && child.max_depth<parent.max_depth && child.deadline_ms<=parent.deadline_ms
 && child.budget.fits(parent.budget),FailureKind::PolicyViolation,"child policy broadens parent authority")
}
#[derive(Debug, Clone)]
pub struct Ledger { limit:Budget, reserved:Budget }
impl Ledger {
 pub fn new(limit:Budget)->Self { Self { limit,reserved:Budget::default() } }
 /// Reservations are conservative: retries consume additional allocation. Nothing is minted on completion.
 pub fn reserve(&mut self,amount:Budget)->Result<()> { let total=self.reserved.checked_add(amount)?; ensure(total.fits(self.limit),FailureKind::BudgetExhausted,"ancestor budget exhausted")?; self.reserved=total; Ok(()) }
 pub fn reserved(&self)->Budget { self.reserved }
}
/// Transactionally reserve across all ancestors before admitting a child.
pub fn reserve_ancestors(ledgers:&mut [&mut Ledger],amount:Budget)->Result<()> {
 ensure(!ledgers.is_empty(),FailureKind::PolicyViolation,"missing ancestor ledger")?;
 for l in ledgers.iter() { ensure(l.reserved.checked_add(amount)?.fits(l.limit),FailureKind::BudgetExhausted,"ancestor budget exhausted")?; }
 for l in ledgers.iter_mut() { l.reserve(amount)?; }
 Ok(())
}
#[cfg(test)] mod tests {
 use super::*;
 #[test] fn reservation_is_atomic() {
 let b=Budget{money_micros:5,tokens:5,calls:1}; let mut a=Ledger::new(b); let mut z=Ledger::new(Budget::default());
 assert!(reserve_ancestors(&mut [&mut a,&mut z],b).is_err()); assert_eq!(a.reserved(),Budget::default());
 }
 #[test] fn overflow_rejected() { let mut l=Ledger::new(Budget{money_micros:u64::MAX,tokens:0,calls:0}); l.reserve(Budget{money_micros:u64::MAX,tokens:0,calls:0}).unwrap(); assert!(l.reserve(Budget{money_micros:1,tokens:0,calls:0}).is_err()); }
}
