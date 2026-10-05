//! One fixture through the three systems.
//!
//! Both codecs are measured on releases written by `StaticReleaseBuilder` and
//! read back with `MmapIndex::load`, scored the way `TurboQuantSearcher`
//! scores them; every quality query also goes through
//! `TurboQuantSearcher::search` and must return the ranking the harness
//! measured.

use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ltsearch::index::{
    encode_vector, CentroidTable, EmbeddingProfile, IndexCodec, MmapIndex, PreparedTurboProdQuery,
    PreparedTurboQuery, ProjectionMatrix, ReleaseSource, StaticChunk, StaticReleaseBuilder,
    StaticReleaseFormat, TurboProdRecord512, TurboQuantProdV1, TurboRecord512, TurboRecordSlice,
    CODEBOOK_FILE, QJL_FILE, RELEASE_MANIFEST_FILE, ROTATION_FILE,
};
use ltsearch::models::{CorpusType, IndexManifest, ShardManifest};
use ltsearch::query::turbo_searcher::{scan_top_k, RankedResult};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};
use rayon::prelude::*;

use crate::dataset::{Dataset, Vector, DIM, GENERATOR};
use crate::metrics::{mean, ndcg_at, percentile, recall_at, ScoreError};
use crate::report::{
    Build, DatasetInfo, Disk, FixtureReport, Latency, Performance, Quality, SystemReport, EXACT,
    LEGACY, PROD,
};

/// Ranking depth for quality: Recall@50 needs the top 50.
const QUALITY_DEPTH: usize = 50;
/// Bias and RMSE use every document up to this many, then this many spread
/// evenly over the corpus.
const PAIR_SAMPLE: usize = 1_000;
const LOAD_REPEATS: usize = 20;
const ENCODE_SAMPLE: usize = 10_000;
const RECORD_FILE: &str = "turbo_static.bin";
const LEGACY_ASSETS: [&str; 2] = ["centroids.bin", "projection.bin"];
const PROD_ASSETS: [&str; 3] = [CODEBOOK_FILE, QJL_FILE, ROTATION_FILE];

/// A deliberately broken v4 estimator, to show the gate catches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degrade {
    /// Score with the MSE term alone: `norm · mse_term`.
    DropQjl,
}

