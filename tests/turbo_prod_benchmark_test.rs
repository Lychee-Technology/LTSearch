use std::hint::black_box;
use std::time::Instant;

use ltsearch::index::{
    encode_vector, CentroidTable, NormPolicy, PreparedTurboQuery, ProjectionMatrix, TurboCodecId,
    TurboQuantConfig, TurboQuantProdV1, TurboRecord512, GAUSSIAN_GENERATOR_VERSION,
};

const DIM: usize = 512;
// Odd, so each p50 is a single measured sample rather than an interpolation.
const BATCHES: usize = 25;
const PREPARE_PER_BATCH: usize = 20;
const ENCODE_PER_BATCH: usize = 100;
const RECORD_COUNT: usize = 10_000;

fn vectors(count: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|vector| {
            (0..DIM)
                .map(|dim| (((vector * 31 + dim) % 17) as f32 - 8.0) / 64.0)
                .collect()
        })
        .collect()
}

/// One warm-up batch, then `BATCHES` measured ones of `per_batch` calls to
/// `run`; returns the sorted per-call nanoseconds.
fn per_call_ns(per_batch: usize, mut run: impl FnMut()) -> Vec<f64> {
    let mut latencies = (0..=BATCHES)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..per_batch {
                run();
            }
            start.elapsed().as_secs_f64() * 1e9 / per_batch as f64
        })
        .skip(1)
        .collect::<Vec<_>>();
    latencies.sort_by(f64::total_cmp);
    latencies
}

/// Compares `TurboQuantProdV1` with the legacy codec at d = m = 512. Run with
/// `cargo test --release --test turbo_prod_benchmark_test -- --ignored --nocapture`
/// to measure the shipping profile; the dev profile is only useful as a smoke test.
#[test]
#[ignore = "benchmark-style smoke test"]
fn turbo_prod_benchmark_reports_encode_prepare_and_score_latency() {
    let start = Instant::now();
    let codec = TurboQuantProdV1::generate(TurboQuantConfig {
        codec_id: TurboCodecId::TurboQuantProdV1,
        mse_seed: 163,
        qjl_seed: 165,
        generator_version: GAUSSIAN_GENERATOR_VERSION,
        norm_policy: NormPolicy::NormalizeAndStore,
        ..TurboQuantConfig::legacy_v1()
    })
    .unwrap();
    let generate_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let centroids = CentroidTable::generate(DIM as u32, 4, 7);
    let projection = ProjectionMatrix::generate(DIM as u32, DIM as u32, 11);

    let docs = vectors(RECORD_COUNT);
    let query = &vectors(1)[0];
    let prod_codes = docs
        .iter()
        .map(|doc| codec.encode(doc).unwrap())
        .collect::<Vec<_>>();
    let legacy_records = docs
        .iter()
        .enumerate()
        .map(|(doc_id, doc)| {
            let encoded = encode_vector(doc, &centroids, &projection).unwrap();
            TurboRecord512 {
                doc_id: doc_id as u64,
                idx: encoded.idx.try_into().unwrap(),
                qjl: encoded.qjl.try_into().unwrap(),
                gamma: encoded.gamma,
                _reserved: [0; 4],
            }
        })
        .collect::<Vec<_>>();

    let mut docs_cycle = docs.iter().cycle();
    let prod_encode = per_call_ns(ENCODE_PER_BATCH, || {
        black_box(codec.encode(black_box(docs_cycle.next().unwrap())).unwrap());
    });
    let legacy_encode = per_call_ns(ENCODE_PER_BATCH, || {
        let doc = docs_cycle.next().unwrap();
        black_box(encode_vector(black_box(doc), &centroids, &projection).unwrap());
    });
    let prod_prepare = per_call_ns(PREPARE_PER_BATCH, || {
        black_box(codec.prepare_query(black_box(query)).unwrap());
    });
    let legacy_prepare = per_call_ns(PREPARE_PER_BATCH, || {
        black_box(PreparedTurboQuery::prepare(black_box(query), &centroids, &projection).unwrap());
    });

    let prepared = codec.prepare_query(query).unwrap();
    let prod_score = per_call_ns(1, || {
        for code in &prod_codes {
            black_box(prepared.score(black_box(code.code())));
        }
    });
    let legacy_prepared = PreparedTurboQuery::prepare(query, &centroids, &projection).unwrap();
    let legacy_score = per_call_ns(1, || {
        for record in &legacy_records {
            black_box(legacy_prepared.score(black_box(record)));
        }
    });
    let per_record = |sorted: &[f64]| sorted[BATCHES / 2] / RECORD_COUNT as f64;

    println!(
        "turbo_prod benchmark dim={DIM} qjl_dim={DIM} debug_assertions={} generate_ms={generate_ms:.0} \
         batches={BATCHES} encode_p50_ns={:.0} legacy_encode_p50_ns={:.0} \
         prepare_p50_ns={:.0} legacy_prepare_p50_ns={:.0} \
         score_records={RECORD_COUNT} score_p50_ns_per_record={:.1} legacy_score_p50_ns_per_record={:.1}",
        cfg!(debug_assertions),
        prod_encode[BATCHES / 2],
        legacy_encode[BATCHES / 2],
        prod_prepare[BATCHES / 2],
        legacy_prepare[BATCHES / 2],
        per_record(&prod_score),
        per_record(&legacy_score),
    );
}
