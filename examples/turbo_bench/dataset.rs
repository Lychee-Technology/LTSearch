//! The synthetic fixture: unit vectors drawn from an anisotropic Gaussian
//! mixture, generated from a fixed seed at any size.
//!
//! Each vector is `normalize(a·μ + b·c_k + Σ_r s_r·g_r·u_r + e·z/√d)`: a
//! direction μ shared by every vector (real embeddings are not centered, so
//! unrelated texts still have a positive cosine), the center `c_k` of its
//! cluster, a rank-32 component along fixed directions `u_r` with power-law
//! weights `s_r` (the anisotropy), and isotropic noise `z`. Queries are drawn
//! like documents but with more noise. With these weights two documents of
//! the same cluster have a cosine of about 0.6, and of different clusters
//! about 0.15.
//!
//! This is a secondary quality signal and the performance workload, not a
//! stand-in for a real corpus: whether v4 beats v3 on real embeddings is
//! for a real-data fixture to show (#177).
//!
//! Every draw comes from [`StandardNormalStream`], which is bit-for-bit
//! reproducible across x86_64 and aarch64, and every vector has its own
//! stream, so the documents of a smaller fixture are a prefix of a larger
//! one and generation parallelizes.

use ltsearch::index::StandardNormalStream;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

pub const DIM: usize = 512;

/// Names the generator's output. Bump it on any change to the vectors below;
/// the committed baseline pins each fixture's digest, so a change that isn't
/// bumped fails the gate rather than quietly moving the numbers.
pub const GENERATOR: &str = "agm-v1";

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
        docs,
        queries,
        digest,
    }
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
}
