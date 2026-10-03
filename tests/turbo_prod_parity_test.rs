//! Parity between `TurboQuantProdV1` and an unoptimized f64 reference.
//!
//! The reference recomputes each step of the estimator in f64 from the
//! codec's public matrices: `‖x‖`, `Π·u`, the nearest centroid, the residual,
//! `S·r′`, and the score `‖x‖·(⟨y′, c[idx]⟩ + γ·√(π/2)/m·⟨z, S·y′⟩)`. The codec
//! works in f32, so a coordinate within rounding of a codebook threshold, or a
//! projection within rounding of 0, may legitimately go either way. The test
//! allows that only inside a stated margin, and scores the reference with the
//! codec's index and sign bits so one such flip doesn't fail the score check.
//!
//! It runs at m = 256, 512 and 1024 against one d = 512 rotation, so a scale
//! or a loop bound that uses d where it means m fails here.

use std::f64::consts::FRAC_PI_2;

use ltsearch::index::{
    fill_standard_normal, EncodedTurboProd, LloydMaxCodebook, NormPolicy, QjlMatrix, Rotation,
    TurboCodecId, TurboQuantConfig, TurboQuantProdV1, GAUSSIAN_GENERATOR_VERSION,
};

const DIM: usize = 512;
const QJL_DIMS: [u32; 3] = [256, 512, 1024];
const DOC_COUNT: usize = 24;
const QUERY_COUNT: usize = 4;
/// A coordinate this close to a threshold may round to either cell.
const THRESHOLD_MARGIN: f64 = 1e-5;
/// A projection `S_j·r′` this close to 0 may get either sign.
const SIGN_MARGIN: f64 = 1e-4;
/// Score errors are relative to ‖y‖ (per term) or ‖x‖·‖y‖ (total), the
/// bound on |⟨y, x⟩|. An m/d mix-up in the scale is off by about 1e-2.
const SCORE_TOLERANCE: f64 = 1e-5;

fn config(qjl_dim: u32) -> TurboQuantConfig {
    TurboQuantConfig {
        codec_id: TurboCodecId::TurboQuantProdV1,
        dim: DIM as u32,
        mse_bits: 2,
        qjl_dim,
        mse_seed: 163,
        qjl_seed: 165,
        generator_version: GAUSSIAN_GENERATOR_VERSION,
        norm_policy: NormPolicy::NormalizeAndStore,
    }
}

/// Gaussian vectors with norms spread over four orders of magnitude.
fn vectors(count: usize, stream_id: u64) -> Vec<Vec<f32>> {
    let mut draws = vec![0.0; count * DIM];
    fill_standard_normal(0x165, stream_id, &mut draws);
    draws
        .chunks_exact(DIM)
        .enumerate()
        .map(|(n, chunk)| {
            let scale = 10f64.powf(n as f64 % 5.0 - 2.0);
            chunk.iter().map(|&value| (value * scale) as f32).collect()
        })
        .collect()
}

fn mat_vec(matrix: &[f32], x: &[f64]) -> Vec<f64> {
    matrix
        .chunks_exact(x.len())
        .map(|row| row.iter().zip(x).map(|(&a, b)| f64::from(a) * b).sum())
        .collect()
}

fn norm(x: &[f64]) -> f64 {
    x.iter().map(|value| value * value).sum::<f64>().sqrt()
}

fn read_idx(idx: &[u8], i: usize) -> usize {
    usize::from((idx[i / 4] >> (2 * (i % 4))) & 0b11)
}

fn sign(signs: &[u8], j: usize) -> f64 {
    if (signs[j / 8] >> (j % 8)) & 1 == 1 {
        1.0
    } else {
        -1.0
    }
}

/// What the reference derives from x, given the codec's index bits.
struct Reference {
    norm: f64,
    gamma: f64,
    /// ‖u − Πᵀ·c[idx]‖, the residual in the original space.
    original_space_gamma: f64,
    ambiguous_indices: usize,
    ambiguous_signs: usize,
}

