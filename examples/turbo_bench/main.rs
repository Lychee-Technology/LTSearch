//! TurboQuant retrieval-quality and performance benchmark (#168): exact f32,
//! the legacy v3 codec and TurboQuant_prod v4 on the same fixtures, with
//! regression gates against `examples/turbo_bench/baseline.json`.
//!
//! See `examples/turbo_bench/README.md` for the commands and what the
//! numbers mean.

mod dataset;
mod gate;
mod metrics;
mod report;
mod run;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use gate::{Baseline, Tolerances, BASELINE_SCHEMA_VERSION};
use report::{Environment, Report, REPORT_SCHEMA_VERSION};
use run::{Degrade, RunOptions};

const USAGE: &str = "\
usage:
  turbo_bench run [--sizes 1000,10000] [--queries 200] [--out PATH]
                  [--baseline PATH] [--degrade drop-qjl] [--quality-only]
  turbo_bench gate --report PATH --baseline PATH
  turbo_bench baseline --report PATH --out PATH
                       [--performance-fixtures synthetic-agm-v1-n10000]";

const DEFAULT_OUT: &str = "target/turbo-bench/report.json";
const DEFAULT_PERFORMANCE_FIXTURES: &str = "synthetic-agm-v1-n10000";
const TOP_K: usize = 10;
const WARMUP_QUERIES: usize = 20;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("gate") => gate_command(&args[1..]),
        Some("baseline") => baseline_command(&args[1..]).map(|()| true),
        _ => Err("expected a subcommand".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("turbo_bench: {error}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `Ok(false)` when a gate failed.
fn run_command(args: &[String]) -> Result<bool, String> {
    let args = Args::parse(
        args,
        &["sizes", "queries", "out", "baseline", "degrade"],
        &["quality-only"],
    )?;
    let sizes = args
        .get("sizes")
        .unwrap_or("1000,10000")
        .split(',')
        .map(|size| {
            size.trim()
                .parse::<usize>()
                .ok()
                .filter(|&size| size > 0)
                .ok_or_else(|| format!("--sizes: {size:?} isn't a positive integer"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let queries = args
        .get("queries")
        .unwrap_or("200")
        .parse::<usize>()
        .ok()
        .filter(|&queries| queries > 0)
        .ok_or("--queries must be a positive integer")?;
    let degrade = args
        .get("degrade")
        .map(|name| Degrade::from_name(name).ok_or(format!("--degrade: unknown mode {name:?}")))
        .transpose()?;
    let baseline = args.get("baseline").map(read_baseline).transpose()?;
    let out = Path::new(args.get("out").unwrap_or(DEFAULT_OUT));
    let options = RunOptions {
        top_k: TOP_K,
        measure_performance: !args.flag("quality-only"),
        warmup_queries: WARMUP_QUERIES.min(queries),
        degrade,
    };

    let environment = Environment::detect();
    if environment.build_profile != "release" && options.measure_performance {
        eprintln!("turbo_bench: warning: a dev build; performance numbers are meaningless");
    }
    let work_dir = tempfile::tempdir().map_err(|error| format!("creating a work dir: {error}"))?;
    let mut fixtures = Vec::with_capacity(sizes.len());
    for &size in &sizes {
        let dataset = dataset::synthetic(size, queries);
        eprintln!("turbo_bench: {} ({queries} queries)", dataset.name);
        let dir = work_dir.path().join(&dataset.name);
        fs::create_dir(&dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
        fixtures.push(run::run_fixture(&dataset, &options, &dir)?);
        // The releases of a large fixture take disk; drop them before the next.
        fs::remove_dir_all(&dir).map_err(|error| format!("removing {}: {error}", dir.display()))?;
    }
    let report = Report {
        schema_version: REPORT_SCHEMA_VERSION,
        environment,
        degrade: degrade.map(|degrade| degrade.name().into()),
        fixtures,
    };

    write_json(out, &report)?;
    let mut summary = report::markdown(&report);
    let passed = match &baseline {
        Some(baseline) => {
            let violations = gate::check(baseline, &report);
            summary.push_str(&gate_summary(&violations, &gate::notes(baseline, &report)));
            violations.is_empty()
        }
        None => true,
    };
    println!("{summary}");
    eprintln!("turbo_bench: wrote {}", out.display());
    append_step_summary(&summary)?;
    Ok(passed)
}

fn gate_command(args: &[String]) -> Result<bool, String> {
    let args = Args::parse(args, &["report", "baseline"], &[])?;
    let report = read_report(args.require("report")?)?;
    let baseline = read_baseline(args.require("baseline")?)?;
    let violations = gate::check(&baseline, &report);
    let summary = gate_summary(&violations, &gate::notes(&baseline, &report));
    println!("{summary}");
    append_step_summary(&summary)?;
    Ok(violations.is_empty())
}

fn baseline_command(args: &[String]) -> Result<(), String> {
    let args = Args::parse(args, &["report", "out", "performance-fixtures"], &[])?;
    let report = read_report(args.require("report")?)?;
    let out = Path::new(args.require("out")?);
    let performance_fixtures: Vec<String> = args
        .get("performance-fixtures")
        .unwrap_or(DEFAULT_PERFORMANCE_FIXTURES)
        .split(',')
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    // Hand-tuned tolerances survive a refresh of the numbers.
    let tolerances = if out.exists() {
        read_baseline(out)?.tolerances
    } else {
        Tolerances::default()
    };
    let baseline = gate::from_report(&report, &performance_fixtures, tolerances)?;
    write_json(out, &baseline)?;
    eprintln!("turbo_bench: wrote {}", out.display());
    Ok(())
}

fn gate_summary(violations: &[String], notes: &[String]) -> String {
    let mut out = if violations.is_empty() {
        "\nGate: passed.\n".to_string()
    } else {
        format!("\nGate: FAILED, {} violation(s):\n\n", violations.len())
    };
    for violation in violations {
        out.push_str(&format!("- {violation}\n"));
    }
    for note in notes {
        out.push_str(&format!("\nNote: {note}.\n"));
    }
    out
}

/// GitHub Actions renders `$GITHUB_STEP_SUMMARY` on the run page.
fn append_step_summary(markdown: &str) -> Result<(), String> {
    let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") else {
        return Ok(());
    };
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{markdown}"))
        .map_err(|error| format!("writing {path}: {error}"))
}

fn read_report(path: impl AsRef<Path>) -> Result<Report, String> {
    let report: Report = read_json(path.as_ref())?;
    if report.schema_version != REPORT_SCHEMA_VERSION {
        return Err(format!(
            "{}: report schema {}, this harness reads {REPORT_SCHEMA_VERSION}",
            path.as_ref().display(),
            report.schema_version
        ));
    }
    Ok(report)
}

fn read_baseline(path: impl AsRef<Path>) -> Result<Baseline, String> {
    let baseline: Baseline = read_json(path.as_ref())?;
    if baseline.schema_version != BASELINE_SCHEMA_VERSION {
        return Err(format!(
            "{}: baseline schema {}, this harness reads {BASELINE_SCHEMA_VERSION}",
            path.as_ref().display(),
            baseline.schema_version
        ));
    }
    Ok(baseline)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parsing {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let mut json = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    json.push('\n');
    fs::write(path, json).map_err(|error| format!("writing {}: {error}", path.display()))
}

/// `--name value` options and bare `--name` flags.
struct Args {
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
}

impl Args {
    fn parse(args: &[String], options: &[&str], flags: &[&str]) -> Result<Self, String> {
        let mut parsed = Self {
            values: BTreeMap::new(),
            flags: BTreeSet::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let name = arg
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument {arg:?}"))?;
            if flags.contains(&name) {
                parsed.flags.insert(name.into());
            } else if options.contains(&name) {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                parsed.values.insert(name.into(), value.clone());
            } else {
                return Err(format!("unknown option --{name}"));
            }
        }
        Ok(parsed)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn require(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}
