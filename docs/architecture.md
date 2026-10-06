# Colony architecture

This document describes Colony v0.1.1 as implemented. Every rule below is enforced in
the code, and each section names where.

## 1. Components and data flow

```text
 host / adapters                                  Colony (pure, deterministic)
 ───────────────                                  ────────────────────────────
 SemanticSource ──▶ OntologySlice ─┐
 Preflight ───────▶ ContextManifest├─▶ WorkGraph.validate ─▶ planner::plan ─▶ Plan
 request JSON ────▶ WorkGraph ─────┘     (core)                (planner)        │
                                                                                ▼
 Registry (provider metadata) ─────────────────────────────▶ Colony::new (re-plans)
                                                                                │
 host event loop ── tick(now) ─────────────────────────────▶ Colony ◀───────────┘
                                                              │  dispatch: allocate
 Mesut::submit ◀── Dispatch (unit, assignment, attempt) ◀─────┤  (allocator) + Ledger
 Mesut ── WorkerResult ──▶ Colony::receive ── artifact checks ┤  reservation (policy)
 Verifier::verify ──▶ Evidence ──▶ Colony::verify ── gates ───┘
```

| Crate | Depends on | Owns |
|---|---|---|
| `colony-core` | serde | wire types, `WorkGraph::validate`, `Error`/`FailureKind`, `valid_sha`, `valid_path`, `overlaps`, `score` |
| `colony-policy` | core | `validate_unit`, `inherit`, `Ledger`, `reserve_ancestors` |
| `colony-provider` | core | `Registry`, `Resource::effective`/`cost`, `InferenceProvider` |
| `colony-planner` | core, policy | `plan` → `Plan` (swarms, critical path, `peak_ready`, warnings) |
| `colony-allocator` | core, policy, provider | `eligible`, `allocate` → `Assignment` |
| `colony-runtime` | all of the above | `Colony`, `Mesut`, `Verifier`, `Dispatch`, `Snapshot` |
| `colony-cli` | all + serde_json | the `colony` binary and the benchmarks |

The layering is strictly `core ← policy/provider ← planner/allocator ← runtime ← cli`.
No library crate performs I/O, spawns threads, reads clocks or uses `HashMap`
iteration.

## 2. Contracts and graph validation (`colony-core`)

Every *input* wire struct (`WorkGraph`, `Policy`, `Budget`, `WorkUnit` and its parts,
`OntologySlice`, `Relation`, `Registry`, `Resource`, `WorkerResult`, `Artifact`,
`Evidence`, `CheckResult`) uses `#[serde(deny_unknown_fields)]`. Output-only types
(`Plan`, `Swarm`, `Assignment`, `Candidate`, `Dispatch`, `Snapshot`, `Event`) do not.
`WorkGraph::validate` returns the topological order or the first violation:

| Rule | Kind |
|---|---|
| non-blank `colony_id` and `objective`; at least one node | `InvalidGraph` |
| policy: `max_workers > 0`, `deadline_ms > 0`, non-empty `allowed_providers` | `InvalidGraph` |
| node ids are non-blank and unique | `InvalidGraph` |
| non-blank `objective`/`rationale`; non-empty `acceptance` and `expected_outputs`; no blank entries; unique outputs | `InvalidGraph` |
| demand vector, `relational_density`, `uncertainty`, `consequence` are finite and in `[0,1]` | `InvalidGraph` |
| `budget.calls`, `demand.output_tokens`, `estimated_ms` > 0 | `InvalidGraph` |
| `context.input_tokens + demand.output_tokens ≤ budget.tokens` (checked add) | `InvalidGraph` |
| `context.source_sha` is 40 or 64 hex characters | `InvalidGraph` |
| every `relevant_paths` entry is a safe path | `InvalidGraph` |
| at least one non-blank deterministic check | `InvalidGraph` |
| no duplicate dependency, every dependency exists, no cycle | `InvalidGraph` |
| mutation: non-empty `allowed_paths`; allowed paths, forbidden paths and `branch` are all safe paths | `InvalidGraph` |
| a unit's classification ≥ every dependency's classification (no downgrade) | `PolicyViolation` |

