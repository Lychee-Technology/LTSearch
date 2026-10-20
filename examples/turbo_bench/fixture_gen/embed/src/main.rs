//! Embeds a turbo_bench real-embedding fixture with LTEmbed (#177).
//!
//! Reads the `text` field of every line of the fixture's `docs.jsonl` and
//! `queries.jsonl`, embeds the first as documents and the second as queries,
//! and writes `docs.f32` and `queries.f32`: 512-d vectors as little-endian
//! f32, one after another, in line order.
//!
//! The engine is configured the way `LTEmbedEmbeddingGenerator` configures it
//! in production: `EngineConfig::default()` (Matryoshka truncation to 512
//! dims, then L2 normalization) and one llama.cpp thread. Throughput comes
//! from several engines, one per worker thread.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use ltembed::engine::{
    EmbeddingEngine, EmbeddingInput, EmbeddingInputKind, EngineConfig, EMBEDDING_DIMENSION,
};

const USAGE: &str = "usage: turbo-bench-embed <bundle_dir> <fixture_dir> [workers]";
const NORM_TOLERANCE: f64 = 1e-3;

fn main() {
    if let Err(error) = run() {
        eprintln!("turbo-bench-embed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (bundle, fixture, workers) = match args.as_slice() {
        [bundle, fixture] => (bundle, fixture, default_workers()),
        [bundle, fixture, workers] => (
            bundle,
            fixture,
            workers
                .parse()
                .ok()
                .filter(|&workers| workers > 0)
                .ok_or("workers must be a positive integer")?,
        ),
        _ => return Err(USAGE.into()),
    };
    let fixture = Path::new(fixture);

    let docs = read_texts(&fixture.join("docs.jsonl"))?;
    let queries = read_texts(&fixture.join("queries.jsonl"))?;
    let inputs: Vec<EmbeddingInput> = docs
        .iter()
        .map(|text| EmbeddingInput::document(text))
        .chain(queries.iter().map(|text| EmbeddingInput::query(text)))
        .collect();
    let vectors = embed_all(Path::new(bundle), &inputs, workers)?;
    let (doc_vectors, query_vectors) = vectors.split_at(docs.len());
    write_f32(&fixture.join("docs.f32"), doc_vectors)?;
    write_f32(&fixture.join("queries.f32"), query_vectors)?;
    eprintln!(
        "turbo-bench-embed: wrote {} documents and {} queries",
        docs.len(),
        queries.len()
    );
    Ok(())
}

fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

fn read_texts(path: &Path) -> Result<Vec<String>, String> {
    let content =
        fs::read_to_string(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|error| format!("{}:{}: {error}", path.display(), i + 1))?;
            value["text"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{}:{}: no non-empty `text`", path.display(), i + 1))
        })
        .collect()
}

/// Every input's embedding, in input order.
fn embed_all(
    bundle: &Path,
    inputs: &[EmbeddingInput],
    workers: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Vec<f32>>>> = Mutex::new(vec![None; inputs.len()]);
    let started = Instant::now();
    eprintln!(
        "turbo-bench-embed: {} texts on {} workers",
        inputs.len(),
        workers.min(inputs.len())
    );

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.min(inputs.len()))
            .map(|_| {
                scope.spawn(|| -> Result<(), String> {
                    let engine =
                        EmbeddingEngine::from_gguf_bundle_dir(bundle, EngineConfig::default())
                            .map_err(|error| format!("loading {}: {error}", bundle.display()))?;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(input) = inputs.get(i) else {
                            return Ok(());
                        };
                        let vector = engine
                            .embed(*input)
                            .map_err(|error| format!("text {i}: {error}"))?;
                        check_vector(i, input.kind, &vector)?;
                        results.lock().expect("no worker panics")[i] = Some(vector);
                        let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if finished % 50 == 0 || finished == inputs.len() {
                            let elapsed = started.elapsed().as_secs_f64();
                            let remaining =
                                elapsed / finished as f64 * (inputs.len() - finished) as f64;
                            eprintln!(
                                "turbo-bench-embed: {finished}/{} in {elapsed:.0} s, \
                                 about {remaining:.0} s left",
                                inputs.len()
                            );
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no worker panics"))
            .collect::<Result<(), String>>()
    })?;

    results
        .into_inner()
        .expect("no worker panics")
        .into_iter()
        .map(|vector| vector.ok_or_else(|| "a text was never embedded".to_string()))
        .collect()
}

/// The harness assumes 512-d unit vectors; anything else is a broken setup.
fn check_vector(i: usize, kind: EmbeddingInputKind, vector: &[f32]) -> Result<(), String> {
    if vector.len() != EMBEDDING_DIMENSION {
        return Err(format!(
            "text {i} ({kind:?}): {} dims, expected {EMBEDDING_DIMENSION}",
            vector.len()
        ));
    }
    if !vector.iter().all(|value| value.is_finite()) {
        return Err(format!("text {i} ({kind:?}): a value isn't finite"));
    }
    let norm = vector
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() > NORM_TOLERANCE {
        return Err(format!("text {i} ({kind:?}): norm {norm}, expected 1"));
    }
    Ok(())
}

fn write_f32(path: &Path, vectors: &[Vec<f32>]) -> Result<(), String> {
    let bytes: Vec<u8> = vectors
        .iter()
        .flatten()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    fs::write(path, bytes).map_err(|error| format!("writing {}: {error}", path.display()))
}