fn reference_encode(codec: &TurboQuantProdV1, x: &[f32], encoded: &EncodedTurboProd) -> Reference {
    let centroids: Vec<f64> = codec
        .codebook()
        .centroids()
        .iter()
        .map(|&c| c.into())
        .collect();
    let x: Vec<f64> = x.iter().map(|&value| value.into()).collect();
    let x_norm = norm(&x);
    let u: Vec<f64> = x.iter().map(|value| value / x_norm).collect();
    let rotated = mat_vec(codec.rotation().values(), &u);

    let mut ambiguous_indices = 0;
    let mut residual = Vec::with_capacity(DIM);
    for (i, &value) in rotated.iter().enumerate() {
        let nearest = (0..centroids.len())
            .min_by(|&a, &b| {
                (value - centroids[a])
                    .abs()
                    .total_cmp(&(value - centroids[b]).abs())
            })
            .unwrap();
        let index = read_idx(&encoded.idx, i);
        if index != nearest {
            let midpoint = (centroids[index] + centroids[nearest]) / 2.0;
            assert!(
                index.abs_diff(nearest) == 1 && (value - midpoint).abs() <= THRESHOLD_MARGIN,
                "coordinate {i} = {value} encoded to cell {index}, nearest is {nearest}"
            );
            ambiguous_indices += 1;
        }
        residual.push(value - centroids[index]);
    }

    let mut ambiguous_signs = 0;
    for (j, projection) in mat_vec(codec.qjl().values(), &residual).iter().enumerate() {
        if (*projection >= 0.0) != (sign(&encoded.signs, j) > 0.0) {
            assert!(
                projection.abs() <= SIGN_MARGIN,
                "sign {j}: S_j·r′ = {projection}, codec bit disagrees"
            );
            ambiguous_signs += 1;
        }
    }

    // u − Πᵀ·c[idx].
    let pi = codec.rotation().values();
    let decoded: Vec<f64> = (0..DIM)
        .map(|i| centroids[read_idx(&encoded.idx, i)])
        .collect();
    let original_space_residual: Vec<f64> = (0..DIM)
        .map(|j| {
            u[j] - (0..DIM)
                .map(|i| f64::from(pi[i * DIM + j]) * decoded[i])
                .sum::<f64>()
        })
        .collect();

    Reference {
        norm: x_norm,
        gamma: norm(&residual),
        original_space_gamma: norm(&original_space_residual),
        ambiguous_indices,
        ambiguous_signs,
    }
}

/// A query's f64 reference preparation: ‖y‖, `y′ = Π·y` and `S·y′`.
struct ReferenceQuery {
    norm: f64,
    rotated: Vec<f64>,
    projected: Vec<f64>,
}

impl ReferenceQuery {
    fn prepare(codec: &TurboQuantProdV1, y: &[f32]) -> Self {
        let y: Vec<f64> = y.iter().map(|&value| value.into()).collect();
        let rotated = mat_vec(codec.rotation().values(), &y);
        Self {
            norm: norm(&y),
            projected: mat_vec(codec.qjl().values(), &rotated),
            rotated,
        }
    }

    /// `(mse_term, qjl_term)` for the unit vector, from the codec's bits and
    /// the reference γ.
    fn terms(
        &self,
        codec: &TurboQuantProdV1,
        encoded: &EncodedTurboProd,
        gamma: f64,
    ) -> (f64, f64) {
        let centroids = codec.codebook().centroids();
        let m = f64::from(codec.config().qjl_dim);
        let mse_term = self
            .rotated
            .iter()
            .enumerate()
            .map(|(i, value)| value * f64::from(centroids[read_idx(&encoded.idx, i)]))
            .sum();
        let signed_sum: f64 = self
            .projected
            .iter()
            .enumerate()
            .map(|(j, value)| sign(&encoded.signs, j) * value)
            .sum();
        (mse_term, gamma * FRAC_PI_2.sqrt() / m * signed_sum)
    }
}

