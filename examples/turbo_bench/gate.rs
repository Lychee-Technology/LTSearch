//! Regression gates: a report against the committed baseline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::report::{Environment, Report, CODECS};

pub const BASELINE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    pub tolerances: Tolerances,
    /// The run the numbers below were taken from.
    pub measured_on: Environment,
    /// Keyed by fixture name. A report may have more fixtures; only these
    /// are gated.
    pub fixtures: BTreeMap<String, FixtureBaseline>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Tolerances {
    /// Absolute.
    pub recall_at_10_max_drop: f64,
    /// Absolute.
    pub ndcg_at_10_max_drop: f64,
    /// Relative to the baseline RMSE.
    pub score_rmse_max_rise: f64,
    /// Relative to the baseline ratio.
    pub scan_ratio_max_rise: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            recall_at_10_max_drop: 0.01,
            ndcg_at_10_max_drop: 0.01,
            score_rmse_max_rise: 0.10,
            scan_ratio_max_rise: 0.25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureBaseline {
    /// The fixture must be these exact vectors, or its numbers don't apply.
    pub dataset_digest: String,
    /// Keyed by codec.
    pub quality: BTreeMap<String, QualityBaseline>,
    /// Codec scan p50 over exact-f32 scan p50, keyed by codec. Empty for a
    /// fixture without a performance gate.
    #[serde(default)]
    pub scan_ratio_to_exact: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QualityBaseline {
    pub recall_at_10: f64,
    pub ndcg_at_10: f64,
    pub score_rmse: f64,
}

/// Every way `report` falls outside `baseline`; empty if it passes.
pub fn check(baseline: &Baseline, report: &Report) -> Vec<String> {
    let tolerances = baseline.tolerances;
    let mut violations = Vec::new();
    for (name, expected) in &baseline.fixtures {
        let Some(fixture) = report.fixtures.iter().find(|f| &f.name == name) else {
            violations.push(format!("{name}: fixture missing from the report"));
            continue;
        };
        if fixture.dataset.digest != expected.dataset_digest {
            violations.push(format!(
                "{name}: dataset digest {} differs from the baseline's {}; \
                 a generator change needs a new fixture name and baseline",
                fixture.dataset.digest, expected.dataset_digest
            ));
            continue;
        }

        for (codec, base) in &expected.quality {
            let Some(system) = fixture.systems.get(codec) else {
                violations.push(format!("{name}/{codec}: system missing from the report"));
                continue;
            };
            let q = &system.quality;
            let mut fail = |metric: &str, actual: f64, limit: f64, base: f64| {
                violations.push(format!(
                    "{name}/{codec}: {metric} {actual:.6} is past the limit {limit:.6} \
                     (baseline {base:.6})"
                ))
            };
            let recall_floor = base.recall_at_10 - tolerances.recall_at_10_max_drop;
            if q.recall_at_10 < recall_floor {
                fail("recall@10", q.recall_at_10, recall_floor, base.recall_at_10);
            }
            let ndcg_floor = base.ndcg_at_10 - tolerances.ndcg_at_10_max_drop;
            if q.ndcg_at_10 < ndcg_floor {
                fail("ndcg@10", q.ndcg_at_10, ndcg_floor, base.ndcg_at_10);
            }
            let rmse_ceiling = base.score_rmse * (1.0 + tolerances.score_rmse_max_rise);
            if q.score_rmse > rmse_ceiling {
                fail("score RMSE", q.score_rmse, rmse_ceiling, base.score_rmse);
            }
        }

        // A quality-only run measured no performance; [`notes`] says so.
        if expected.scan_ratio_to_exact.is_empty() || !measures_performance(report) {
            continue;
        }
        if report.environment.build_profile != "release" {
            violations.push(format!(
                "{name}: the performance gate needs a release build, got {}",
                report.environment.build_profile
            ));
            continue;
        }
        for (codec, base) in &expected.scan_ratio_to_exact {
            let Some(performance) = fixture
                .systems
                .get(codec)
                .and_then(|system| system.performance.as_ref())
            else {
                violations.push(format!(
                    "{name}/{codec}: no performance numbers in the report"
                ));
                continue;
            };
            let ceiling = base * (1.0 + tolerances.scan_ratio_max_rise);
            if performance.scan_ratio_to_exact > ceiling {
                violations.push(format!(
                    "{name}/{codec}: scan p50 ÷ exact-f32 scan p50 {:.4} is past the limit \
                     {ceiling:.4} (baseline {base:.4})",
                    performance.scan_ratio_to_exact
                ));
            }
        }
    }
    violations
}

/// What the gate couldn't check, or checked against a different machine.
/// Informational: none of these fail the gate.
pub fn notes(baseline: &Baseline, report: &Report) -> Vec<String> {
    let performance_gated = baseline
        .fixtures
        .values()
        .any(|fixture| !fixture.scan_ratio_to_exact.is_empty());
    if !performance_gated {
        return Vec::new();
    }
    if !measures_performance(report) {
        return vec!["performance gate skipped: the report is quality-only".into()];
    }
    let (measured, run) = (&baseline.measured_on, &report.environment);
    if (&measured.cpu_model, &measured.arch) != (&run.cpu_model, &run.arch) {
        return vec![format!(
            "the baseline's scan ratios were measured on {} ({}), this run on {} ({}); \
             the performance gate is only meaningful on the baseline's machine type",
            measured.cpu_model, measured.arch, run.cpu_model, run.arch
        )];
    }
    Vec::new()
}

fn measures_performance(report: &Report) -> bool {
    report
        .fixtures
        .iter()
        .flat_map(|fixture| fixture.systems.values())
        .any(|system| system.performance.is_some())
}

/// A baseline taken from `report`: quality for every fixture, and scan
/// ratios for `performance_fixtures`.
pub fn from_report(
    report: &Report,
    performance_fixtures: &[String],
    tolerances: Tolerances,
) -> Result<Baseline, String> {
    if report.degrade.is_some() {
        return Err("refusing to take a baseline from a degraded run".into());
    }
    for name in performance_fixtures {
        if !report.fixtures.iter().any(|f| &f.name == name) {
            return Err(format!("performance fixture {name} isn't in the report"));
        }
    }
    let mut fixtures = BTreeMap::new();
    for fixture in &report.fixtures {
        let system = |codec: &str| {
            fixture
                .systems
                .get(codec)
                .ok_or_else(|| format!("{}: no {codec} system", fixture.name))
        };
        let mut quality = BTreeMap::new();
        let mut scan_ratio_to_exact = BTreeMap::new();
        for codec in CODECS {
            let system = system(codec)?;
            quality.insert(
                codec.to_string(),
                QualityBaseline {
                    recall_at_10: system.quality.recall_at_10,
                    ndcg_at_10: system.quality.ndcg_at_10,
                    score_rmse: system.quality.score_rmse,
                },
            );
            if performance_fixtures.contains(&fixture.name) {
                let performance = system
                    .performance
                    .as_ref()
                    .ok_or_else(|| format!("{}/{codec}: no performance numbers", fixture.name))?;
                scan_ratio_to_exact.insert(codec.to_string(), performance.scan_ratio_to_exact);
            }
        }
        fixtures.insert(
            fixture.name.clone(),
            FixtureBaseline {
                dataset_digest: fixture.dataset.digest.clone(),
                quality,
                scan_ratio_to_exact,
            },
        );
    }
    Ok(Baseline {
        schema_version: BASELINE_SCHEMA_VERSION,
        tolerances,
        measured_on: report.environment.clone(),
        fixtures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{
        DatasetInfo, FixtureReport, Latency, Performance, Quality, SystemReport,
        REPORT_SCHEMA_VERSION,
    };

    const FIXTURE: &str = "synthetic-test";

    fn report(recall: f64, ndcg: f64, rmse: f64, ratio: f64) -> Report {
        let system = SystemReport {
            quality: Quality {
                recall_at_1: 1.0,
                recall_at_10: recall,
                recall_at_50: 1.0,
                ndcg_at_10: ndcg,
                score_bias: 0.0,
                score_rmse: rmse,
                score_pairs: 1,
            },
            performance: Some(Performance {
                build: None,
                disk: None,
                load_ms_p50: None,
                prepare_us: None,
                scan_ms: Latency { p50: 1.0, p95: 1.0 },
                docs_per_sec: 1.0,
                scan_ratio_to_exact: ratio,
            }),
        };
        Report {
            schema_version: REPORT_SCHEMA_VERSION,
            environment: Environment {
                git_sha: "abc".into(),
                build_profile: "release".into(),
                opt_level: "z".into(),
                threads: 4,
                cpu_model: "test".into(),
                arch: "aarch64".into(),
                os: "linux".into(),
            },
            degrade: None,
            fixtures: vec![FixtureReport {
                name: FIXTURE.into(),
                dataset: DatasetInfo {
                    generator: "test".into(),
                    docs: 10,
                    queries: 2,
                    dim: 512,
                    digest: "d1".into(),
                },
                systems: CODECS
                    .iter()
                    .map(|codec| (codec.to_string(), system.clone()))
                    .collect(),
            }],
        }
    }

    fn baseline() -> Baseline {
        from_report(
            &report(0.90, 0.95, 0.020, 2.0),
            &[FIXTURE.into()],
            Tolerances::default(),
        )
        .unwrap()
    }

    fn violations(recall: f64, ndcg: f64, rmse: f64, ratio: f64) -> Vec<String> {
        check(&baseline(), &report(recall, ndcg, rmse, ratio))
    }

    #[test]
    fn the_baseline_run_passes() {
        assert_eq!(violations(0.90, 0.95, 0.020, 2.0), Vec::<String>::new());
    }

    #[test]
    fn changes_within_tolerance_pass() {
        assert!(violations(0.8905, 0.9405, 0.0219, 2.49).is_empty());
        // Improvements never fail.
        assert!(violations(1.0, 1.0, 0.001, 0.5).is_empty());
    }

    #[test]
    fn recall_drop_past_tolerance_fails_for_each_codec() {
        let found = violations(0.8895, 0.95, 0.020, 2.0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("legacy_v3: recall@10"), "{found:?}");
        assert!(found[1].contains("prod_v4: recall@10"), "{found:?}");
    }

    #[test]
    fn ndcg_drop_past_tolerance_fails() {
        let found = violations(0.90, 0.9395, 0.020, 2.0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|v| v.contains("ndcg@10")), "{found:?}");
    }

    #[test]
    fn rmse_rise_past_tolerance_fails() {
        let found = violations(0.90, 0.95, 0.0221, 2.0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|v| v.contains("score RMSE")), "{found:?}");
    }

    #[test]
    fn scan_ratio_rise_past_tolerance_fails() {
        let found = violations(0.90, 0.95, 0.020, 2.51);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|v| v.contains("scan p50")), "{found:?}");
    }

    #[test]
    fn a_dataset_change_fails_instead_of_comparing_numbers() {
        let mut changed = report(0.90, 0.95, 0.020, 2.0);
        changed.fixtures[0].dataset.digest = "d2".into();
        let found = check(&baseline(), &changed);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("dataset digest"), "{found:?}");
    }

    #[test]
    fn a_missing_fixture_or_codec_fails() {
        let mut missing = report(0.90, 0.95, 0.020, 2.0);
        missing.fixtures[0].name = "other".into();
        assert!(check(&baseline(), &missing)[0].contains("fixture missing"));

        let mut missing = report(0.90, 0.95, 0.020, 2.0);
        missing.fixtures[0].systems.remove("prod_v4");
        let found = check(&baseline(), &missing);
        assert!(found.iter().any(|v| v.contains("prod_v4: system missing")));
    }

    #[test]
    fn performance_gate_needs_release_numbers() {
        let mut dev = report(0.90, 0.95, 0.020, 2.0);
        dev.environment.build_profile = "dev".into();
        let found = check(&baseline(), &dev);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("needs a release build"), "{found:?}");

        let mut partial = report(0.90, 0.95, 0.020, 2.0);
        partial.fixtures[0]
            .systems
            .get_mut("prod_v4")
            .unwrap()
            .performance = None;
        let found = check(&baseline(), &partial);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("prod_v4: no performance numbers"),
            "{found:?}"
        );
    }

    #[test]
    fn a_quality_only_run_gates_quality_and_notes_the_skipped_performance_gate() {
        let mut quality_only = report(0.90, 0.95, 0.020, 2.0);
        quality_only.environment.build_profile = "dev".into();
        for system in quality_only.fixtures[0].systems.values_mut() {
            system.performance = None;
        }
        assert!(check(&baseline(), &quality_only).is_empty());
        let notes = notes(&baseline(), &quality_only);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("performance gate skipped"), "{notes:?}");

        let system = quality_only.fixtures[0].systems.get_mut("prod_v4").unwrap();
        system.quality.recall_at_10 = 0.5;
        let found = check(&baseline(), &quality_only);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("prod_v4: recall@10"), "{found:?}");
    }

    #[test]
    fn a_run_on_another_machine_is_noted() {
        assert!(notes(&baseline(), &report(0.90, 0.95, 0.020, 2.0)).is_empty());
        let mut elsewhere = report(0.90, 0.95, 0.020, 2.0);
        elsewhere.environment.arch = "x86_64".into();
        let notes = notes(&baseline(), &elsewhere);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("only meaningful on the baseline's machine"));
    }

    #[test]
    fn a_fixture_without_a_performance_baseline_skips_the_performance_gate() {
        let baseline =
            from_report(&report(0.90, 0.95, 0.020, 2.0), &[], Tolerances::default()).unwrap();
        let mut dev = report(0.90, 0.95, 0.020, 9.0);
        dev.environment.build_profile = "dev".into();
        assert!(check(&baseline, &dev).is_empty());
    }

    #[test]
    fn a_degraded_run_is_not_a_baseline() {
        let mut degraded = report(0.90, 0.95, 0.020, 2.0);
        degraded.degrade = Some("drop-qjl".into());
        assert!(from_report(&degraded, &[], Tolerances::default()).is_err());
    }
}
