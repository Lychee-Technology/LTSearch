//! Statistical correctness of the composed TurboQuant_prod estimator: its
//! bias, variance and distortion against Theorems 1 and 2 of the paper, and
//! how it compares with simpler estimators on the same pairs (#167).
//!
//! Recall@k can't confirm these claims: a biased estimator can rank well and
//! an unbiased one can rank poorly. This suite checks the math directly. It
//! averages in two separate ways:
//!
//! - **Over codec randomness**, the theorems' setting: fixed (x, y) pairs and
//!   many independent (Π, S) draws. Generating Π costs O(d³) per draw, so the
//!   default run uses d = m = 64 and the d = 512 run is `#[ignore]`d.
//!   `TurboQuantProdV1` only accepts the production shape (d = 512 with the
//!   committed codebook), so these tests compose the primitives it composes:
//!   `Rotation::generate(d, seed)`, `LloydMaxSolution::solve(d, 2)` and
//!   `QjlMatrix::generate(d, d, seed)`, with the codec's encode steps.
//! - **Over data, with production seeds**: the codec a v4 release ships,
//!   reached through `StaticReleaseFormat::from_name("v4")`, over random unit
//!   pairs at d = 512.
//!
//! The pairs are unit vectors whose inner products ⟨y, x⟩ are spread evenly
//! over (−1, 1), so a multiplicative bias shows up as a slope.
//!
//! Bias checks allow `Z_TOLERANCE` = 4 standard errors; distortion checks
//! against theory use the relative bands stated on each test. All vectors and
//! codec draws are seeded, so every run computes the same numbers and CI
//! can't flake on sampling noise. The observed values in the docs below come
//! from that fixed sample.
//!
//! The samples are computed once per test binary, spread over all cores. In
//! the `dev` profile CI runs, the default tests take about 12 s on 12 cores
//! and 18 s on 4; their budget is 2 minutes. The d = 512 run over codec
//! randomness takes 14 CPU-minutes in that profile, so it is `#[ignore]`d;
//! under `--release` it takes about 4 s on 12 cores:
//!
//! ```text
//! cargo test --release --test turbo_prod_statistics_test -- --ignored --nocapture
//! ```
//!
//! `--nocapture` also prints the measured values of every test, including
//! the table that compares TurboQuant_prod with MSE-only b = 2, MSE-only
//! b = 3 and the legacy codec.

use std::f64::consts::FRAC_PI_2;
use std::sync::OnceLock;

use ltsearch::index::{
    encode_vector, fill_standard_normal, CentroidTable, LloydMaxCodebook, LloydMaxSolution,
    PreparedTurboQuery, ProjectionMatrix, QjlMatrix, Rotation, StaticReleaseFormat,
    TurboQuantConfig, TurboQuantProdV1, TurboRecord512,
};

/// Bias checks allow this many standard errors.
const Z_TOLERANCE: f64 = 4.0;
/// Seed of every test vector; each set draws from its own stream.
const VECTOR_SEED: u64 = 0x167;

/// Theorem 1's bound on D_mse at b = 2, √3·π/2·4⁻².
const D_MSE_BOUND: f64 = 0.170;
/// Theorem 2's bound on d·D_prod/‖y‖² at b = 3, √3·π²·4⁻³.
const D_PROD_BOUND: f64 = 0.267;

// --- over codec randomness ---------------------------------------------------

const SEED_DIM: usize = 64;
const SEED_PAIRS: usize = 24;
const SEED_DRAWS: u64 = 400;

/// The d = 64 sample, shared by the tests that check it.
fn seed_sample() -> &'static SeedSample {
    static SAMPLE: OnceLock<SeedSample> = OnceLock::new();
    SAMPLE.get_or_init(|| SeedSample::draw(SEED_DIM, SEED_PAIRS, SEED_DRAWS))
}

/// Observed: the largest |bias| is 1.6 standard errors, with standard errors
/// near 2.5e-3.
#[test]
fn prod_is_unbiased_over_codec_randomness() {
    assert_prod_is_unbiased(seed_sample());
}