A **safe path** is non-empty and relative. It contains no `\`, `:` or NUL, and has no
empty, `.` or `..` component. Classifications are ordered
`PUBLIC < INTERNAL < CONFIDENTIAL < RESTRICTED < LOCAL_ONLY`. Topological order is
deterministic: each round admits the ready ids in lexicographic order.

## 3. Policy and budgets (`colony-policy`)

- `validate_unit` rejects a unit whose classification exceeds `max_classification`
  (`PolicyViolation`), a mutation the policy does not allow (`PolicyViolation`), or a
  unit budget that does not fit the colony budget (`BudgetExhausted`).
- `inherit(parent, child)` accepts a child only if it *narrows* authority: providers
  are a subset; classification ≤ parent; mutation only if the parent allows it; human
  approval kept if the parent requires it; `0 < max_workers ≤ parent`;
  `max_depth < parent`; `deadline_ms ≤ parent`; budget fits the parent. Anything else
  is `PolicyViolation`.
- `Ledger::reserve` adds with `checked_add` (overflow is `BudgetExhausted`) and
  commits only if the total still fits the limit. A failed reservation changes nothing.
- `reserve_ancestors` checks every ledger first and only then commits to all of them,
  so a reservation is all-or-nothing across the hierarchy. An empty ancestor list is
  `PolicyViolation`.

**Ledger semantics.** Reservations are conservative. A dispatch reserves the unit's
full `budget`. Nothing is refunded on completion, failure or a rejected `submit`, so
each retry must fit what remains. `reserved()` therefore bounds total possible
spend, not actual spend.

## 4. Planner: coupling and swarms (`colony-planner`)

`plan(graph, ontology)` validates the graph and checks that every ontology relation
has a valid strength and non-empty endpoints (`ContractViolation`). It admits every
unit through `validate_unit` and requires the *sum* of unit budgets to fit the colony
budget (`BudgetExhausted`).

Two units are **coupled** (placed in the same swarm) if any of these holds:

1. an ontology relation with `strength ≥ 0.5` and kind other than `independent` links
   an entity of one to an entity of the other (in either direction);
2. they share an ontology entity;
3. both mutate, and they use the same branch or have overlapping allowed paths
   (component-aware: `src` overlaps `src/a` but not `srcs`);
4. either has `relational_density ≥ 0.7`.

Coupling is transitive (union of groups). Next, every `depends_on` relation becomes a
real dependency edge (the unit holding `from` depends on the unit holding `to`), and
the graph is re-validated, so a semantic cycle is rejected as `InvalidGraph`. Members
of each swarm are then **serialized** in topological order (each depends on the
previous one), which removes hidden shared-state concurrency. Then the graph is
validated a final time.

Outputs: one `Swarm` per group (`pipeline` if it has several members, `single`
otherwise); `serial_estimate_ms` (the sum of `estimated_ms`, checked);
`critical_path`/`critical_path_ms` (longest path, where ties go to the later unit in
topological order); `peak_ready` (widest ready frontier, capped at `max_workers`); and
a warning for each unit that touches a locked ontology entity.

## 5. Allocator: eligibility and scoring (`colony-allocator`)

`eligible(unit, policy, resource)` is a set of hard filters, checked in order:

| Filter | Kind |
|---|---|
| `validate_unit` | as in §3 |
| provider is in `allowed_providers` | `PolicyViolation` |
| `LOCAL_ONLY` work requires `resource.local` | `PolicyViolation` |
| resource is approved for the unit's classification | `PolicyViolation` |
| `available` and `remaining_calls > 0` | `ProviderUnavailable` |
| input + output fit `context_window`; output ≤ `max_output` | `ContextOverflow` |
| estimated cost ≤ unit `money_micros` | `BudgetExhausted` |

**Cost** is integer: `ceil((input_tokens·input_price + output_tokens·output_price) / 1e6)`
micros, computed in `u128`. An overflow at any step is `BudgetExhausted`.

**Capability provenance** (`Resource::effective`): benchmarked capabilities are used
if present, with confidence 0.8; otherwise declared ones with confidence 0.5. With
`observed.samples > 0`, these are blended toward observed capabilities with weight
`min(s/(s+10), 0.95)`, and confidence becomes `0.5 + 0.5·validated_success_rate`.

**Score** for each eligible resource:

```text
fit          = Σ demandᵢ·abilityᵢ / Σ demandᵢ          (1.0 when demand is all zero)
abundance    = max(min(remaining_calls, 100) / 100, 0.01)
coordination = 1 + relational_density + consequence·uncertainty
score        = fit · confidence · abundance
               ÷ (max(cost, 1) · shadow_price · latency_ms · coordination)
