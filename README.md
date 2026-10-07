# Colony (`:e`)

Colony is a deterministic, transport-independent **control plane** for bounded LLM work.
It takes a graph of tightly scoped work units, checks every contract, groups
semantically coupled units into swarms, assigns each unit a provider/model by
comparative advantage, and drives a `pending → running → awaiting verification →
validated` lifecycle under integer budget authority. A unit only counts as done when
**trusted** evidence from a host-supplied verifier accepts it. A worker's own output
never certifies itself.

**What the control plane is not:** the crates below `colony-adapters` make no HTTP
calls, open no sockets, run no async runtime, do no filesystem I/O, and read no
clocks. They do not call models or run tools. The host does those things and drives
time by calling the runtime. `colony-adapters` is that host boundary: Mesut
execution, a trusted verifier, an ELCI inference provider, a Padagonia semantic
source, and Lucid-style preflight. `simulate` stays a synthetic stand-in.
`execute` is the live path.

## Quickstart

The commands below work offline. Rust 1.85 or newer is required. `execute --endpoint` is the one path that opens a socket.

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

**execute** runs the same state machine through the host adapters. `--root` is the
workspace preflight reads. Pass exactly one model path: `--command` (a local program;
repeat `--arg` for its arguments) or `--endpoint` (cleartext `http://` OpenAI-compatible
`/v1/chat/completions`; `--api-key` only with an endpoint). `--ontology` is an optional
Padagonia store. `--approve` records host approval for a mutation the policy requires.
The report is `execution_complete` plus the snapshot and trusted evidence. It does not
include scratch paths or Mesut task ids. Each attempt runs in a private directory,
which is removed when the run ends. Colony copies pinned files into that directory
and does not write outputs back into `--root`.

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

Dependencies point one way:
`core ← policy/provider ← planner/allocator ← runtime ← colony-adapters ← cli`.
Crates below `colony-adapters` stay transport-neutral.

| Crate | Role |
|---|---|
| `colony-core` | Wire contracts (`WorkGraph`, `WorkUnit`, `Evidence`, …), graph validation, `Error`/`FailureKind`, the `SemanticSource`/`Preflight` traits |
| `colony-policy` | Unit admission, policy inheritance (`inherit`), budget `Ledger` and atomic `reserve_ancestors` |
| `colony-provider` | `Registry`/`Resource` metadata, capability provenance, integer cost, the `InferenceProvider` trait |
| `colony-planner` | Swarm coupling, semantic edge injection, critical path and peak-width estimates |
| `colony-allocator` | Hard eligibility filters and comparative-advantage scoring |
| `colony-runtime` | The `Colony` state machine and the `Mesut`/`Verifier` traits |
| `colony-adapters` | Host implementations: `MesutExecutor`, `HostVerifier`, `ElciProvider`, `PadagoniaSource`, `LucidPreflight` |
| `colony-cli` | The `colony` binary (`plan`, `validate`, `simulate`, `execute`) and the benchmarks |

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

- Crates below `colony-adapters` depend only on `serde` and `serde_json`. The adapter
  crate also depends on path `mesut`, path `padagonia`, `sha2`, and `tokio`.
- Control-plane tests do not touch the network, the clock, or randomness. Adapter tests
  spawn a local process and, for the HTTP provider, a loopback listener. They do not
  call the public network. CLI tests use temp files.
- Benchmarks are std-only (`harness = false`, no criterion) and run on stable Rust.
  Recorded numbers are in [docs/benchmarks.md](docs/benchmarks.md).

## License

MIT. See [LICENSE](LICENSE).