impl Degrade {
    pub fn from_name(name: &str) -> Option<Self> {
        (name == "drop-qjl").then_some(Self::DropQjl)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::DropQjl => "drop-qjl",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    /// Top-k of the timed scans.
    pub top_k: usize,
    pub measure_performance: bool,
    /// Untimed queries run through every system before timing.
    pub warmup_queries: usize,
    pub degrade: Option<Degrade>,
}

pub fn run_fixture(
    dataset: &Dataset,
    options: &RunOptions,
    work_dir: &Path,
) -> Result<FixtureReport, String> {
    let names: Vec<String> = (0..dataset.docs.len()).map(doc_name).collect();
    let chunks: Vec<StaticChunk> = names
        .iter()
        .enumerate()
        .map(|(i, name)| StaticChunk {
            doc_id: name.clone(),
            text: format!("synthetic document {i}"),
            ..StaticChunk::default()
        })
        .collect();
    let embeddings: Vec<Vec<f32>> = dataset.docs.iter().map(|doc| doc.to_vec()).collect();
    let build = |format, label: &str| {
        Release::build(
            work_dir.join(label),
            format,
            dataset,
            &chunks,
            &embeddings,
            &names,
        )
    };
    let legacy = build(StaticReleaseFormat::V3, "v3")?;
    let prod = build(
        StaticReleaseFormat::from_name("v4").expect("v4 is a release format"),
        "v4",
    )?;

    let (
        TurboRecordSlice::V2Dim512(legacy_records),
        IndexCodec::Legacy {
            centroids,
            projection,
        },
    ) = (legacy.index.records(), legacy.index.codec())
    else {
        return Err("the v3 release didn't load as a legacy index".into());
    };
    let (TurboRecordSlice::V4Dim512(prod_records), IndexCodec::Prod(codec)) =
        (prod.index.records(), prod.index.codec())
    else {
        return Err("the v4 release didn't load as a TurboQuant_prod index".into());
    };
    // The exact scan ranks ties by the same hashed doc_id as the codecs.
    let exact_docs: Vec<ExactDoc> = prod_records
        .iter()
        .zip(&dataset.docs)
        .map(|(record, vector)| ExactDoc {
            doc_id: record.doc_id,
            vector: *vector,
        })
        .collect();

    let drop_qjl = options.degrade == Some(Degrade::DropQjl);
    let exact = System::Exact { docs: &exact_docs };
    let legacy_system = System::Legacy {
        records: legacy_records,
        centroids,
        projection,
    };
    let prod_system = System::Prod {
        records: prod_records,
        codec,
        drop_qjl,
    };

    let ground_truth = GroundTruth::new(&exact, dataset);
    let legacy_searcher = TurboQuantSearcher::new(legacy.index.clone());
    let prod_searcher = TurboQuantSearcher::new(prod.index.clone());
    // A degraded v4 ranks differently from the searcher by design.
    let prod_check = (!drop_qjl).then_some(&prod_searcher);
    let qualities = [
        quality(&exact, dataset, &ground_truth, None, &names)?,
        quality(
            &legacy_system,
            dataset,
            &ground_truth,
            Some(&legacy_searcher),
            &names,
        )?,
        quality(&prod_system, dataset, &ground_truth, prod_check, &names)?,
    ];

    let performances = if options.measure_performance {
        let systems = [&exact, &legacy_system, &prod_system];
        let timings = time_queries(&systems, dataset, options);
        let exact_scan_p50 = percentile(&timings[0].scan_ms, 50.0);
        let performance = |index: usize, release: Option<&Release>| -> Result<_, String> {
            let timing = &timings[index];
            let scan_ms = latency(&timing.scan_ms);
            Ok(Some(Performance {
                build: release.map(|release| release.throughput(systems[index], dataset)),
                disk: release.map(|release| release.disk()).transpose()?,
                load_ms_p50: release.map(|release| release.load_ms_p50()).transpose()?,
                prepare_us: release.map(|_| latency(&timing.prepare_us)),
                scan_ms,
                docs_per_sec: dataset.docs.len() as f64 / (scan_ms.p50 / 1e3),
                scan_ratio_to_exact: scan_ms.p50 / exact_scan_p50,
            }))
        };
        [
            performance(0, None)?,
            performance(1, Some(&legacy))?,
            performance(2, Some(&prod))?,
        ]
    } else {
        [None, None, None]
    };

    let systems = [EXACT, LEGACY, PROD]
        .into_iter()
        .zip(qualities)
        .zip(performances)
        .map(|((name, quality), performance)| {
            (
                name.to_string(),
                SystemReport {
                    quality,
                    performance,
                },
            )
        })
        .collect();
    Ok(FixtureReport {
        name: dataset.name.clone(),
        dataset: DatasetInfo {
            generator: GENERATOR.into(),
            docs: dataset.docs.len(),
            queries: dataset.queries.len(),
            dim: DIM,
            digest: dataset.digest.clone(),
        },
        systems,
    })
}

/// Sorted by position, as the release builder requires.
fn doc_name(index: usize) -> String {
    format!("doc-{index:07}")
}

struct Release {
    dir: PathBuf,
    format: StaticReleaseFormat,
    index: Arc<MmapIndex>,
    builder_seconds: f64,
}

impl Release {
    fn build(
        dir: PathBuf,
        format: StaticReleaseFormat,
        dataset: &Dataset,
        chunks: &[StaticChunk],
        embeddings: &[Vec<f32>],
        names: &[String],
    ) -> Result<Self, String> {
        let profile = EmbeddingProfile {
            model_id: format!("synthetic/{GENERATOR}"),
            dim: DIM as u32,
        };
        let source = ReleaseSource {
            kind: "synthetic".into(),
            dataset_path: dataset.name.clone(),
            table_version: 0,
            table_row_count: chunks.len() as u64,
            corpus_type: CorpusType::Legal,
        };
        let started = Instant::now();
        StaticReleaseBuilder::new(format)
            .build_release(&dir, chunks, embeddings, &profile, &source)
            .map_err(|error| format!("building {}: {error}", dir.display()))?;
        let builder_seconds = started.elapsed().as_secs_f64();

        let index = MmapIndex::load(&dir)
            .map_err(|error| format!("loading {}: {error:?}", dir.display()))?;
        // Rankings are compared by record position across the three systems,
        // so the release must keep the input order.
        for (i, name) in names.iter().enumerate() {
            let stored = index
                .original_doc_id(i)
                .map_err(|error| format!("reading doc_id {i}: {error:?}"))?;
            if stored != Some(name.as_str()) {
                return Err(format!(
                    "{}: record {i} is {stored:?}, expected {name}",
                    dir.display()
                ));
            }
        }
        Ok(Self {
            dir,
            format,
            index: Arc::new(index),
            builder_seconds,
        })
    }

