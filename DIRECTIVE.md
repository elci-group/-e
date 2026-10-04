# Colony Completion Directive v1.0

Date: 2026-10-04 · Owner: elci-group · Repo: `elci-group/-e` · Status: ratified, executing

## 1. Purpose

Colony (`:e`) is a deterministic, transport-independent control plane for orchestrating
bounded LLM work units: it validates work graphs, plans semantically-coupled swarms,
allocates provider/model resources by comparative advantage, and drives a
pending → running → verified lifecycle under integer budget authority with trusted,
never-worker-certified evidence.

This directive defines what "complete" means for v0.1.0 and how we get there.

## 2. Definition of done (acceptance contract)

The release is complete when **all** of the following hold:

1. `deliver --spec deliver.toml --strict` passes 7/7 (file, format, tests, lint, simulation).
2. `README.md` is a real readme (concept, quickstart, usage, invariants, dev guide); a
   `require_line_count` guard is added to `deliver.toml`.
3. `docs/architecture.md` exists and documents components, data flow, the runtime state
   machine, fail-closed properties, and adapter extension points (`Mesut`, `Verifier`,
   `InferenceProvider`, `SemanticSource`, `Preflight`).
4. Test suite covers every `FailureKind` reachable from public APIs, including negative
   paths; `cargo test --workspace --offline` green with zero ignored.
5. Stable-Rust benchmarks (no criterion; harness=false) exist under
   `crates/colony-cli/benches/`, exercise validate/plan/allocate/lifecycle at multiple
   graph sizes, and real measured numbers are recorded in `docs/benchmarks.md`.
6. `cargo clippy --workspace --all-targets --offline -- -D warnings` clean.
7. `uni analyze` findings triaged; real defects fixed; rationale recorded for declined ones.
8. kaptaind produces the release commit; repo pushed to `origin` (github.com:elci-group/-e).

## 3. Non-negotiable architecture invariants

Violating any of these is a defect regardless of tests:

- **I1 Layering.** `core ← policy/provider ← planner/allocator ← runtime ← cli`.
  No dependency inversions, no cycles between crates.
- **I2 Transport neutrality.** No HTTP clients, no sockets, no async runtimes, no
  filesystem or process access in library crates. `Colony` creates no executor/timer;
  the host calls `tick`. Live execution arrives via the `Mesut`/`Verifier` traits only.
- **I3 Integer accounting.** Money and tokens are `u64` integer micro-units; all
  accumulation uses `checked_add`; no floating-point arithmetic on money. Capability
  scores are `f64` but must be finite and in `[0,1]` (`colony_core::score`).
- **I4 Determinism.** `BTreeMap`/`BTreeSet` everywhere ordering matters; no `HashMap`
  iteration, no randomness, no wall-clock reads inside the library. Same input → same
  plan, same allocation, same event sequence.
- **I5 Fail-closed.** Every rejection is a `colony_core::Error` with the most specific
  `FailureKind`. No panics in library paths: no `unwrap`, no `expect`, no unguarded
  indexing, no unreachable-by-convention assumptions. Library code returns `Result`.
- **I6 Trusted evidence only.** `Evidence` is produced exclusively by the host's
  `Verifier` implementation; worker output is never self-certifying. Verification
  gates run deterministic checks before any semantic approval.
- **I7 Strict contracts.** Wire structs keep `#[serde(deny_unknown_fields)]`; graph
  validation keeps SHA anchoring (40/64 hex), safe relative paths, dependency DAG and
  no-classification-downgrade rules.
- **I8 Offline & dependency-free.** No new external crates. Only `serde`/`serde_json`
  (already in tree). Everything builds and tests with `--offline`.
- **I9 Standard style.** `cargo fmt --check` is green at all times after the W1 pass;
  code stays idiomatic rustfmt style.

## 4. Current state (baseline, verified 2026-10-04)

- 7 crates, ~522 lines of dense but complete, non-stub logic. No `todo!`/`unimplemented!`.
- 29 tests pass; clippy `-D warnings` clean; simulation runs end-to-end on
  `examples/request.json` (4 units, all validated).
- Codebase is hand-compressed single-line style: **fmt check fails** (W1 fixes).
- `README.md` is a 5-byte stub; `docs/` is empty although `deliver.toml` and the CLI
  `--help` both promise `docs/architecture.md`.
- `Mesut`/`Verifier` have only a synthetic in-CLI simulation; real adapters are
  **explicitly out of scope** for v0.1.0 (they belong to the ELCI provider layer).
- Git: fresh repo, one commit (`first commit`, README stub only), everything else
  untracked; remote `origin` = `git@github.com:elci-group/-e.git` (exists).

## 5. Workstreams

### W1 — Format baseline (sequential, before swarm)
`cargo fmt --all`; verify `cargo fmt --all -- --check` exits 0; tests still green.
Single pass now prevents formatting oscillation and merge noise later.

### W2 — Documentation (swarm item)
- `README.md`: what Colony is and is not; the four commands with real captured output;
  safety invariants in plain language; crate map; development guide (build/test/lint/
  simulate/bench, all offline); scope note on adapters.
- `docs/architecture.md`: contracts and validation rules; planner coupling/swarm model;
  allocator scoring (fit × confidence × abundance ÷ cost·shadow_price·latency·coordination);
  runtime state machine with event taxonomy; budget ledger semantics (conservative
  reservations, no refunds); threat model (untrusted workers, downgrade attempts,
  mutation scope escapes, budget inflation); adapter extension points.