/// Observed: slope 0.8862 (standard error 7.4e-4) against 1 − D_mse = 0.8855.
#[test]
fn mse_only_is_shrunk_by_one_minus_d_mse_over_codec_randomness() {
    assert_mse_only_slope(seed_sample());
}

/// Observed: d·D_prod 0.1691 against a predicted 0.1738.
#[test]
fn prod_distortion_is_the_qjl_variance_over_codec_randomness() {
    assert_prod_distortion(seed_sample());
}

/// The three checks above at the production dimension, with 200 draws.
/// Observed: largest |z| 1.9; slope 0.8833 (standard error 4.1e-4) against
/// 0.8829; d·D_prod 0.1813 against a predicted 0.1789.
#[test]
#[ignore = "14 CPU-minutes in the dev profile; run under --release: \
            cargo test --release --test turbo_prod_statistics_test -- --ignored"]
fn over_codec_randomness_at_the_production_dimension() {
    let sample = SeedSample::draw(512, SEED_PAIRS, 200);
    assert_prod_is_unbiased(&sample);
    assert_mse_only_slope(&sample);
    assert_prod_distortion(&sample);
}

/// For every pair, the prod error ⟨y, x̃⟩ − ⟨y, x⟩ averages to zero within
/// `Z_TOLERANCE` standard errors over the draws. Given Π, the QJL term is an
/// unbiased estimate of ⟨y′, r′⟩ over S, so this holds exactly, not just in
/// the limit.
fn assert_prod_is_unbiased(sample: &SeedSample) {
    let mut worst_z: f64 = 0.0;
    for (pair, &truth) in sample.truths.iter().enumerate() {
        let errors: Vec<f64> = sample
            .draws
            .iter()
            .map(|draw| draw.prod[pair] - truth)
            .collect();
        let (bias, standard_error) = mean_and_standard_error(&errors);
        println!(
            "d = {}, ⟨y, x⟩ = {truth:+.3}: prod bias {bias:+.2e}, SE {standard_error:.2e}",
            sample.dim
        );
        assert!(
            bias.abs() <= Z_TOLERANCE * standard_error,
            "pair {pair} (⟨y, x⟩ = {truth}): bias {bias} is more than \
             {Z_TOLERANCE} standard errors ({standard_error})"
        );
        worst_z = worst_z.max(bias.abs() / standard_error);
    }
    println!(
        "d = {}: largest |z| over the pairs {worst_z:.2}",
        sample.dim
    );
}

/// MSE-only b = 2 is the first stage of prod scored alone: ⟨y′, c[idx]⟩.
/// The Lloyd-Max centroid condition E[X·Q(X)] = E[Q(X)²] makes its
/// expectation (1 − D_mse)·⟨y, x⟩ for unit x, so the slope through the origin
/// of the estimate against ⟨y, x⟩ is 1 − D_mse, with D_mse from #164's
/// solver at this d. The slope must be farther than `Z_TOLERANCE` standard
/// errors from 1, which shows the check can see a multiplicative bias, and
/// within `Z_TOLERANCE` standard errors and 1% of 1 − D_mse.
///
/// Each draw gives one slope fitted over all pairs; the pairs share the draw,
/// so the standard error comes from the spread of those per-draw slopes.
fn assert_mse_only_slope(sample: &SeedSample) {
    let sum_of_squares = dot(&sample.truths, &sample.truths);
    let slopes: Vec<f64> = sample
        .draws
        .iter()
        .map(|draw| dot(&sample.truths, &draw.mse) / sum_of_squares)
        .collect();
    let (slope, standard_error) = mean_and_standard_error(&slopes);
    let expected = 1.0 - LloydMaxSolution::solve(sample.dim as u32, 2).distortion();
    println!(
        "d = {}: MSE-only b = 2 slope {slope:.4} (SE {standard_error:.1e}), \
         1 − D_mse = {expected:.4}",
        sample.dim
    );

    assert!(
        (1.0 - slope).abs() > Z_TOLERANCE * standard_error,
        "slope {slope} isn't distinguishable from 1 (SE {standard_error})"
    );
    assert!(
        (slope - expected).abs() <= Z_TOLERANCE * standard_error,
        "slope {slope} is more than {Z_TOLERANCE} standard errors ({standard_error}) \
         from 1 − D_mse = {expected}"
    );
    assert!(
        (slope / expected - 1.0).abs() <= 0.01,
        "slope {slope} is more than 1% from 1 − D_mse = {expected}"
    );
}