    fn load_ms_p50(&self) -> Result<f64, String> {
        let mut times = Vec::with_capacity(LOAD_REPEATS);
        for _ in 0..LOAD_REPEATS {
            let started = Instant::now();
            let index = MmapIndex::load(&self.dir)
                .map_err(|error| format!("loading {}: {error:?}", self.dir.display()))?;
            times.push(started.elapsed().as_secs_f64() * 1e3);
            black_box(index);
        }
        Ok(percentile(&times, 50.0))
    }

    fn throughput(&self, system: &System, dataset: &Dataset) -> Build {
        let sample = &dataset.docs[..dataset.docs.len().min(ENCODE_SAMPLE)];
        let started = Instant::now();
        for doc in sample {
            system.encode(doc);
        }
        let single = started.elapsed().as_secs_f64();
        let started = Instant::now();
        sample.par_iter().for_each(|doc| system.encode(doc));
        let rayon = started.elapsed().as_secs_f64();
        Build {
            release_builder_vectors_per_sec: dataset.docs.len() as f64 / self.builder_seconds,
            encode_single_thread_vectors_per_sec: sample.len() as f64 / single,
            encode_rayon_vectors_per_sec: sample.len() as f64 / rayon,
            encode_sample: sample.len(),
        }
    }

    fn disk(&self) -> Result<Disk, String> {
        let assets: &[&str] = match self.format {
            StaticReleaseFormat::V3 => &LEGACY_ASSETS,
            StaticReleaseFormat::V4(_) => &PROD_ASSETS,
        };
        let docs = self.index.record_count() as f64;
        let mut disk = Disk {
            record_bytes_per_vector: 0.0,
            sidecar_bytes_per_vector: Default::default(),
            asset_bytes: 0,
            manifest_bytes: 0,
        };
        let entries = fs::read_dir(&self.dir)
            .map_err(|error| format!("listing {}: {error}", self.dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("listing {}: {error}", self.dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = entry
                .metadata()
                .map_err(|error| format!("reading {name}: {error}"))?
                .len();
            if name == RECORD_FILE {
                disk.record_bytes_per_vector = bytes as f64 / docs;
            } else if name == RELEASE_MANIFEST_FILE {
                disk.manifest_bytes = bytes;
            } else if assets.contains(&name.as_str()) {
                disk.asset_bytes += bytes;
            } else {
                disk.sidecar_bytes_per_vector
                    .insert(name, bytes as f64 / docs);
            }
        }
        Ok(disk)
    }
}

struct ExactDoc {
    doc_id: u64,
    vector: Vector,
}

enum System<'a> {
    Exact {
        docs: &'a [ExactDoc],
    },
    Legacy {
        records: &'a [TurboRecord512],
        centroids: &'a CentroidTable,
        projection: &'a ProjectionMatrix,
    },
    Prod {
        records: &'a [TurboProdRecord512],
        codec: &'a TurboQuantProdV1,
        drop_qjl: bool,
    },
}

enum Prepared {
    Exact(Box<Vector>),
    Legacy(Box<PreparedTurboQuery>),
    Prod(PreparedTurboProdQuery),
}

impl System<'_> {
    fn prepare(&self, query: &Vector) -> Prepared {
        match self {
            Self::Exact { .. } => Prepared::Exact(Box::new(*query)),
            Self::Legacy {
                centroids,
                projection,
                ..
            } => Prepared::Legacy(Box::new(
                PreparedTurboQuery::prepare(query, centroids, projection)
                    .expect("a 512-d query prepares"),
            )),
            Self::Prod { codec, .. } => {
                Prepared::Prod(codec.prepare_query(query).expect("a 512-d query prepares"))
            }
        }
    }

