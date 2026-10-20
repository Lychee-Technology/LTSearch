//! The fixtures: synthetic ones generated from a seed at any size, and
//! committed real embeddings.
//!
//! The synthetic fixture is unit vectors drawn from an anisotropic Gaussian
//! mixture. Each vector is `normalize(a·μ + b·c_k + Σ_r s_r·g_r·u_r + e·z/√d)`:
//! a direction μ shared by every vector (real embeddings are not centered, so
//! unrelated texts still have a positive cosine), the center `c_k` of its
//! cluster, a rank-32 component along fixed directions `u_r` with power-law
//! weights `s_r` (the anisotropy), and isotropic noise `z`. Queries are drawn
//! like documents but with more noise. With these weights two documents of
//! the same cluster have a cosine of about 0.6, and of different clusters
//! about 0.15.
//!
//! The synthetic fixture is the performance workload and a secondary quality
//! signal, not a stand-in for a real corpus: how the codecs rank depends on
//! the neighbourhood structure of the vectors. The committed fixtures under
//! `fixtures/` are real LTEmbed embeddings (#177), loaded by [`committed`];
//! each one's README says where its vectors came from.
//!
//! Every synthetic draw comes from [`StandardNormalStream`], which is
//! bit-for-bit reproducible across x86_64 and aarch64, and every vector has
//! its own stream, so the documents of a smaller fixture are a prefix of a
//! larger one and generation parallelizes.

use std::fs;
use std::path::Path;

use ltsearch::index::StandardNormalStream;
use rayon::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const DIM: usize = 512;

/// Names the generator's output. Bump it on any change to the vectors below;
/// the committed baseline pins each fixture's digest, so a change that isn't
/// bumped fails the gate rather than quietly moving the numbers.
const GENERATOR: &str = "agm-v1";

const SEED: u64 = 168;
const CLUSTERS: usize = 64;
const ANISOTROPIC_RANK: usize = 32;
const ANISOTROPIC_DECAY: f64 = 0.75;

const MEAN_WEIGHT: f64 = 0.5;
const CLUSTER_WEIGHT: f64 = 0.8;
const ANISOTROPIC_WEIGHT: f64 = 0.5;
const DOC_NOISE: f64 = 0.6;
const QUERY_NOISE: f64 = 0.8;

// Stream ids: the high 32 bits name what is drawn, the low 32 bits index it.
const STREAM_MEAN: u64 = 0;
const STREAM_CLUSTER: u64 = 1 << 32;
const STREAM_DIRECTION: u64 = 2 << 32;
const STREAM_DOC: u64 = 3 << 32;
const STREAM_QUERY: u64 = 4 << 32;

pub type Vector = [f32; DIM];

pub struct Dataset {
    pub name: String,
    /// What made the vectors, for the report: the synthetic generator's
    /// version, or the committed fixture's name.
    pub generator: String,
    /// The embedding model the fixture's releases record.
    pub model_id: String,
    pub docs: Vec<Vector>,
    pub queries: Vec<Vector>,
    /// SHA-256 of the counts and every value, see [`digest`].
    pub digest: String,
}

/// The `docs`-document, `queries`-query synthetic fixture.
pub fn synthetic(docs: usize, queries: usize) -> Dataset {
    assert!(docs < 1 << 32 && queries < 1 << 32, "fixture too large");
    let mixture = Mixture::new();
    let docs: Vec<Vector> = (0..docs)
        .into_par_iter()
        .map(|i| mixture.sample(STREAM_DOC + i as u64, i % CLUSTERS, DOC_NOISE))
        .collect();
    let queries: Vec<Vector> = (0..queries)
        .into_par_iter()
        .map(|i| mixture.sample(STREAM_QUERY + i as u64, i % CLUSTERS, QUERY_NOISE))
        .collect();
    let digest = digest(&docs, &queries);
    Dataset {
        name: format!("synthetic-{GENERATOR}-n{}", docs.len()),
        generator: GENERATOR.into(),
        model_id: format!("synthetic/{GENERATOR}"),
        docs,
        queries,
        digest,
    }
}

/// The committed real-embedding fixtures, by directory under `fixtures/`.
/// Every run includes them.
pub const COMMITTED: [&str; 1] = ["ltembed-uscode-v1"];

const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/turbo_bench/fixtures");
/// The embedder writes unit vectors; a norm off by more than this means the
/// file isn't what the manifest says.
const NORM_TOLERANCE: f64 = 1e-3;

/// What the harness reads from a committed fixture's `manifest.json`. The
/// provenance the manifest also records is for readers.
#[derive(Deserialize)]
struct Manifest {
    docs: usize,
    queries: usize,
    dim: usize,
    model_id: String,
    digest: String,
}

/// The committed fixture `fixtures/<name>`.
pub fn committed(name: &str) -> Result<Dataset, String> {
    load_committed(name, &Path::new(FIXTURES_DIR).join(name))
}