/// Given Π, the prod error is the QJL error alone, whose variance over S is
/// (γ²/m)·((π/2)‖y′‖² − ⟨r̂, y′⟩²). Averaged over the pairs and draws, the
/// squared error must be within 10% of that variance and within Theorem 2's
/// bound.
fn assert_prod_distortion(sample: &SeedSample) {
    let mut squared_errors = Vec::new();
    let mut variances = Vec::new();
    for draw in &sample.draws {
        for (pair, truth) in sample.truths.iter().enumerate() {
            squared_errors.push((draw.prod[pair] - truth).powi(2));
            variances.push(draw.qjl_variance[pair]);
        }
    }
    let dim = sample.dim as f64;
    let distortion = mean(&squared_errors);
    let predicted = mean(&variances);
    println!(
        "d = {}: d·D_prod = {:.4}, predicted {:.4}, bound {D_PROD_BOUND}",
        sample.dim,
        dim * distortion,
        dim * predicted
    );

    assert!(
        dim * distortion <= D_PROD_BOUND,
        "d·D_prod = {} exceeds the bound",
        dim * distortion
    );
    assert!(
        (distortion / predicted - 1.0).abs() <= 0.10,
        "D_prod {distortion} is more than 10% from the QJL variance {predicted}"
    );
}

/// Each pair's estimates under independent codec draws, all at m = d.
struct SeedSample {
    dim: usize,
    truths: Vec<f64>,
    draws: Vec<Draw>,
}

/// One codec draw's estimates, indexed by pair.
struct Draw {
    prod: Vec<f64>,
    mse: Vec<f64>,
    qjl_variance: Vec<f64>,
}

impl SeedSample {
    fn draw(dim: usize, pairs: usize, draws: u64) -> Self {
        let pairs = unit_pairs(dim, pairs, 0);
        let codebook = LloydMaxSolution::solve(dim as u32, 2).to_codebook();
        let draws = map_on_all_cores(&(0..draws).collect::<Vec<_>>(), |&draw| {
            // Rotation and QJL draw from separate Gaussian streams, so
            // sharing a seed wouldn't correlate them; distinct seeds keep that
            // obvious.
            let rotation = Rotation::generate(dim as u32, 0x1670_0000 + draw);
            let qjl = QjlMatrix::generate(dim as u32, dim as u32, 0x1678_0000 + draw);
            let mut estimates = Draw {
                prod: Vec::new(),
                mse: Vec::new(),
                qjl_variance: Vec::new(),
            };
            for (x, y) in &pairs {
                let code = quantize(&codebook, rotate(&rotation, x));
                let rotated_query = rotate(&rotation, y);
                let mut signs = vec![0; qjl.signs_len()];
                qjl.encode(&code.residual, &mut signs).unwrap();
                let qjl_term = qjl
                    .prepare_query(&rotated_query)
                    .unwrap()
                    .estimate(&signs, code.gamma);

                let mse = dot32(&rotated_query, &code.reconstruction);
                estimates.prod.push(mse + f64::from(qjl_term));
                estimates.mse.push(mse);
                estimates
                    .qjl_variance
                    .push(qjl_variance(&code, &rotated_query, dim));
            }
            estimates
        });

        Self {
            dim,
            truths: pairs.iter().map(|(x, y)| dot32(x, y)).collect(),
            draws,
        }
    }
}

// --- over data, with production seeds ----------------------------------------

const DATA_PAIRS: usize = 2000;

