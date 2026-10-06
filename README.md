# Colony (`:e`)

Colony is a deterministic, transport-independent **control plane** for bounded LLM work.
It takes a graph of tightly scoped work units, checks every contract, groups
semantically coupled units into swarms, assigns each unit a provider/model by
comparative advantage, and drives a `pending → running → awaiting verification →
validated` lifecycle under integer budget authority. A unit only counts as done when
**trusted** evidence from a host-supplied verifier accepts it. A worker's own output
never certifies itself.

**What Colony is not:** it makes no HTTP calls, opens no sockets, runs no async
runtime, does no filesystem I/O in its libraries, and reads no clocks. It does not
call models or run tools. The host does those things through two traits, `Mesut`
(execution) and `Verifier` (trusted checks), and drives time by calling `tick`.
Live adapters belong to the ELCI provider layer and are **out of scope for v0.1.x**.
The CLI includes only a synthetic simulation.

## Quickstart

All commands work offline. Rust 1.85 or newer is required.

```sh
cargo build --offline
cargo run --offline -q -- validate examples/request.json
cargo run --offline -q -- plan     examples/request.json
cargo run --offline -q -- simulate examples/request.json
```

> If your checkout path contains `:` (as `:e` does), Cargo cannot use a target
> directory inside it. Set `CARGO_TARGET_DIR` to a path without a colon, or add a local,
> git-ignored `.cargo/config.toml` with `[build] target-dir = "..."`.

## Usage

The binary is `colony`. Its input is one JSON request
(`{"graph", "ontology", "registry"}`). See [`examples/request.json`](examples/request.json):
four read-only audit units, where `synthesis` depends on the other three. Every
command validates the whole request first. On any error it writes `colony: <error>`
to stderr and exits with status `1`. Contract errors print as
`<FailureKind>: <message>`, for example `colony: InvalidGraph: dependency cycle`.

```text
$ colony --help
Colony (:e) 0.1.1

Usage: colony <plan|validate|simulate> REQUEST.json

plan      Compile bounded work candidates and ontology into an explained JSON plan
validate  Check contracts, policy, semantic DAG and provider eligibility
simulate  Exercise lifecycle with synthetic artifacts; runs no inference or tools

Live execution is available through the Rust Mesut and Verifier adapter traits.
See examples/request.json and docs/architecture.md.

$ colony --version
colony 0.1.1
```

**validate** checks graph contracts, policy, the planned DAG and provider
eligibility for every unit (call quotas are consumed in topological order):

```text
$ colony validate examples/request.json
{"valid":true,"work_units":4}
```

**plan** prints the full plan: swarms, critical path, estimates, and one
assignment per unit that ranks and explains every candidate. Abridged from the real
output:

```text
$ colony plan examples/request.json
mode               plan_only
swarms             swarm-1 [dependency-audit]  swarm-2 [platform-audit]
                   swarm-3 [test-analysis]     swarm-4 [synthesis]
critical_path      test-analysis → synthesis   (2000 ms; serial estimate 4000 ms)
peak_ready         3
assignments        dependency-audit → example-local/research
                   platform-audit   → example-local/code
                   test-analysis    → example-local/reasoning
                   synthesis        → example-local/reasoning
top candidate      {"provider": "example-local", "model": "research",
                    "score": 0.0001449456975772765, "estimated_cost_micros": 4,
                    "reasons": ["capability source: benchmarked; fit 0.826; confidence 0.800",
                                "quota 100; latency 1000ms; shadow price 1; coordination 1.140"]}
```

**simulate** runs the real runtime state machine with a synthetic executor and
verifier. It performs no inference, tests, Git changes or Mesut execution:

```text
$ colony simulate examples/request.json
mode       simulation_complete
warning    Synthetic artifacts and verifier checks. No inference, real tests,
           Git changes or Mesut execution occurred.
states     all four units "validated"; completed: true
reserved   {"calls": 4, "money_micros": 400000, "tokens": 40000}
events     18: ColonyCreated, then per unit WorkerAssigned → ArtifactProduced →
           ValidationStarted → WorkerCompleted, then ColonyCompleted
evidence   per unit: the required checks with evidence refs, acceptance, verifier
```

## Safety invariants

- **Fail closed.** Every rejection is a typed `colony_core::Error` with a specific
  `FailureKind`. Library code returns `Result` and has no `unwrap`, `expect` or
  unchecked indexing in its paths.
- **Integer money.** Money, tokens and calls are `u64` (money in micro-units), and all
  accumulation is checked. Floats are used only for capability scores, which must be
  finite and within `[0, 1]`.
- **Deterministic.** Ordered maps throughout, no randomness, no wall-clock reads. The
  same input gives the same plan, the same allocation and the same event sequence.
- **Authority only narrows.** A child policy cannot widen providers, classification,
  mutation rights, workers, depth, deadline or budget. Data cannot flow from a
  higher-classified unit into a lower-classified one, and `LOCAL_ONLY` work runs only
  on local resources.
- **Conservative accounting.** Each dispatch reserves the unit's full budget against
  the colony ledger. Nothing is refunded, and a retry needs a new reservation.
- **Trusted evidence only.** A unit is validated only after the host's `Verifier`
  shows the required deterministic checks passed with evidence refs, the acceptance
  criteria are covered, the result is consistent, and semantic approval is given when
  the contract requires it.
- **Strict contracts.** Input wire structs reject unknown fields. Sources must be pinned to
  a 40- or 64-hex SHA, and paths must be safe and relative.

The full rules and the threat model are in [docs/architecture.md](docs/architecture.md).

## Crate map

Dependencies point one way: `core ← policy/provider ← planner/allocator ← runtime ← cli`.

| Crate | Role |
|---|---|
| `colony-core` | Wire contracts (`WorkGraph`, `WorkUnit`, `Evidence`, …), graph validation, `Error`/`FailureKind`, the `SemanticSource`/`Preflight` traits |
| `colony-policy` | Unit admission, policy inheritance (`inherit`), budget `Ledger` and atomic `reserve_ancestors` |
| `colony-provider` | `Registry`/`Resource` metadata, capability provenance, integer cost, the `InferenceProvider` trait |
| `colony-planner` | Swarm coupling, semantic edge injection, critical path and peak-width estimates |
| `colony-allocator` | Hard eligibility filters and comparative-advantage scoring |
| `colony-runtime` | The `Colony` state machine and the `Mesut`/`Verifier` traits |
| `colony-cli` | The `colony` binary (`plan`, `validate`, `simulate`) and the benchmarks |

## Development

Every command is offline. These four are the release gate (also in `deliver.toml`):

```sh
cargo fmt --all -- --check
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo run --offline -q -- simulate examples/request.json
```

Other useful commands:

```sh
deliver --spec deliver.toml --strict              # the full release gate
cargo bench --offline -p colony-cli --bench lifecycle
```

- The only external crates are `serde` and `serde_json`. Do not add others.
- Tests are hermetic: no network, no time, no randomness. Only the CLI end-to-end
  tests touch the filesystem, through temp files.
- Benchmarks are std-only (`harness = false`, no criterion) and run on stable Rust.
  Recorded numbers are in [docs/benchmarks.md](docs/benchmarks.md).

## License

MIT. See [LICENSE](LICENSE).
