//! Task 14 (#112 PR-2): emit a pinned Lance `documents` fixture for the static
//! release v3 end-to-end flow (`scripts/e2e/run-static-release-flow.sh`).
//!
//! Builds a 512-dim `FixedSizeList<Float32,512>` `documents` table whose schema
//! and batch construction mirror `tests/static_release_cli_test.rs`
//! (`make_schema`/`make_batch`/`create_documents_table`). Each row's `metadata`
//! column is a JSON object carrying citation fields
//! (`resource_id`/`source_type`/`source_ref`/`title`/`url`/`lang`) so the query
//! path can build a `Citation` and apply the `{"lang":"zh"}` filter. Rows use
//! non-numeric `doc_id`s (`doc-alpha`, ...) so the flow can strongly assert
//! `static_chunks[0].doc_id == "doc-alpha"`.
//!
//! Embeddings are all `0.1` so they rank identically against the query's fixed
//! `0.1`-repeated embedding; the `lang` filter alone selects the top chunk.
//!
//! CLI: `emit_static_lance_fixture <dataset_path> [--variant a|b]`. Variant `b`
//! changes one English row's text so the derived `release_id` differs from
//! variant `a` while the Chinese `doc-alpha` row (and thus the `lang:zh`
//! assertions) stays identical. On success the created table version is printed
//! as a single integer line on stdout for the driver to pin in the build config.

use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};

const DIM: i32 = 512;

struct FixtureRow {
    doc_id: &'static str,
    text: String,
    metadata: String,
}

fn make_schema(dim: i32) -> Arc<ArrowSchema> {
    Arc::new(ArrowSchema::new(vec![
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("timestamp", DataType::Int64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            true,
        ),
    ]))
}

fn make_batch(schema: Arc<ArrowSchema>, dim: i32, rows: &[FixtureRow]) -> RecordBatch {
    let doc_ids = StringArray::from(rows.iter().map(|r| r.doc_id).collect::<Vec<_>>());
    let texts = StringArray::from(rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>());
    let metadata = StringArray::from(rows.iter().map(|r| r.metadata.as_str()).collect::<Vec<_>>());
    let timestamps = Int64Array::from(vec![0_i64; rows.len()]);
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|_| Some((0..dim).map(|_| Some(0.1_f32)).collect::<Vec<_>>())),
        dim,
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(doc_ids),
            Arc::new(texts),
            Arc::new(metadata),
            Arc::new(timestamps),
            Arc::new(embeddings),
        ],
    )
    .expect("record batch construction must succeed")
}

async fn create_documents_table(dataset_path: &str, dim: i32, rows: &[FixtureRow]) -> u64 {
    let schema = make_schema(dim);
    let batch = make_batch(schema.clone(), dim, rows);
    let batches: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(batch)].into_iter(),
        schema,
    ));

    let conn = lancedb::connect(dataset_path)
        .execute()
        .await
        .expect("lancedb connect must succeed");
    let table = conn
        .create_table("documents", batches)
        .execute()
        .await
        .expect("create documents table must succeed");
    table
        .version()
        .await
        .expect("table version must be readable")
}

fn metadata_json(
    resource_id: &str,
    source_ref: &str,
    title: &str,
    url: &str,
    lang: &str,
) -> String {
    serde_json::json!({
        "resource_id": resource_id,
        "source_type": "statute",
        "source_ref": source_ref,
        "title": title,
        "url": url,
        "lang": lang,
    })
    .to_string()
}

/// Rows for the requested variant. `doc-alpha` (lang `zh`) is the only Chinese
/// row and never changes, so the `{"lang":"zh"}` filter deterministically
/// selects it as `static_chunks[0]`. Variant `b` rewrites `doc-gamma`'s text so
/// the content fingerprint — and thus the derived `release_id` — differs.
fn rows_for_variant(variant: &str) -> Vec<FixtureRow> {
    let gamma_text = match variant {
        "b" => "gamma english body revised for variant b".to_string(),
        _ => "gamma english body for variant a".to_string(),
    };
    vec![
        FixtureRow {
            doc_id: "doc-alpha",
            text: "alpha 中文法规正文，用于中文过滤断言".to_string(),
            metadata: metadata_json(
                "res-alpha",
                "ref-alpha",
                "Alpha 中文标题",
                "https://example.com/alpha",
                "zh",
            ),
        },
        FixtureRow {
            doc_id: "doc-beta",
            text: "beta english statute body for citation coverage".to_string(),
            metadata: metadata_json(
                "res-beta",
                "ref-beta",
                "Beta English Title",
                "https://example.com/beta",
                "en",
            ),
        },
        FixtureRow {
            doc_id: "doc-gamma",
            text: gamma_text,
            metadata: metadata_json(
                "res-gamma",
                "ref-gamma",
                "Gamma English Title",
                "https://example.com/gamma",
                "en",
            ),
        },
    ]
}

#[tokio::main]
async fn main() {
    let mut dataset_path: Option<String> = None;
    let mut variant = "a".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--variant" => {
                variant = args
                    .next()
                    .expect("--variant requires a value (a|b)")
                    .to_string();
            }
            other if other.starts_with("--") => panic!("unknown argument: {other}"),
            positional => {
                if dataset_path.is_some() {
                    panic!("unexpected extra positional argument: {positional}");
                }
                dataset_path = Some(positional.to_string());
            }
        }
    }

    let dataset_path =
        dataset_path.expect("usage: emit_static_lance_fixture <dataset_path> [--variant a|b]");
    assert!(
        variant == "a" || variant == "b",
        "--variant must be 'a' or 'b', got {variant}"
    );

    let rows = rows_for_variant(&variant);
    let version = create_documents_table(&dataset_path, DIM, &rows).await;

    eprintln!(
        "emitted {} documents (variant {variant}) into {dataset_path} at table version {version}",
        rows.len()
    );
    // Sole stdout line: the pinned table version for the static-build config.
    println!("{version}");
}