/// The d = 512 sample, shared by the tests that check it.
fn data_sample() -> &'static [PairEstimates] {
    static SAMPLE: OnceLock<Vec<PairEstimates>> = OnceLock::new();
    SAMPLE.get_or_init(|| draw_data_sample(DATA_PAIRS))
}

/// The codec a v4 release ships, reached the way the release builder reaches
/// it: the format name, its config, and the assets that config generates.
fn production_codec() -> TurboQuantProdV1 {
    let format = StaticReleaseFormat::from_name("v4").unwrap();
    assert_eq!(format.codec_config(), TurboQuantConfig::prod_v1());
    TurboQuantProdV1::generate(format.codec_config()).unwrap()
}

/// The production rotation is a dense Haar draw, not a signed permutation or
/// another identity-like map, which would leave the codebook's assumption
/// about rotated coordinates false. A signed permutation has entries of ±1
/// and one nonzero entry per row; a Haar row at d = 512 has its largest entry
/// near √(2·ln d²/d) ≈ 0.22 and nearly all entries of order 1/√d. Observed:
/// largest |Π_ij| 0.197, and at least 448 entries per row with
/// |Π_ij| ≥ 0.1/√d.
#[test]
fn production_rotation_is_not_a_signed_permutation() {
    let codec = production_codec();
    let rotation = codec.rotation();
    let d = rotation.dim() as usize;
    let largest = rotation
        .values()
        .iter()
        .fold(0.0f32, |largest, value| largest.max(value.abs()));
    let floor = 0.1 / (d as f32).sqrt();
    let sparsest_row = rotation
        .values()
        .chunks_exact(d)
        .map(|row| row.iter().filter(|value| value.abs() >= floor).count())
        .min()
        .unwrap();
    println!("largest |Π_ij| {largest:.3}; fewest entries ≥ 0.1/√d in a row: {sparsest_row}");

    assert!(largest < 0.5, "largest |Π_ij| is {largest}");
    assert!(
        sparsest_row >= d / 2,
        "a row has only {sparsest_row} entries ≥ 0.1/√d"
    );
}

/// D_mse = E‖x − x̃_mse‖² = E[γ²] for unit x. The data are isotropic, so
/// Π·x is uniform on the sphere for the fixed production Π, and the mean of
/// γ² is #164's predicted D_mse in expectation. It must be within 1% of that
/// value, and within Theorem 1's bound. Observed: 0.11713 (standard error
/// 1.7e-4) against 0.11711.
#[test]
fn production_d_mse_matches_the_lloyd_max_prediction() {
    let gamma_squared = column(data_sample(), |pair| pair.gamma_squared);
    let (distortion, standard_error) = mean_and_standard_error(&gamma_squared);
    let predicted = LloydMaxSolution::solve(512, 2).distortion();
    println!(
        "d = 512, production seeds: D_mse {distortion:.5} (SE {standard_error:.1e}), \
         predicted {predicted:.5}, bound {D_MSE_BOUND}"
    );

    assert!(
        distortion <= D_MSE_BOUND,
        "D_mse {distortion} exceeds the bound"
    );
    assert!(
        (distortion / predicted - 1.0).abs() <= 0.01,
        "D_mse {distortion} is more than 1% from the prediction {predicted}"
    );
}

/// D_prod = E[(⟨y, x̃⟩ − ⟨y, x⟩)²] for unit y must be within Theorem 2's
/// bound, 0.267/d, and within 10% of the QJL variance averaged over the same
/// pairs, E[(γ²/m)·((π/2)‖y′‖² − ⟨r̂, y′⟩²)]. The prod estimate is unbiased,
/// so the two agree in expectation over S; with S fixed at the production
/// seed they agree up to that one draw's deviation, plus sampling noise of
/// about 3% at this sample size. Observed: d·D_prod 0.1788 against 0.1792.
#[test]
fn production_d_prod_is_the_averaged_qjl_variance_and_within_theorem_2() {
    let sample = data_sample();
    let squared_errors: Vec<f64> = sample
        .iter()
        .map(|pair| (pair.prod - pair.truth).powi(2))
        .collect();
    let (distortion, standard_error) = mean_and_standard_error(&squared_errors);
    let predicted = mean(&column(sample, |pair| pair.qjl_variance));
    println!(
        "d = 512, production seeds: d·D_prod {:.4} (SE {:.4}), averaged QJL variance \
         {:.4}, bound {D_PROD_BOUND}; RMSE {:.4}",
        512.0 * distortion,
        512.0 * standard_error,
        512.0 * predicted,
        distortion.sqrt()
    );

    assert!(
        512.0 * distortion <= D_PROD_BOUND,
        "d·D_prod = {} exceeds the bound",
        512.0 * distortion
    );
    assert!(
        (distortion / predicted - 1.0).abs() <= 0.10,
        "D_prod {distortion} is more than 10% from the averaged QJL variance {predicted}"
    );
}