    /// The top `top_k`, best first. The codec arms score the way
    /// `TurboQuantSearcher` does.
    fn scan(&self, prepared: &Prepared, top_k: usize) -> Vec<RankedResult> {
        let heap = match (self, prepared) {
            (Self::Exact { docs }, Prepared::Exact(query)) => {
                scan_top_k(docs, top_k, |doc| (doc.doc_id, dot(query, &doc.vector)))
            }
            (Self::Legacy { records, .. }, Prepared::Legacy(query)) => {
                scan_top_k(records, top_k, |record| {
                    (record.doc_id, query.score(record))
                })
            }
            (
                Self::Prod {
                    records,
                    drop_qjl: false,
                    ..
                },
                Prepared::Prod(query),
            ) => scan_top_k(records, top_k, |record| {
                (record.doc_id, query.score(record.code()))
            }),
            (
                Self::Prod {
                    records,
                    drop_qjl: true,
                    ..
                },
                Prepared::Prod(query),
            ) => scan_top_k(records, top_k, |record| {
                (record.doc_id, mse_only(query, record))
            }),
            _ => unreachable!("query prepared for another system"),
        };
        heap.into_sorted_vec()
    }

    fn score(&self, prepared: &Prepared, index: usize) -> f32 {
        match (self, prepared) {
            (Self::Exact { docs }, Prepared::Exact(query)) => dot(query, &docs[index].vector),
            (Self::Legacy { records, .. }, Prepared::Legacy(query)) => query.score(&records[index]),
            (
                Self::Prod {
                    records, drop_qjl, ..
                },
                Prepared::Prod(query),
            ) => {
                if *drop_qjl {
                    mse_only(query, &records[index])
                } else {
                    query.score(records[index].code())
                }
            }
            _ => unreachable!("query prepared for another system"),
        }
    }

