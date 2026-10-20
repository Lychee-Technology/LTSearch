# ltembed-uscode-v1

Real LTEmbed embeddings for turbo_bench (#177): 1,879 US Code statute passages
as documents and 200 questions written for them as queries, 512-d. Every
`turbo_bench run` measures this fixture, and CI's `turbo-bench` job gates it
against `../../baseline.json`.

| File | Contents |
|---|---|
| `docs.jsonl` | One passage per line: `citation` and `text`. |
| `queries.jsonl` | One question per line: `text`, and `about`, the citation of the passage it was written for. |
| `docs.f32`, `queries.f32` | The embeddings in line order, 512 little-endian f32 each. These files are the source of truth. |
| `manifest.json` | Counts, dimension, model ID, the digest the harness checks, and the pins the vectors were generated with. |

## Corpus

Every section of five titles of the United States Code, 2024 Main Edition
(laws enacted through January 6, 2025): 4 (flag, seal, seat of government and
the states), 9 (arbitration), 11 (bankruptcy), 17 (copyrights) and 35
(patents). `../../fixture_gen/uscode_passages.py` downloads govinfo's HTML
edition of each title, checks it against a pinned sha256 and writes
`docs.jsonl`.

A passage is statute text: the section heading, then the section's statutory
paragraphs, without source credits or editorial notes. A section longer than
1,500 characters is split at paragraph boundaries into parts of about even
size, each repeating the heading, and its citation names the part, as in
"11 U.S.C. 101 (part 8 of 38)". Repealed, omitted and transferred sections,
which have no statute text, are left out.

Whole titles keep the corpus's own density. Long sections give many close
neighbours, which is the hard case for a quantized ranking: 17 U.S.C. 115
alone is 104 passages.

US Code text is a work of the US government and isn't subject to copyright
(17 U.S.C. 105).

## Queries

200 questions of the kind someone types into a legal search tool, written
for this fixture. Question i was written about passage ⌊i · 1879 / 200⌋, which
its `about` field names. The questions paraphrase instead of quoting and don't
cite section numbers. Most are full questions; some are short lowercase
queries.

The quality metrics compare each codec's ranking with the exact f32 ranking,
not with relevance judgements, so the questions only need to be realistic
queries. `about` records where a question came from; it isn't a relevance
label.

## Embeddings

- Model: `jinaai/jina-embeddings-v5-text-nano-retrieval` at Hugging Face
  revision `ac5d898c`, the Q5_K_M GGUF that production uses. The pins are in
  `sam/builder.Dockerfile`.
- Engine: LTEmbed `0390d9c`, the revision LTSearch's `Cargo.lock` pins, with
  `EngineConfig::default()`: the model's 768-d output truncated to 512 dims
  (Matryoshka) and L2-normalized. Passages are embedded as `Document` input
  and questions as `Query` input, so the engine prefixes them with
  `Document: ` and `Query: `. Each engine runs one llama.cpp thread, as in
  production.
- Platform: linux/arm64 with the static llama.cpp archives built for
  Graviton2, on the Lambda builder's Amazon Linux 2023 base image. These
  vectors were generated under qemu-user emulation on an x86_64 host.

The model's weights are licensed CC BY-NC 4.0 by Jina AI, per its model card.
These vectors are that model's output. This fixture doesn't settle whether
the licence's terms reach them; production's use of the same model raises the
same question.

## Regenerating

```sh
# The passages. Downloads about 7 MB from govinfo and checks the pinned sha256.
python3 examples/turbo_bench/fixture_gen/uscode_passages.py \
  examples/turbo_bench/fixtures/ltembed-uscode-v1/docs.jsonl

# The vectors and manifest.json. Builds and runs the embedder in a linux/arm64
# container; CONTAINER_ENGINE=podman to use podman.
examples/turbo_bench/fixture_gen/generate.sh examples/turbo_bench/fixtures/ltembed-uscode-v1
```

No script writes `queries.jsonl`, and neither command changes it.

On an x86_64 host the container runs under qemu-user emulation, which needs a
registered binfmt handler for aarch64. There, the embedder's build took about
8 minutes and embedding about 3 hours on 10 workers of a 12-thread desktop.

Another LTEmbed revision, quantization or platform may not reproduce these
vectors bit for bit, and govinfo may republish its HTML, which fails the
pinned sha256. Regenerated vectors are a new fixture (see `../../README.md`).