/// Prints the bias, slope and RMSE of each estimator on the same pairs:
/// TurboQuant_prod, MSE-only b = 2 (prod's first stage alone), MSE-only
/// b = 3 (prod's bit budget, all spent on the codebook) and the legacy codec,
/// which is reported only. "Scaled RMSE" is the RMS error about the fitted
/// slope, the noise left once a constant scale, which can't change a ranking,
/// is removed.
///
/// For any fixed Π and isotropic data, E[⟨y′, Q(x′)⟩] = (1 − D_mse)·⟨y, x⟩
/// exactly, as over codec randomness, so both MSE-only slopes must be within
/// `Z_TOLERANCE` standard errors of their 1 − D_mse. Prod's slope is reported
/// but not checked: with S fixed, its expectation over the data is 1 only up
/// to that one draw's deviation.
#[test]
fn prod_against_simpler_estimators_on_the_same_pairs() {
    let sample = data_sample();
    let truths = column(sample, |pair| pair.truth);
    println!(
        "d = 512, production seeds, {} unit pairs with ⟨y, x⟩ spread over (−1, 1):",
        sample.len()
    );
    println!("estimator            bias  bias SE    slope  slope SE     RMSE  scaled RMSE");
    for (name, estimates) in [
        ("prod (2 + 1 bits)", column(sample, |pair| pair.prod)),
        ("MSE-only b = 2", column(sample, |pair| pair.mse2)),
        ("MSE-only b = 3", column(sample, |pair| pair.mse3)),
        ("legacy", column(sample, |pair| pair.legacy)),
    ] {
        let fit = Fit::new(&truths, &estimates);
        println!(
            "{name:<17} {:+.1e}  {:.1e}  {:7.4}   {:.1e}  {:7.4}  {:11.4}",
            fit.bias,
            fit.bias_standard_error,
            fit.slope,
            fit.slope_standard_error,
            fit.rmse,
            fit.scaled_rmse
        );
    }

    for bits in [2, 3] {
        let estimates = column(sample, |pair| if bits == 2 { pair.mse2 } else { pair.mse3 });
        let fit = Fit::new(&truths, &estimates);
        let expected = 1.0 - LloydMaxSolution::solve(512, bits).distortion();
        println!("MSE-only b = {bits}: 1 − D_mse = {expected:.4}");
        assert!(
            (fit.slope - expected).abs() <= Z_TOLERANCE * fit.slope_standard_error,
            "MSE-only b = {bits}: slope {} is more than {Z_TOLERANCE} standard errors ({}) \
             from 1 − D_mse = {expected}",
            fit.slope,
            fit.slope_standard_error
        );
    }
}

/// One unit pair's ⟨y, x⟩ and each estimator's estimate of it, plus what the
/// distortion checks need.
struct PairEstimates {
    truth: f64,
    prod: f64,
    mse2: f64,
    mse3: f64,
    legacy: f64,
    gamma_squared: f64,
    qjl_variance: f64,
}