```

Candidates are sorted by score (descending; ineligible candidates last), then by
`(provider, model)` ascending, so the result does not depend on registry order. Every
candidate carries its reasons. If none is eligible, the error is `ProviderUnavailable`
and lists each `provider/model` with the reason it was rejected. The registry is
validated first (`ContractViolation`): unique non-blank identities, positive limits,
`shadow_price` finite and ≥ 1, and all capabilities and success rates in `[0,1]`.

## 6. Runtime state machine (`colony-runtime`)

`Colony::new(plan, registry)` **re-plans** from `plan.graph` and `plan.ontology`, and
ignores any serialized derived fields (swarms, estimates), so a tampered plan cannot
grant authority. It also validates the registry.

```text
           dispatch               receive ok                  verify ok
 Pending ───────────▶ Running ───────────────▶ Awaiting ───────────────▶ Validated
    ▲                  │  receive invalid       Verification    │
    │ repair           │  fail                                  │ verify rejected
    └──── Rejected ◀───┴────────────────────────────────────────┘
 cancel / deadline tick: every state except Validated ──▶ Cancelled
```

`ready()` returns the `Pending` units whose dependencies are all `Validated`. It is
empty once the colony is cancelling.

**dispatch(id, now, safe_width, human_approved, mesut)** runs these gates in order:
monotonic clock (`ContractViolation`); not cancelling and `now < deadline`
(`Cancelled`); the unit is ready (`DependencyFailure`); units that are `Running` or
`AwaitingVerification` number fewer than `min(safe_width, max_workers)`
(`ProviderThrottled`, so `safe_width = 0` stops dispatch); mutation has human approval
when the policy requires it (`PolicyViolation`); mutation does not touch a locked
ontology entity (`SemanticConflict`). Next it allocates against a registry view where
resources at their concurrency limit are unavailable. After that it reserves the
unit budget on the ledger, increments the attempt id (never reused) and calls
`Mesut::submit`. If `submit` fails, the reservation is kept and the unit stays
`Pending`. If it succeeds, the resource's quota drops by one. The `Dispatch` carries
the dependencies' results, plus the prior result and the `ValidationFailed`/
`WorkerFailed` events from any earlier attempt, for repair context.

**receive(attempt, result, now)** requires a `Running` unit whose active attempt,
provider and model all match (`ContractViolation`); late results after a cancel or the
deadline are `Cancelled`. Then the artifacts are checked:

| Check | Kind |
|---|---|
| reported `usage` fits the unit budget | `BudgetExhausted` |
| artifact names are unique and exactly cover `expected_outputs` | `InvalidArtifact` |
| non-blank `reference`; `source_sha` equals the context SHA | `InvalidArtifact` |
| read-only unit: no `changed_paths` | `ContractViolation` |
| mutation: valid `result_sha`; each changed path is safe, inside an allowed path and outside every forbidden path | `ContractViolation` |

A failure rejects the unit (`Rejected`, event `ValidationFailed`). Success moves it to
`AwaitingVerification`.

**verify(id, now, verifier)** requires `AwaitingVerification` (`ContractViolation`) and
a live colony (`Cancelled`). It then calls the host's `Verifier` and checks the
`Evidence` it returns. `work_unit` must match, `verifier` must be non-blank,
`timestamp_ms ≤ now`, `consistent` must be true, and `semantic_approved` must be true
when `semantic_review` is set; a failure here is `ConvergenceFailure`. Every
acceptance criterion must be covered (`ConvergenceFailure`). Every required
deterministic check must be present, passed and have an evidence ref, and every
reported check must pass (`DeterministicValidationFailure`). An error returned by the
verifier itself passes through and rejects the unit.

**fail(id, attempt, error, now)** lets the adapter report an execution failure for the
*active* attempt only (`ContractViolation` otherwise). **repair(id)** moves a
`Rejected` unit back to `Pending`. Validated siblings are kept, and the next dispatch
needs a fresh reservation. **cancel(now, mesut)** sets every non-validated unit to
`Cancelled` and calls `Mesut::cancel` for each active handle. Handles whose
cancellation fails stay active, the event is `CancellationPending`, and the error is
returned so the host can retry. **tick(now, mesut)** cancels once `now ≥ deadline_ms`
and the colony is not complete. After that it only retries outstanding cancellation
ACKs, so repeated ticks do not repeat events. Every entry point except `repair`
rejects a clock that moves backwards.

**Event taxonomy** (each `Event` has a dense `sequence`, the latest host time and an
optional unit): `ColonyCreated`, `WorkerAssigned` (detail `provider/model`),
`ArtifactProduced`, `ValidationStarted`, `WorkerCompleted`, `ValidationFailed`,
`WorkerFailed`, `RepairScheduled`, `CancellationPending`, `ColonyCancelled`,
`ColonyCompleted`. `complete()` is true only when nothing is cancelling and every unit
is `Validated` with evidence.

## 7. Threat model

| Threat | Mitigation |
|---|---|
| **Untrusted worker claims success** | Worker output is only an artifact. Validation needs `Evidence` from the host `Verifier`, gated as in §6. A result must match the active attempt, provider and model, so stale, duplicate, cross-attempt and forged-provider results are rejected. |
| **Classification downgrade** | Dependencies cannot flow to a lower classification. Units cannot exceed policy authority. Resources must be approved per classification, and `LOCAL_ONLY` must run locally. Child policies only narrow (`inherit`). |
| **Mutation scope escape** | Mutation needs policy permission and, when required, host approval. Paths and branches are safe and relative. Changed paths must lie inside allowed paths and outside forbidden ones (component-aware). A result SHA is required. Read-only units may not report changes. Ontology locks block mutation (`SemanticConflict`). |
| **Budget inflation** | Integer, overflow-checked accounting. The sum of unit ceilings must fit the colony. Each dispatch reserves the full unit budget with no refunds. Reported usage beyond the reservation is rejected. Provider quotas and concurrency are enforced. |
| **Tampered plan / time** | The runtime re-plans from the raw graph. The clock must be monotonic. Deadlines are enforced on dispatch, receive, verify and `tick`. |
| **Unknown input fields** | `deny_unknown_fields` on all wire structs blocks smuggled fields such as self-certification flags. |

## 8. Adapter extension points

| Trait | Crate | Implemented by | Contract |
|---|---|---|---|
| `Mesut` | runtime | execution layer | `submit` returns a handle only if the job was accepted, using idempotent attempt ids. It enforces the contract's budget, immutable snapshot, isolated mutation environment and scope. `cancel` succeeds only once all descendant calls and tools have stopped. |
| `Verifier` | runtime | trusted host | Runs deterministic gates before any semantic approval. Never treats worker prose as evidence. |
| `InferenceProvider` | provider | ELCI provider layer | Exposes its `Resource` and runs one `InferenceEnvelope`, which keeps the contract, provider and model in separate typed fields. |
| `SemanticSource` | core | Padagonia | Returns the `OntologySlice` (relations, locked entities) for an objective. Colony keeps no semantic store. |
| `Preflight` | core | Lucid Preflight | Returns the `ContextManifest`. Reads and token accounting happen upstream. |

The CLI's `simulate` implements `Mesut` and `Verifier` with synthetic, clearly labelled
stand-ins. Real adapters are out of scope for v0.1.x.