#[test]
fn encode_and_score_match_the_f64_reference_for_m_below_equal_and_above_d() {
    let rotation = Rotation::generate(DIM as u32, 163);
    let codebook = LloydMaxCodebook::committed(DIM as u32, 2).unwrap();
    let docs = vectors(DOC_COUNT, 0);
    let queries = vectors(QUERY_COUNT, 1);

    for qjl_dim in QJL_DIMS {
        let codec = TurboQuantProdV1::from_assets(
            config(qjl_dim),
            rotation.clone(),
            codebook.clone(),
            QjlMatrix::generate(DIM as u32, qjl_dim, 165),
        )
        .unwrap();
        let prepared = queries
            .iter()
            .map(|query| {
                (
                    codec.prepare_query(query).unwrap(),
                    ReferenceQuery::prepare(&codec, query),
                )
            })
            .collect::<Vec<_>>();

        let (mut gamma_error, mut rotation_gap, mut term_error, mut total_error) =
            (0f64, 0f64, 0f64, 0f64);
        let (mut ambiguous_indices, mut ambiguous_signs) = (0, 0);
        for doc in &docs {
            let encoded = codec.encode(doc).unwrap();
            assert_eq!(encoded.idx.len(), DIM / 4);
            assert_eq!(encoded.signs.len(), qjl_dim.div_ceil(8) as usize);

            let reference = reference_encode(&codec, doc, &encoded);
            assert!(
                (f64::from(encoded.norm) - reference.norm).abs() <= 1e-7 * reference.norm,
                "norm {} vs {}",
                encoded.norm,
                reference.norm
            );
            gamma_error = gamma_error.max((f64::from(encoded.gamma) - reference.gamma).abs());
            rotation_gap =
                rotation_gap.max((reference.original_space_gamma - reference.gamma).abs());
            ambiguous_indices += reference.ambiguous_indices;
            ambiguous_signs += reference.ambiguous_signs;

            for (prepared, query) in &prepared {
                let (mse_term, qjl_term) = query.terms(&codec, &encoded, reference.gamma);
                let breakdown = prepared.score_breakdown(encoded.code());
                term_error = term_error
                    .max((f64::from(breakdown.mse_term) - mse_term).abs() / query.norm)
                    .max((f64::from(breakdown.qjl_term) - qjl_term).abs() / query.norm);
                let total = reference.norm * (mse_term + qjl_term);
                total_error = total_error.max(
                    (f64::from(prepared.score(encoded.code())) - total).abs()
                        / (reference.norm * query.norm),
                );
            }
        }

        println!(
            "turbo_prod parity m={qjl_dim} docs={DOC_COUNT} queries={QUERY_COUNT} \
             max_gamma_error={gamma_error:.2e} max_original_vs_rotated_gamma={rotation_gap:.2e} \
             max_term_error={term_error:.2e} max_total_error={total_error:.2e} \
             ambiguous_indices={ambiguous_indices} ambiguous_signs={ambiguous_signs}"
        );
        assert!(gamma_error <= 1e-5, "m={qjl_dim}: γ off by {gamma_error}");
        // Π is orthogonal up to its f32 storage, so the residual has the same
        // norm in both spaces (module docs of `turbo_prod`, "Residual space").
        assert!(
            rotation_gap <= 1e-5,
            "m={qjl_dim}: ‖r‖ − ‖r′‖ = {rotation_gap}"
        );
        assert!(
            term_error <= SCORE_TOLERANCE,
            "m={qjl_dim}: a score term is off by {term_error} of ‖y‖"
        );
        assert!(
            total_error <= SCORE_TOLERANCE,
            "m={qjl_dim}: the score is off by {total_error} of ‖x‖·‖y‖"
        );
    }
}