fn draw_data_sample(pairs: usize) -> Vec<PairEstimates> {
    let codec = production_codec();
    let config = codec.config();
    let codebook_3 = LloydMaxSolution::solve(config.dim, 3).to_codebook();
    // The legacy codec's production assets, as its release builders
    // generate them.
    let legacy = TurboQuantConfig::legacy_v1();
    let centroids =
        CentroidTable::generate(legacy.dim, legacy.centroids_per_dim(), legacy.mse_seed);
    let projection = ProjectionMatrix::generate(legacy.dim, legacy.qjl_dim, legacy.qjl_seed);

    map_on_all_cores(&unit_pairs(config.dim as usize, pairs, 1), |(x, y)| {
        let encoded = codec.encode(x).unwrap();
        let terms = codec
            .prepare_query(y)
            .unwrap()
            .score_breakdown(encoded.code());

        // The codec's encode steps, repeated here, give the residual it
        // sketched, which the QJL variance needs.
        let rotated = rotate(codec.rotation(), &codec_unit(x));
        let code = quantize(codec.codebook(), rotated.clone());
        assert_eq!(code.gamma, encoded.gamma);
        let rotated_query = rotate(codec.rotation(), y);
        let code_3 = quantize(&codebook_3, rotated);

        let legacy_code = encode_vector(x, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: 0,
            idx: legacy_code.idx.try_into().unwrap(),
            qjl: legacy_code.qjl.try_into().unwrap(),
            gamma: legacy_code.gamma,
            _reserved: [0; 4],
        };
        let legacy_query = PreparedTurboQuery::prepare(y, &centroids, &projection).unwrap();

        PairEstimates {
            truth: dot32(x, y),
            prod: f64::from(terms.total()),
            mse2: f64::from(terms.norm * terms.mse_term),
            mse3: f64::from(encoded.norm) * dot32(&rotated_query, &code_3.reconstruction),
            legacy: f64::from(legacy_query.score(&record)),
            gamma_squared: f64::from(encoded.gamma).powi(2),
            qjl_variance: qjl_variance(&code, &rotated_query, config.qjl_dim as usize),
        }
    })
}

fn column(sample: &[PairEstimates], field: impl Fn(&PairEstimates) -> f64) -> Vec<f64> {
    sample.iter().map(field).collect()
}

/// The bias, the slope through the origin, and the RMSE of estimates against
/// truths, with the standard errors of the first two.
struct Fit {
    bias: f64,
    bias_standard_error: f64,
    slope: f64,
    slope_standard_error: f64,
    rmse: f64,
    scaled_rmse: f64,
}

impl Fit {
    fn new(truths: &[f64], estimates: &[f64]) -> Self {
        let errors: Vec<f64> = estimates
            .iter()
            .zip(truths)
            .map(|(estimate, truth)| estimate - truth)
            .collect();
        let (bias, bias_standard_error) = mean_and_standard_error(&errors);
        let sum_of_squares = dot(truths, truths);
        let slope = dot(truths, estimates) / sum_of_squares;
        let scaled_errors: Vec<f64> = estimates
            .iter()
            .zip(truths)
            .map(|(estimate, truth)| estimate - slope * truth)
            .collect();
        let residual_variance = dot(&scaled_errors, &scaled_errors) / (truths.len() - 1) as f64;
        Self {
            bias,
            bias_standard_error,
            slope,
            slope_standard_error: (residual_variance / sum_of_squares).sqrt(),
            rmse: (dot(&errors, &errors) / errors.len() as f64).sqrt(),
            scaled_rmse: (dot(&scaled_errors, &scaled_errors) / errors.len() as f64).sqrt(),
        }
    }
}

// --- the codec's steps -------------------------------------------------------

/// The quantized rotated vector x′ = Π·u: its centroids c[idx], the residual
/// r′ = x′ − c[idx] and γ = ‖r′‖, in the codec's arithmetic.
struct MseCode {
    reconstruction: Vec<f32>,
    residual: Vec<f32>,
    gamma: f32,
}

fn quantize(codebook: &LloydMaxCodebook, rotated: Vec<f32>) -> MseCode {
    let mut residual = rotated;
    let reconstruction: Vec<f32> = residual
        .iter()
        .map(|&value| codebook.decode(codebook.encode(value)))
        .collect();
    for (value, centroid) in residual.iter_mut().zip(&reconstruction) {
        *value -= centroid;
    }
    let gamma = residual
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum::<f64>()
        .sqrt() as f32;
    MseCode {
        reconstruction,
        residual,
        gamma,
    }
}