/// A fixture directory: `manifest.json`, and `docs.f32` and `queries.f32`,
/// which hold 512-d vectors as little-endian f32, one after another. The
/// vectors must be unit length and match the manifest's counts and digest.
fn load_committed(name: &str, dir: &Path) -> Result<Dataset, String> {
    let read = |file: &str| {
        let path = dir.join(file);
        fs::read(&path).map_err(|error| format!("reading {}: {error}", path.display()))
    };
    let manifest: Manifest = serde_json::from_slice(&read("manifest.json")?)
        .map_err(|error| format!("{}/manifest.json: {error}", dir.display()))?;
    if manifest.dim != DIM {
        return Err(format!(
            "{}: {}-d vectors, the codecs take {DIM}",
            dir.display(),
            manifest.dim
        ));
    }
    let vectors = |file: &str, count: usize| {
        unit_vectors(&read(file)?, count)
            .map_err(|error| format!("{}/{file}: {error}", dir.display()))
    };
    let docs = vectors("docs.f32", manifest.docs)?;
    let queries = vectors("queries.f32", manifest.queries)?;
    let digest = digest(&docs, &queries);
    if digest != manifest.digest {
        return Err(format!(
            "{}: the vectors' digest {digest} isn't the manifest's {}",
            dir.display(),
            manifest.digest
        ));
    }
    Ok(Dataset {
        name: name.into(),
        generator: name.into(),
        model_id: manifest.model_id,
        docs,
        queries,
        digest,
    })
}

/// `count` unit vectors from little-endian f32 `bytes`.
fn unit_vectors(bytes: &[u8], count: usize) -> Result<Vec<Vector>, String> {
    if count.checked_mul(DIM * 4) != Some(bytes.len()) {
        return Err(format!(
            "{} bytes, expected {count} vectors of {DIM} f32",
            bytes.len()
        ));
    }
    bytes
        .chunks_exact(DIM * 4)
        .enumerate()
        .map(|(i, chunk)| {
            let mut vector = [0.0; DIM];
            for (value, bytes) in vector.iter_mut().zip(chunk.chunks_exact(4)) {
                *value = f32::from_le_bytes(bytes.try_into().expect("4 bytes"));
            }
            let norm = vector
                .iter()
                .map(|&v| f64::from(v) * f64::from(v))
                .sum::<f64>()
                .sqrt();
            if norm.is_nan() || (norm - 1.0).abs() > NORM_TOLERANCE {
                return Err(format!("vector {i} has norm {norm}, expected 1"));
            }
            Ok(vector)
        })
        .collect()
}

/// SHA-256 over the document and query counts (`u64` little-endian) and then
/// every document and query value (`f32` little-endian), in order.
pub fn digest(docs: &[Vector], queries: &[Vector]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((docs.len() as u64).to_le_bytes());
    hasher.update((queries.len() as u64).to_le_bytes());
    for value in docs.iter().chain(queries).flatten() {
        hasher.update(value.to_le_bytes());
    }
    hex::encode(hasher.finalize())
}

struct Mixture {
    mean: Vec<f64>,
    centers: Vec<Vec<f64>>,
    directions: Vec<Vec<f64>>,
    direction_weights: Vec<f64>,
}

impl Mixture {
    fn new() -> Self {
        let raw_weights: Vec<f64> = (1..=ANISOTROPIC_RANK)
            .map(|r| (r as f64).powf(-ANISOTROPIC_DECAY))
            .collect();
        let norm = raw_weights.iter().map(|w| w * w).sum::<f64>().sqrt();
        Self {
            mean: unit_normal(STREAM_MEAN),
            centers: (0..CLUSTERS as u64)
                .map(|k| unit_normal(STREAM_CLUSTER + k))
                .collect(),
            directions: (0..ANISOTROPIC_RANK as u64)
                .map(|r| unit_normal(STREAM_DIRECTION + r))
                .collect(),
            direction_weights: raw_weights
                .iter()
                .map(|w| ANISOTROPIC_WEIGHT * w / norm)
                .collect(),
        }
    }

    fn sample(&self, stream_id: u64, cluster: usize, noise: f64) -> Vector {
        let mut stream = StandardNormalStream::new(SEED, stream_id);
        let mut loadings = [0.0; ANISOTROPIC_RANK];
        stream.fill(&mut loadings);
        let mut vector = [0.0; DIM];
        stream.fill(&mut vector);

        let noise_scale = noise / (DIM as f64).sqrt();
        for (j, value) in vector.iter_mut().enumerate() {
            *value = MEAN_WEIGHT * self.mean[j]
                + CLUSTER_WEIGHT * self.centers[cluster][j]
                + noise_scale * *value;
        }
        for ((direction, weight), loading) in self
            .directions
            .iter()
            .zip(&self.direction_weights)
            .zip(loadings)
        {
            for (value, component) in vector.iter_mut().zip(direction) {
                *value += weight * loading * component;
            }
        }

        let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
        vector.map(|v| (v / norm) as f32)
    }
}