- `LICENSE` (MIT, matching workspace metadata), `docs/benchmarks.md` placeholder filled
  by W4 with real numbers.
- Every claim must be verified against actual code and command output. No aspirational docs.

### W3 — Test hardening (swarm items, per crate group)
Target: every `FailureKind` reachable from public APIs has at least one deterministic
test; every public function has at least one positive test. Use unit
`#[cfg(test)]` modules or per-crate `tests/` dirs. All tests hermetic: no network, no
filesystem (except CLI e2e), no time, no randomness.

- **core/policy/provider**: duplicate/empty ids, unknown deps, cycles, contract
  emptiness, score bounds (NaN, >1), zero limits, context+output > budget, SHA
  length/hex, path safety (absolute, `\`, `:`, `.`/`..` components, NUL), budget
  `fits`/`checked_add` overflow; `inherit` narrowing every clause; ledger
  reserve/overflow/atomicity; registry duplicates, `shadow_price` NaN/<1, capability
  bounds, observed-blend math, cost overflow.
- **planner/allocator**: semantic `DependsOn` edge injection and resulting cycle
  rejection; coupling triggers (ontology relation ≥0.5, shared entity, mutation path
  overlap, relational_density ≥0.7); in-group serialization; budget sum > colony;
  locked-entity warnings; critical-path/serial estimates; peak_ready ≤ max_workers;
  allocator eligibility rejections per clause; scoring determinism and
  (provider, model) tie-break; no-eligible-provider error enumerates reasons.
- **runtime**: full lifecycle happy path; artifact coverage/provenance rejections;
  late/duplicate/cross-attempt results; `fail` + `repair` re-dispatch with fresh
  reservation; cancel propagates handles and stops dispatch; deadline via `tick`;
  `safe_width=0` pressure stop; human-approval gate for mutation; ontology lock gate;
  evidence acceptance-coverage and all-checks-passed gates.
- **cli**: `--help`/`--version`; `plan`, `validate`, `simulate` on
  `examples/request.json`; malformed JSON, unknown verb, wrong arity → exit 1;
  cyclic graph → non-zero. Use `CARGO_BIN_EXE_colony` + temp files under
  `std::env::temp_dir`.

### W4 — Benchmarks (swarm item)
`crates/colony-cli/benches/lifecycle.rs` with `harness = false` (stable Rust, std-only
timing, no criterion). Synthetic graph generators: chain, fan-out, dense. Measure:
`WorkGraph::validate`, `planner::plan`, `allocator::allocate`, and a full simulated
colony lifecycle at 10/100/500 nodes. Print ops and ns/op; repeat for min-of-N
stability. Record real numbers, rustc version, and CPU model in
`docs/benchmarks.md`. `cargo bench --offline` must work.

### W5 — Robustness audit & hardening (folded into each swarm item)
While touching each crate, audit for: panic paths (`unwrap`/`expect`/indexing),
unchecked arithmetic on `u64` (durations, quotas, sequence numbers), serde gaps
(non-finite floats, empty strings that matter), and error-kind precision. Replace
panics with `Result`; add `checked_*` where accumulation can overflow; tighten
`FailureKind` selection. Public API stays source-compatible; existing 29 tests must
keep passing unmodified unless they encode a bug.

### W6 — Release hygiene (directive owner, post-swarm)
Extend `deliver.toml` (README line-count guard, benchmarks doc check); confirm
`.gitignore` covers `target/` and `.cargo/`; `uni analyze` snapshot; fix/triage;
`kaptaind` release commit; `git push origin main`; report.

## 6. Execution rules

- Swarm items own disjoint files (see §7). Touch only what you own. Cargo build-lock
  contention between parallel agents is normal — wait, do not kill.
- Verify with: `cargo test --workspace --offline`, `cargo clippy --workspace
  --all-targets --offline -- -D warnings`, `cargo fmt --all -- --check`,
  and `cargo run --offline -q -- simulate examples/request.json`.
- If a transient failure clearly originates in another crate's in-progress edit,
  re-run before diagnosing. Never revert another item's files.
- All changes land in one working tree; single release commit at the end via kaptaind.

## 7. File ownership matrix

| Item | Owns |
|---|---|
| W2 docs | `README.md`, `docs/**`, `LICENSE` |
| W3a core/policy/provider | `crates/colony-{core,policy,provider}/` |
| W3b planner/allocator | `crates/colony-{planner,allocator}/` |
| W3c runtime | `crates/colony-runtime/` |
| W3d/W4 cli+bench | `crates/colony-cli/` |
| owner (me) | `Cargo.toml`, `deliver.toml`, `DIRECTIVE.md`, git/release |

## 8. Risk register

- **Formatting oscillation** → single W1 pass before swarm; fmt check in gate (seen in
  sibling projects).
- **Offline dependency scarcity** → I8; benches are std-only harness=false.
- **Parallel-edit conflicts** → §7 ownership; crate-level isolation.
- **Scope creep into live adapters** → out of scope; traits + docs only.
- **Doc drift** → W2 requires captured real output; deliver.toml guards docs existence.
- **Overzealous hardening breaking API** → existing tests immutable; additive changes.

## 9. Release procedure

1. Full gate green (deliver 7/7 + extended checks).
2. `uni analyze --json` snapshot triaged; fixes applied; noted in release notes.
3. `kaptaind-cli analyze` dry-run reviewed, then kaptaind release commit.
4. `git push origin main`; verify remote head.
5. Report: directive vs. outcome, benchmark table, uni findings, release SHA.