/// The variance over S of the QJL estimate of ⟨r′, y′⟩ with m rows,
/// (γ²/m)·((π/2)‖y′‖² − ⟨r̂, y′⟩²), r̂ = r′/γ.
fn qjl_variance(code: &MseCode, rotated_query: &[f32], qjl_dim: usize) -> f64 {
    let gamma = f64::from(code.gamma);
    let along = dot32(&code.residual, rotated_query) / gamma;
    gamma * gamma / qjl_dim as f64
        * (FRAC_PI_2 * dot32(rotated_query, rotated_query) - along * along)
}

fn rotate(rotation: &Rotation, x: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; x.len()];
    rotation.rotate(x, &mut out).unwrap();
    out
}

/// x/‖x‖ as the codec computes it: in f64, rounded to f32.
fn codec_unit(x: &[f32]) -> Vec<f32> {
    let norm = dot32(x, x).sqrt();
    x.iter()
        .map(|&value| (f64::from(value) / norm) as f32)
        .collect()
}

// --- helpers -----------------------------------------------------------------

/// `count` unit pairs (x, y) in ℝ^dim with ⟨y, x⟩ = ρ_n spread evenly over
/// (−1, 1): x is uniform on the sphere and y = ρ_n·x + √(1 − ρ_n²)·z, with
/// z uniform on the unit sphere orthogonal to x. Both are normalized in f64
/// and rounded to f32.
fn unit_pairs(dim: usize, count: usize, stream_id: u64) -> Vec<(Vec<f32>, Vec<f32>)> {
    let mut draws = vec![0.0; 2 * count * dim];
    fill_standard_normal(VECTOR_SEED, stream_id, &mut draws);
    draws
        .chunks_exact(2 * dim)
        .enumerate()
        .map(|(n, chunk)| {
            let (x, z) = chunk.split_at(dim);
            let x = normalized(x);
            let along = dot(&x, z);
            let z: Vec<f64> = z.iter().zip(&x).map(|(z, x)| z - along * x).collect();
            let z = normalized(&z);
            let rho = -1.0 + (2 * n + 1) as f64 / count as f64;
            let y: Vec<f64> = x
                .iter()
                .zip(&z)
                .map(|(x, z)| rho * x + (1.0 - rho * rho).sqrt() * z)
                .collect();
            (to_f32(&x), to_f32(&normalized(&y)))
        })
        .collect()
}

/// `items.iter().map(f).collect()`, spread over all cores. Each result
/// depends only on its item, so the output doesn't depend on the core count.
fn map_on_all_cores<T: Sync, U: Send>(items: &[T], f: impl Fn(&T) -> U + Sync) -> Vec<U> {
    let threads = std::thread::available_parallelism().map_or(1, |threads| threads.get());
    let chunk_len = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(chunk_len)
            .map(|chunk| scope.spawn(|| chunk.iter().map(&f).collect::<Vec<_>>()))
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect()
    })
}

fn normalized(x: &[f64]) -> Vec<f64> {
    let norm = dot(x, x).sqrt();
    x.iter().map(|value| value / norm).collect()
}

fn to_f32(x: &[f64]) -> Vec<f32> {
    x.iter().map(|&value| value as f32).collect()
}

fn dot(x: &[f64], y: &[f64]) -> f64 {
    x.iter().zip(y).map(|(x, y)| x * y).sum()
}

/// ⟨x, y⟩ of f32 vectors, in f64.
fn dot32(x: &[f32], y: &[f32]) -> f64 {
    x.iter()
        .zip(y)
        .map(|(&x, &y)| f64::from(x) * f64::from(y))
        .sum()
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// The sample mean and its standard error.
fn mean_and_standard_error(values: &[f64]) -> (f64, f64) {
    let mean = mean(values);
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    (mean, (variance / values.len() as f64).sqrt())
}