fn unit_normal(stream_id: u64) -> Vec<f64> {
    let mut vector = vec![0.0; DIM];
    StandardNormalStream::new(SEED, stream_id).fill(&mut vector);
    let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
    vector.iter().map(|v| v / norm).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &Vector, b: &Vector) -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum()
    }

    #[test]
    fn vectors_are_unit_length() {
        let dataset = synthetic(64, 8);
        for vector in dataset.docs.iter().chain(&dataset.queries) {
            assert!((dot(vector, vector) - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn smaller_fixture_is_a_prefix_of_a_larger_one() {
        let small = synthetic(10, 4);
        let large = synthetic(100, 4);
        assert_eq!(small.docs[..], large.docs[..10]);
        assert_eq!(small.queries, large.queries);
        assert_ne!(small.digest, large.digest);
        assert_eq!(small.name, "synthetic-agm-v1-n10");
    }

    #[test]
    fn same_cluster_documents_are_closer_than_other_clusters() {
        let dataset = synthetic(4 * CLUSTERS, 0);
        let docs = &dataset.docs;
        let mut same = 0.0;
        let mut other = 0.0;
        for i in 0..CLUSTERS {
            same += dot(&docs[i], &docs[i + CLUSTERS]);
            other += dot(&docs[i], &docs[(i + 1) % CLUSTERS]);
        }
        let (same, other) = (same / CLUSTERS as f64, other / CLUSTERS as f64);
        assert!((0.5..0.7).contains(&same), "same-cluster cosine {same}");
        assert!(
            (0.05..0.25).contains(&other),
            "cross-cluster cosine {other}"
        );
    }

    #[test]
    fn digest_covers_counts_and_values() {
        let dataset = synthetic(3, 2);
        assert_eq!(digest(&dataset.docs, &dataset.queries), dataset.digest);
        // Moving a vector from the queries to the documents keeps the bytes
        // but not the counts.
        let mut docs = dataset.docs.clone();
        docs.push(dataset.queries[0]);
        assert_ne!(digest(&docs, &dataset.queries[1..]), dataset.digest);
        let mut queries = dataset.queries.clone();
        queries[1][0] = -queries[1][0];
        assert_ne!(digest(&dataset.docs, &queries), dataset.digest);
    }

    #[test]
    fn committed_fixtures_match_their_manifests() {
        for name in COMMITTED {
            let dataset = committed(name).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(
                (dataset.name.as_str(), dataset.generator.as_str()),
                (name, name)
            );
            assert!(!dataset.docs.is_empty() && !dataset.queries.is_empty());
        }
    }

    fn write_fixture(dir: &Path, docs: &[Vector], queries: &[Vector], digest: &str) {
        let bytes = |vectors: &[Vector]| -> Vec<u8> {
            vectors
                .iter()
                .flatten()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        };
        fs::write(dir.join("docs.f32"), bytes(docs)).unwrap();
        fs::write(dir.join("queries.f32"), bytes(queries)).unwrap();
        let manifest = serde_json::json!({
            "docs": docs.len(),
            "queries": queries.len(),
            "dim": DIM,
            "model_id": "test/model",
            "digest": digest,
            "generated_with": {"anything": "the harness doesn't read"},
        });
        fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    #[test]
    fn a_committed_fixture_loads_as_written() {
        let source = synthetic(5, 3);
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), &source.docs, &source.queries, &source.digest);
        let loaded = load_committed("test", dir.path()).unwrap();
        assert_eq!((loaded.docs, loaded.queries), (source.docs, source.queries));
        assert_eq!(loaded.digest, source.digest);
        assert_eq!(loaded.model_id, "test/model");
    }

    #[test]
    fn a_committed_fixture_that_differs_from_its_manifest_is_refused() {
        let source = synthetic(5, 3);
        let dir = tempfile::tempdir().unwrap();
        let load_error = |docs: &[Vector], digest: &str| {
            write_fixture(dir.path(), docs, &source.queries, digest);
            refusal(dir.path())
        };

        // A changed value.
        let mut changed = source.docs.clone();
        changed[2].swap(0, 1);
        let error = load_error(&changed, &source.digest);
        assert!(error.contains("digest"), "{error}");

        // Not unit length, even with a digest that matches.
        let scaled: Vec<Vector> = source.docs.iter().map(|v| v.map(|x| 2.0 * x)).collect();
        let error = load_error(&scaled, &digest(&scaled, &source.queries));
        assert!(error.contains("norm"), "{error}");

        // A file that doesn't hold the manifest's count.
        write_fixture(dir.path(), &source.docs, &source.queries, &source.digest);
        let path = dir.path().join("queries.f32");
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 4);
        fs::write(&path, bytes).unwrap();
        let error = refusal(dir.path());
        assert!(
            error.contains("queries.f32") && error.contains("bytes"),
            "{error}"
        );
    }

    fn refusal(dir: &Path) -> String {
        match load_committed("test", dir) {
            Ok(_) => panic!("loaded a fixture that differs from its manifest"),
            Err(error) => error,
        }
    }
}
