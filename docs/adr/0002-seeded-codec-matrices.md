# ADR-0002: Seeded Codec Matrices Are Materialized and Bit-Reproducible

- Status: Accepted
- Date: 2026-10-02
- Issue: #161 (parent #160; applies to #163, #165, #166)

## Context

TurboQuant codecs depend on matrices that are derived from seeds: the legacy codec's centroid
table and projection matrix today, and TurboQuant_prod's rotation and QJL matrix next. Two facts
shape how those matrices may be produced:

- Release IDs hash the digests of every output file, including those matrices. If a generator's
  output depends on the build machine, the same input gets a different release ID on x86_64 and
  aarch64.
- Generators drift. #156 upgraded rand from 0.8 to 0.10, and rand 0.9 changed how
  `random_range(-1.0..=1.0)` maps ChaCha8 output to f32 (rand#1289). Since then about 81% of the
  values in the legacy `centroids.bin` and `projection.bin` differ from pre-#156 builds, each by at
  most 2.4e-7. No test noticed, because `static_release_determinism_test` only compares two builds
  on the same machine.

## Decision

**Materialization.** Only builders run a matrix generator. They write the generated matrices into
the release, and the query side loads them and never regenerates. A generator change therefore
cannot change how an existing release scores. It changes what a rebuild produces, and a rebuild is
a new release with a new release ID.

**Codec identity.** `TurboQuantConfig` (`src/index/codec_config.rs`) names a codec with
`TurboCodecId` and carries every parameter that determines its bytes: seeds, bit widths and
`generator_version`. `TurboQuantConfig::legacy_v1()` is the only definition of the legacy
constants. `TurboCodecId` codes and names are persisted identifiers. They are never renumbered or
renamed, and code 0 means "no codec id".

**Gaussian generator.** New codecs draw N(0, 1) values from one sampler,
`fill_standard_normal(seed, stream_id, out)` (`src/index/gaussian.rs`). It applies Box–Muller to a
ChaCha8 keystream whose key and nonce layout the module defines. `ln` and `sincos` come from the
pure-Rust `libm` crate rather than the platform C library, so for a given `(seed, stream_id)` the
output is bit-identical on every IEEE 754 binary64 target, x86_64 and aarch64 included. The module
docs specify the sequence fully: an independent ChaCha8 + Box–Muller reimplementation reproduces
it. `rand_distr` is not used because its samplers' output may change between its releases.

**Versioning.** Any change to the sampler's output, whether from our code or from a `rand_chacha`
or `libm` upgrade, bumps `GAUSSIAN_GENERATOR_VERSION`, and that version is recorded with the
matrices it produced (#166 decides where). The version also covers the generators built on the
sampler: a codec records one `generator_version`, so a change to how `Rotation::generate`
(`src/index/rotation.rs`) turns draws into a matrix, such as its QR or sign canonicalization,
bumps the same version.

**Golden pins.** Golden tests pin the sampler's leading values, the digest of a 512 × 512 fill and
the digest of a 512 × 512 rotation.
They also pin the legacy assets, v2 index bytes, and v3 release bytes and release ID
(`tests/turbo_legacy_golden_test.rs`). CI runs them on aarch64 against values captured on x86_64.

## Consequences

- Upgrading `rand_chacha` or `libm` may require a generator version bump. The golden tests turn a
  silent drift into a deliberate decision.
- Old releases stay loadable after any generator change, because they carry their own matrices.
  Only rebuilds are affected.
- The legacy generators (`CentroidTable::generate`, `ProjectionMatrix::generate`) predate
  versioning and are recorded as `generator_version = 0`. They still go through rand's
  `random_range`, so a future rand upgrade can move them again. The golden test catches that, but
  there is no version to bump: the choice is to pin rand or accept new legacy release IDs.
- Targets without IEEE 754 binary64 arithmetic, such as x87-only i586, are outside the
  reproducibility guarantee.
