# Benchmarks

The benchmarks are std-only and run on stable Rust (`harness = false`, no criterion),
in `crates/colony-cli/benches/lifecycle.rs`:

```sh
cargo bench --offline -p colony-cli --bench lifecycle
```

Each case runs a fixed number of operations per sample, repeats for 7 samples after
one warm-up, and reports the **minimum** ns/op.

## Workloads

The graphs are built from the first unit in `examples/request.json`. Every node has
its own ontology entity, so no coupling is triggered. The registry is the example's
three local models, with quota and concurrency large enough that capacity never
limits dispatch.

| Shape | Edges |
|---|---|
| `chain` | node *i* depends on *i−1*. No parallelism, so the critical path equals the serial estimate. |
| `fan-out` | one root; *n−2* leaves depend on the root; one sink depends on every leaf |
| `dense` | node *i* depends on every node in `max(0, i−16)..i` (≈16 edges per node) |

| Bench | Measures |
|---|---|
| `validate` | `WorkGraph::validate` (contract checks and topological order) |
| `plan` | `colony_planner::plan` (admission, coupling, edge injection, three validations, estimates), including the graph clone |
| `allocate` | `colony_allocator::allocate` for the last unit against 3 resources |
| `lifecycle` | the full simulated run: `plan` → `Colony::new` (re-plan) → dispatch/receive/verify of every unit until `complete()` |

## Results

Recorded 2026-10-06 on the following environment:

- rustc 1.98.1 (48a229cea 2026-09-01), `bench` profile (release, opt-level 3)
- Intel(R) Core(TM) 5 120U, 12 logical CPUs, Linux 6.18.7

| bench | shape | 10 nodes | 100 nodes | 500 nodes |
|---|---|---:|---:|---:|
| validate | chain | 5.2 µs | 122 µs | 1.94 ms |
| validate | fan-out | 4.2 µs | 111 µs | 1.34 ms |
| validate | dense | 7.6 µs | 521 µs | 9.49 ms |
| plan | chain | 48 µs | 1.52 ms | 34.3 ms |
| plan | fan-out | 36 µs | 648 µs | 6.60 ms |
| plan | dense | 65 µs | 3.40 ms | 61.2 ms |
| allocate | chain | 1.3 µs | 1.3 µs | 1.3 µs |
| allocate | fan-out | 1.0 µs | 1.3 µs | 1.8 µs |
| allocate | dense | 1.3 µs | 1.3 µs | 1.3 µs |
| lifecycle | chain | 170 µs | 4.85 ms | 90.4 ms |
| lifecycle | fan-out | 169 µs | 2.88 ms | 46.9 ms |
| lifecycle | dense | 229 µs | 10.1 ms | 181 ms |

<details><summary>Raw output</summary>

```text
bench      shape    nodes  iters      min ns/op
validate   chain       10   2000           5241
plan       chain       10    200          48075
allocate   chain       10   2000           1307
lifecycle  chain       10    200         169891
validate   chain      100    100         122087
plan       chain      100     10        1519784
allocate   chain      100    100           1301
lifecycle  chain      100     10        4845070
validate   chain      500      8        1942443
plan       chain      500      2       34302200
allocate   chain      500      8           1289
lifecycle  chain      500      2       90392567
validate   fan-out     10   2000           4248
plan       fan-out     10    200          35929
allocate   fan-out     10   2000            981
lifecycle  fan-out     10    200         169069
validate   fan-out    100    100         111450
plan       fan-out    100     10         648131
allocate   fan-out    100    100           1303
lifecycle  fan-out    100     10        2881271
validate   fan-out    500      8        1335100
plan       fan-out    500      2        6600215
allocate   fan-out    500      8           1793
lifecycle  fan-out    500      2       46886338
validate   dense       10   2000           7609
plan       dense       10    200          65210
allocate   dense       10   2000           1300
lifecycle  dense       10    200         228567
validate   dense      100    100         521219
plan       dense      100     10        3399373
allocate   dense      100    100           1279
lifecycle  dense      100     10       10102299
validate   dense      500      8        9494551
plan       dense      500      2       61172936
allocate   dense      500      8           1283
lifecycle  dense      500      2      181148333
```

</details>

## Observations

- **`allocate` is flat** at about 1.3 µs. It depends on the registry size, not the
  graph size.
- **Validation, planning and the lifecycle grow super-linearly.** Going from 10 to 500
  nodes (×50) costs ×370 for `validate` on a chain and ×530 for the chain lifecycle.
  The sources are known and deliberate simplicity choices:
  - Topological sort removes each finished id from every remaining dependency set.
  - `WorkGraph::unit` is a linear scan.
  - `plan` validates three times and runs a pairwise O(n²) coupling pass.
  - `Colony::ready` rescans every unit on each dispatch.
- **Absolute cost is small next to the work it orchestrates.** A 500-unit dense colony
  plans, dispatches, receives and verifies everything in about 0.18 s, while a single
  LLM call takes seconds. Indexing units by id would be the first optimisation if
  graphs much larger than 500 units are needed.
- Numbers come from a laptop CPU with frequency scaling. Treat them as orders of
  magnitude and re-run the benchmark before comparing across changes.
