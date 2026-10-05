# turbo_bench

Retrieval quality and scan performance of the static tier's codecs (#168). Each
run compares three systems on the same vectors:

- `exact_f32`: brute-force inner product over the f32 vectors, through the
  searcher's own parallel top-k scan (`scan_top_k`). It is the ground truth for
  quality and the reference for latency ratios.
- `legacy_v3`: the v3 static release format and its legacy codec.
- `prod_v4`: the v4 static release format, TurboQuant_prod v1.

Both codecs are measured on releases written by `StaticReleaseBuilder` and
loaded with `MmapIndex::load`, the production path. Every quality query also
goes through `TurboQuantSearcher::search`, and the run fails if the searcher
ranks differently from what the harness measured.

## Running it

```sh
# What CI's turbo-bench job runs: N = 1k and 10k, gated against the baseline.
# On another machine type its performance gate can fail (see Gates).
cargo run --release --example turbo_bench -- run --sizes 1000,10000 --queries 200 \
  --baseline examples/turbo_bench/baseline.json

# N = 100k as well. CI doesn't run it (#178).
cargo run --release --example turbo_bench -- run --sizes 1000,10000,100000 --queries 200

# The quality gate alone. Quality doesn't depend on the build or machine, so a
# dev build gives the same numbers.
cargo run --example turbo_bench -- run --quality-only \
  --baseline examples/turbo_bench/baseline.json

# The cost of the size-optimized shipping profile (opt-level "z"): the same run
# at opt-level 3, in its own target dir so the shipping build stays cached.
CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_TARGET_DIR=target/opt3 \
  cargo run --release --example turbo_bench -- run --sizes 1000,10000,100000
```

The report goes to `target/turbo-bench/report.json` (`--out` to change it),
and a Markdown table of it to stdout. The exit status is 1 if the gate fails and
2 on any other error. `turbo_bench gate --report R --baseline B` checks an
existing report again.

## Reading the report

- Recall@k is the share of the exact top k found in the codec's top k. NDCG@10
  uses the exact inner product as graded relevance, with negative values
  clamped to 0. Both are averaged over queries.
- Score bias and RMSE compare each codec score with the exact inner product.
  They use every (query, doc) pair up to N = 1000, and 1000 evenly spaced docs
  per query above that. v3 scores aren't on the inner-product scale: they lack
  the √(π/2)/m factor (#160), so v3's bias and RMSE only compare with earlier
  v3 runs, never with v4.
- `load_ms_p50` is the fixed cost of one `MmapIndex::load` with a warm page
  cache. It's kept apart from per-query latency.
- Scan latency is the parallel scan plus top-10 selection, excluding query
  prepare, over every query (at least 100). The systems take turns query by
  query, so changes in machine load affect all of them alike.
- Build throughput is reported three ways. The first is the whole
  `StaticReleaseBuilder` run, which encodes on one thread. The other two are
  encode only, on up to 10k docs, single-threaded and on the rayon pool.
- Record bytes per vector include the record file's header. Sidecars, which
  hold the doc ids, text and metadata, are listed per file. Their sizes reflect
  the fixture's short synthetic text, not a real corpus.
- The environment block records the git SHA, build profile and opt-level,
  rayon thread count, CPU, architecture and OS. `--degrade` runs are marked as
  such.

## Gates

`baseline.json` holds the committed numbers and their tolerances:

- **Quality**, for every fixture and codec in the baseline. The gate fails if
  Recall@10 or NDCG@10 drops more than 0.01 below the baseline, or if score
  RMSE rises more than 10% above it. The fixture and codecs are
  deterministic, so any change in these numbers comes from a code change.
- **Performance**, at N = 10k for both codecs. The gate fails if the codec's
  scan p50 divided by the exact-f32 scan p50 of the same run rises more than
  25% above the baseline ratio. The ratio cancels most runner-to-runner speed
  differences, but not differences between machine types. The baseline's
  ratios come from CI's `ubuntu-24.04-arm` runner. On another CPU the gate
  still applies them and prints a note saying the comparison is across
  machines, so the run can fail (an x86 desktop does); locally, gate quality
  alone with `--quality-only`. The gate stays on deliberately: if CI's runner
  hardware changes, a failure asking for a new baseline is better than a gate
  that quietly stops checking.
- Each fixture is pinned by its dataset digest. A changed generator fails the
  gate instead of quietly moving the numbers.

To change the baseline, take the report from the `turbo-bench-report` artifact
of the PR's CI run, so the ratios come from the CI machine type, and then run:

```sh
cargo run --release --example turbo_bench -- baseline --report report.json \
  --out examples/turbo_bench/baseline.json
```

The command keeps the tolerances already in the file, and it refuses a
degraded report. The PR that changes the baseline has to say why the numbers
moved.

`--degrade drop-qjl` scores v4 with its MSE term alone (`norm · mse_term`), to
show that the gate catches a broken estimator. On the synthetic fixtures this
doesn't hurt ranking; it ranks slightly better. It does bias the scores
downward, so only the RMSE gate catches it.

## Dataset

The fixtures are synthetic (#168, decision point 1, option b). They are unit
vectors from an anisotropic Gaussian mixture, generated from a fixed seed at
any N; `dataset.rs` documents the generator. The generation is bit-for-bit
reproducible across x86_64 and aarch64, so the committed digests hold on both.
Smaller fixtures are a prefix of larger ones.

The synthetic data is the performance workload and a secondary quality signal.
It can't show how the codecs rank real embeddings. A committed real-embedding
fixture, which is what #169's v4-against-v3 decision needs, waits on a corpus
and license decision (#177).