    fn encode(&self, doc: &Vector) {
        match self {
            Self::Exact { .. } => {}
            Self::Legacy {
                centroids,
                projection,
                ..
            } => {
                black_box(encode_vector(doc, centroids, projection).expect("a 512-d doc encodes"));
            }
            Self::Prod { codec, .. } => {
                black_box(codec.encode(doc).expect("a 512-d doc encodes"));
            }
        }
    }
}

fn dot(query: &Vector, doc: &Vector) -> f32 {
    query.iter().zip(doc).map(|(q, d)| q * d).sum()
}

fn mse_only(query: &PreparedTurboProdQuery, record: &TurboProdRecord512) -> f32 {
    let terms = query.score_breakdown(record.code());
    terms.norm * terms.mse_term
}

/// The exact top [`QUALITY_DEPTH`] of each query, with its inner products.
struct GroundTruth {
    rankings: Vec<Vec<usize>>,
    scores: Vec<Vec<f64>>,
}

impl GroundTruth {
    fn new(exact: &System, dataset: &Dataset) -> Self {
        let (rankings, scores) = dataset
            .queries
            .par_iter()
            .map(|query| {
                let ranked = exact.scan(&exact.prepare(query), QUALITY_DEPTH);
                (
                    ranked.iter().map(|r| r.record_index as usize).collect(),
                    ranked.iter().map(|r| f64::from(r.score)).collect(),
                )
            })
            .unzip();
        Self { rankings, scores }
    }
}

fn quality(
    system: &System,
    dataset: &Dataset,
    truth: &GroundTruth,
    searcher: Option<&TurboQuantSearcher>,
    names: &[String],
) -> Result<Quality, String> {
    let docs = &dataset.docs;
    let pair_docs: Vec<usize> = if docs.len() <= PAIR_SAMPLE {
        (0..docs.len()).collect()
    } else {
        (0..PAIR_SAMPLE)
            .map(|j| j * docs.len() / PAIR_SAMPLE)
            .collect()
    };
    let manifest = stub_manifest();

    let per_query: Vec<([f64; 4], ScoreError)> = dataset
        .queries
        .par_iter()
        .enumerate()
        .map(|(q, query)| {
            let prepared = system.prepare(query);
            let ranking: Vec<usize> = system
                .scan(&prepared, QUALITY_DEPTH)
                .iter()
                .map(|r| r.record_index as usize)
                .collect();

            if let Some(searcher) = searcher {
                let results = searcher
                    .search(&manifest, query, QUALITY_DEPTH)
                    .map_err(|error| format!("TurboQuantSearcher::search: {error:?}"))?;
                let searched: Vec<&str> = results.iter().map(|r| r.doc_id.as_str()).collect();
                let measured: Vec<&str> = ranking.iter().map(|&i| names[i].as_str()).collect();
                if searched != measured {
                    return Err(format!(
                        "query {q}: TurboQuantSearcher returned {searched:?}, \
                         the harness measured {measured:?}"
                    ));
                }
            }

            let exact = &truth.rankings[q];
            let relevance: Vec<f64> = ranking
                .iter()
                .take(10)
                .map(|&i| f64::from(dot(query, &docs[i])))
                .collect();
            let mut error = ScoreError::default();
            for &doc in &pair_docs {
                error.add(system.score(&prepared, doc), dot(query, &docs[doc]));
            }
            Ok((
                [
                    recall_at(1, exact, &ranking),
                    recall_at(10, exact, &ranking),
                    recall_at(QUALITY_DEPTH, exact, &ranking),
                    ndcg_at(10, &relevance, &truth.scores[q]),
                ],
                error,
            ))
        })
        .collect::<Result<_, String>>()?;

    let column =
        |metric: usize| mean(&per_query.iter().map(|(m, _)| m[metric]).collect::<Vec<_>>());
    // Summed in query order, so the result doesn't depend on rayon's split.
    let error = per_query
        .iter()
        .fold(ScoreError::default(), |total, (_, error)| {
            total.merge(error)
        });
    Ok(Quality {
        recall_at_1: column(0),
        recall_at_10: column(1),
        recall_at_50: column(2),
        ndcg_at_10: column(3),
        score_bias: error.bias(),
        score_rmse: error.rmse(),
        score_pairs: error.pairs(),
    })
}

struct Timings {
    prepare_us: Vec<f64>,
    scan_ms: Vec<f64>,
}

/// Times every query through every system. Systems take turns within each
/// query, in a rotating order, so drift in machine load during the run
/// lands on all of them alike.
fn time_queries(systems: &[&System; 3], dataset: &Dataset, options: &RunOptions) -> [Timings; 3] {
    for query in dataset.queries.iter().take(options.warmup_queries) {
        for system in systems {
            black_box(system.scan(&system.prepare(query), options.top_k));
        }
    }
    let mut timings = [(); 3].map(|_| Timings {
        prepare_us: Vec::with_capacity(dataset.queries.len()),
        scan_ms: Vec::with_capacity(dataset.queries.len()),
    });
    for (q, query) in dataset.queries.iter().enumerate() {
        for turn in 0..systems.len() {
            let s = (q + turn) % systems.len();
            let started = Instant::now();
            let prepared = black_box(systems[s].prepare(query));
            let prepared_at = Instant::now();
            black_box(systems[s].scan(&prepared, options.top_k));
            let scanned_at = Instant::now();
            timings[s]
                .prepare_us
                .push((prepared_at - started).as_secs_f64() * 1e6);
            timings[s]
                .scan_ms
                .push((scanned_at - prepared_at).as_secs_f64() * 1e3);
        }
    }
    timings
}

fn latency(values: &[f64]) -> Latency {
    Latency {
        p50: percentile(values, 50.0),
        p95: percentile(values, 95.0),
    }
}

/// `search` doesn't read the manifest; the static tier passes one through.
fn stub_manifest() -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: 1,
            manifest_path: "m.json".into(),
            updated_at: 0,
        },
        manifest: IndexManifest {
            version_id: 1,
            created_at: 0,
            embedding_dim: DIM,
            document_count: 0,
            num_shards: 0,
            shards: vec![ShardManifest {
                shard_id: 0,
                document_count: 0,
                lance_path: String::new(),
                tantivy_path: String::new(),
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::synthetic;
    use crate::gate::{check, from_report, Tolerances};
    use crate::report::{Environment, Report, REPORT_SCHEMA_VERSION};

    fn quality_only(degrade: Option<Degrade>) -> RunOptions {
        RunOptions {
            top_k: 10,
            measure_performance: false,
            warmup_queries: 0,
            degrade,
        }
    }

    fn report(fixture: FixtureReport, degrade: Option<Degrade>) -> Report {
        Report {
            schema_version: REPORT_SCHEMA_VERSION,
            environment: Environment::detect(),
            degrade: degrade.map(|d| d.name().into()),
            fixtures: vec![fixture],
        }
    }

    #[test]
    fn exact_system_is_its_own_ground_truth() {
        let dataset = synthetic(300, 8);
        let dir = tempfile::tempdir().unwrap();
        let fixture = run_fixture(&dataset, &quality_only(None), dir.path()).unwrap();
        let exact = &fixture.systems[EXACT].quality;
        assert_eq!(
            (exact.recall_at_1, exact.recall_at_10, exact.recall_at_50),
            (1.0, 1.0, 1.0)
        );
        assert_eq!(exact.ndcg_at_10, 1.0);
        assert_eq!((exact.score_bias, exact.score_rmse), (0.0, 0.0));
        assert_eq!(exact.score_pairs, 300 * 8);
        // v4 is an unbiased inner-product estimate, so it's close on average.
        let prod = &fixture.systems[PROD].quality;
        assert!(prod.score_bias.abs() < 0.01, "{prod:?}");
        assert!(prod.recall_at_10 > 0.5, "{prod:?}");
    }

    #[test]
    fn dropping_the_qjl_term_fails_the_quality_gate() {
        let dataset = synthetic(1_000, 50);
        let dir = tempfile::tempdir().unwrap();
        let healthy = run_fixture(&dataset, &quality_only(None), &dir.path().join("a")).unwrap();
        let degrade = Some(Degrade::DropQjl);
        let degraded =
            run_fixture(&dataset, &quality_only(degrade), &dir.path().join("b")).unwrap();

        let baseline = from_report(&report(healthy, None), &[], Tolerances::default()).unwrap();
        let violations = check(&baseline, &report(degraded, degrade));
        assert!(
            violations.iter().any(|v| v.contains("prod_v4")),
            "{violations:?}"
        );
        // Only v4 was degraded.
        assert!(
            violations.iter().all(|v| v.contains("prod_v4")),
            "{violations:?}"
        );
    }
}
