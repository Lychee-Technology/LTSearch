use std::hint::black_box;
use std::time::Instant;

use ltsearch::index::{AssetError, Rotation};

const DIM: usize = 512;
// Both odd, so each p50 is a single measured sample rather than an
// interpolation.
const GENERATE_RUNS: usize = 5;
const BATCHES: usize = 25;
const VECTORS_PER_BATCH: usize = 1_000;

/// `Rotation::rotate` or `Rotation::inverse_rotate`.
type Apply = fn(&Rotation, &[f32], &mut [f32]) -> Result<(), AssetError>;

/// Run with `cargo test --release --test turbo_rotation_benchmark_test -- --ignored --nocapture`
/// to measure the shipping profile; the dev profile is only useful as a smoke test.
#[test]
#[ignore = "benchmark-style smoke test"]
fn turbo_rotation_benchmark_reports_generate_and_rotate_latency() {
    let mut generate_ms = (0..GENERATE_RUNS)
        .map(|run| {
            let start = Instant::now();
            black_box(Rotation::generate(DIM as u32, run as u64));
            start.elapsed().as_secs_f64() * 1_000.0
        })
        .collect::<Vec<_>>();
    generate_ms.sort_by(f64::total_cmp);

    let rotation = Rotation::generate(DIM as u32, 163);
    let vectors = (0..VECTORS_PER_BATCH)
        .map(|vector| {
            (0..DIM)
                .map(|dim| (((vector * 31 + dim) % 17) as f32 - 8.0) / 64.0)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut out = vec![0.0; DIM];
    let mut per_vector_ns = |apply: Apply| {
        // One warm-up batch, then BATCHES measured ones.
        let mut latencies = (0..=BATCHES)
            .map(|_| {
                let start = Instant::now();
                for vector in &vectors {
                    apply(&rotation, black_box(vector), &mut out).unwrap();
                    black_box(&out);
                }
                start.elapsed().as_secs_f64() * 1e9 / VECTORS_PER_BATCH as f64
            })
            .skip(1)
            .collect::<Vec<_>>();
        latencies.sort_by(f64::total_cmp);
        latencies
    };
    let rotate_ns = per_vector_ns(Rotation::rotate);
    let inverse_ns = per_vector_ns(Rotation::inverse_rotate);

    println!(
        "turbo_rotation benchmark dim={DIM} debug_assertions={} \
         generate_runs={GENERATE_RUNS} generate_p50_ms={:.1} generate_min_ms={:.1} generate_max_ms={:.1} \
         batches={BATCHES}x{VECTORS_PER_BATCH} rotate_p50_ns={:.0} rotate_min_ns={:.0} \
         inverse_rotate_p50_ns={:.0} inverse_rotate_min_ns={:.0}",
        cfg!(debug_assertions),
        generate_ms[GENERATE_RUNS / 2],
        generate_ms[0],
        generate_ms[GENERATE_RUNS - 1],
        rotate_ns[BATCHES / 2],
        rotate_ns[0],
        inverse_ns[BATCHES / 2],
        inverse_ns[0],
    );
}
