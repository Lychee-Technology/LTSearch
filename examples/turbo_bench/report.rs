//! The JSON report: one per run, every fixture and system.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const REPORT_SCHEMA_VERSION: u32 = 1;

pub const EXACT: &str = "exact_f32";
pub const LEGACY: &str = "legacy_v3";
pub const PROD: &str = "prod_v4";
/// The systems the gate checks: the codecs, not their ground truth.
pub const CODECS: [&str; 2] = [LEGACY, PROD];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub environment: Environment,
    /// `Some` when the run scored v4 with a deliberately broken estimator.
    pub degrade: Option<String>,
    pub fixtures: Vec<FixtureReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    /// `GITHUB_SHA`, else `git rev-parse HEAD`; `-dirty` if tracked files
    /// changed.
    pub git_sha: String,
    /// `release` or `dev`, from `debug_assertions`.
    pub build_profile: String,
    /// `CARGO_PROFILE_RELEASE_OPT_LEVEL` at build time, else the
    /// `[profile.release]` value in Cargo.toml; `0` for a dev build.
    pub opt_level: String,
    /// The rayon pool's size.
    pub threads: usize,
    pub cpu_model: String,
    pub arch: String,
    pub os: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureReport {
    pub name: String,
    pub dataset: DatasetInfo,
    /// Keyed by [`EXACT`], [`LEGACY`] and [`PROD`].
    pub systems: BTreeMap<String, SystemReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetInfo {
    pub generator: String,
    pub docs: usize,
    pub queries: usize,
    pub dim: usize,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemReport {
    pub quality: Quality,
    /// `None` in a quality-only run.
    pub performance: Option<Performance>,
}

/// Against the exact-f32 top-k of each query, averaged over queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quality {
    pub recall_at_1: f64,
    pub recall_at_10: f64,
    pub recall_at_50: f64,
    pub ndcg_at_10: f64,
    /// Mean of score − exact inner product. Only comparable within a codec:
    /// v3 scores aren't on the inner-product scale.
    pub score_bias: f64,
    pub score_rmse: f64,
    /// (query, doc) pairs behind bias and RMSE.
    pub score_pairs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Performance {
    /// `None` for exact f32, which has nothing to build.
    pub build: Option<Build>,
    /// `None` for exact f32, which isn't written to disk; its vectors are
    /// `4 · dim` bytes each.
    pub disk: Option<Disk>,
    /// `MmapIndex::load` of the release, warm page cache. A fixed per-load
    /// cost, kept apart from per-query latency.
    pub load_ms_p50: Option<f64>,
    /// `None` for exact f32, which has nothing to prepare.
    pub prepare_us: Option<Latency>,
    /// Parallel scan and top-k selection, excluding prepare.
    pub scan_ms: Latency,
    /// Documents divided by the scan p50.
    pub docs_per_sec: f64,
    /// Scan p50 over the exact-f32 scan p50 of the same run. The gated
    /// performance number.
    pub scan_ratio_to_exact: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Build {
    /// The whole `StaticReleaseBuilder` run, which encodes on one thread:
    /// documents divided by its wall time.
    pub release_builder_vectors_per_sec: f64,
    /// Encode only, without the builder's file writes.
    pub encode_single_thread_vectors_per_sec: f64,
    /// Encode only, spread over the rayon pool.
    pub encode_rayon_vectors_per_sec: f64,
    /// Documents encoded for the two encode-only numbers.
    pub encode_sample: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    /// The record file (header included) over the document count.
    pub record_bytes_per_vector: f64,
    /// Each per-document sidecar's size over the document count. They hold
    /// the synthetic text, doc_id and metadata, so they reflect this fixture
    /// rather than a real corpus.
    pub sidecar_bytes_per_vector: BTreeMap<String, f64>,
    /// The codec's asset files, whose size doesn't depend on the corpus.
    pub asset_bytes: u64,
    pub manifest_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Latency {
    pub p50: f64,
    pub p95: f64,
}

impl Environment {
    pub fn detect() -> Self {
        let release = !cfg!(debug_assertions);
        Self {
            git_sha: git_sha(),
            build_profile: if release { "release" } else { "dev" }.into(),
            opt_level: match option_env!("CARGO_PROFILE_RELEASE_OPT_LEVEL") {
                _ if !release => "0".into(),
                Some(level) => level.into(),
                None => manifest_release_opt_level(),
            },
            threads: rayon::current_num_threads(),
            cpu_model: cpu_model(),
            arch: std::env::consts::ARCH.into(),
            os: std::env::consts::OS.into(),
        }
    }
}

/// `opt-level` under `[profile.release]` in Cargo.toml, read at build time;
/// Cargo's default 3 if it isn't set.
fn manifest_release_opt_level() -> String {
    include_str!("../../Cargo.toml")
        .lines()
        .skip_while(|line| line.trim() != "[profile.release]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "opt-level").then(|| value.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "3".into())
}

fn git_sha() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        return sha;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    match git(&["rev-parse", "HEAD"]) {
        Some(sha) => match git(&["status", "--porcelain", "--untracked-files=no"]) {
            Some(status) if !status.is_empty() => format!("{sha}-dirty"),
            _ => sha,
        },
        None => "unknown".into(),
    }
}

/// `model name` on x86_64; aarch64 Linux reports only the implementer and
/// part numbers, so those are kept as they are.
fn cpu_model() -> String {
    let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") else {
        return "unknown".into();
    };
    let field = |name: &str| {
        cpuinfo.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    };
    if let Some(model) = field("model name") {
        return model;
    }
    match (field("CPU implementer"), field("CPU part")) {
        (Some(implementer), Some(part)) => format!("implementer {implementer} part {part}"),
        _ => "unknown".into(),
    }
}

/// The report as a Markdown table, for logs and PR descriptions.
pub fn markdown(report: &Report) -> String {
    let env = &report.environment;
    let mut out = format!(
        "{} ({}), {} threads, profile {} (opt-level {}), git {}",
        env.cpu_model, env.arch, env.threads, env.build_profile, env.opt_level, env.git_sha
    );
    if let Some(degrade) = &report.degrade {
        out.push_str(&format!(", DEGRADED: {degrade}"));
    }
    out.push_str("\n\n| fixture | system | R@1 | R@10 | R@50 | NDCG@10 | bias | RMSE ");
    out.push_str("| build vec/s (builder / 1T / rayon) | record B/vec | load ms ");
    out.push_str("| prepare µs p50 | scan ms p50 / p95 | Mdocs/s | scan ÷ exact |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for fixture in &report.fixtures {
        for (name, system) in &fixture.systems {
            let q = &system.quality;
            out.push_str(&format!(
                "| {} | {name} | {:.3} | {:.3} | {:.3} | {:.4} | {:+.4} | {:.4} ",
                fixture.name,
                q.recall_at_1,
                q.recall_at_10,
                q.recall_at_50,
                q.ndcg_at_10,
                q.score_bias,
                q.score_rmse
            ));
            match &system.performance {
                Some(p) => out.push_str(&format!(
                    "| {} | {} | {} | {} | {:.3} / {:.3} | {:.2} | {:.2} |\n",
                    p.build.as_ref().map_or("—".into(), |b| format!(
                        "{:.0} / {:.0} / {:.0}",
                        b.release_builder_vectors_per_sec,
                        b.encode_single_thread_vectors_per_sec,
                        b.encode_rayon_vectors_per_sec
                    )),
                    p.disk.as_ref().map_or("—".into(), |d| format!(
                        "{:.1}",
                        d.record_bytes_per_vector
                    )),
                    p.load_ms_p50.map_or("—".into(), |ms| format!("{ms:.2}")),
                    p.prepare_us
                        .map_or("—".into(), |us| format!("{:.1}", us.p50)),
                    p.scan_ms.p50,
                    p.scan_ms.p95,
                    p.docs_per_sec / 1e6,
                    p.scan_ratio_to_exact
                )),
                None => out.push_str("| | | | | | | |\n"),
            }
        }
    }
    out
}
