use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use indexmap::IndexMap;
use serde_json::Value;

use crate::jsonfmt::{object, pretty_json_bytes};
use crate::model::{MatchingConfig, RunRequest, load_dataset};
use crate::runner::{default_output_root, display_path, project_root, run_backtest};

#[derive(Debug, Parser)]
#[command(
    name = "rust_backtester",
    about = "Rust IMC Prosperity backtester",
    version
)]
struct Args {
    #[arg(long)]
    trader: Option<PathBuf>,
    #[arg(long)]
    dataset: Option<String>,
    #[arg(long)]
    day: Option<i64>,
    #[arg(long = "run-id")]
    run_id: Option<String>,
    #[arg(long = "trade-match-mode", default_value = "all")]
    trade_match_mode: String,
    #[arg(long = "queue-penetration", default_value_t = 1.0)]
    queue_penetration: f64,
    #[arg(long = "price-slippage-bps", default_value_t = 0.0)]
    price_slippage_bps: f64,
    #[arg(long = "output-root")]
    output_root: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    persist: bool,
    #[arg(long, value_enum, default_value_t = ProductDisplayMode::Summary)]
    products: ProductDisplayMode,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let trader = resolve_trader(args.trader.as_deref())?;
    let dataset = resolve_dataset_input(args.dataset.as_deref())?;
    let output_root = args.output_root.clone().unwrap_or_else(default_output_root);

    let (run_id_seed, plans) = build_run_plan(
        &dataset.roots,
        args.day,
        args.run_id.as_deref(),
        dataset.exclude_submission_when_day_filtered,
    )?;
    let mut rows = Vec::with_capacity(plans.len());
    let mut outputs = Vec::with_capacity(plans.len());
    let matching = MatchingConfig {
        trade_match_mode: args.trade_match_mode.clone(),
        queue_penetration: args.queue_penetration,
        price_slippage_bps: args.price_slippage_bps,
    };

    for plan in plans {
        let output = run_backtest(&RunRequest {
            trader_file: trader.path.clone(),
            dataset_file: plan.dataset_file.clone(),
            day: plan.day,
            matching: matching.clone(),
            run_id: Some(plan.run_id),
            output_root: output_root.clone(),
            persist: args.persist,
            write_submission_log: true,
            materialize_artifacts: args.persist,
            metadata_overrides: Default::default(),
        })?;

        rows.push(SummaryRow {
            dataset: short_dataset_label(&plan.dataset_file),
            day: output.metrics.day,
            tick_count: output.metrics.tick_count,
            own_trade_count: output.metrics.own_trade_count,
            final_pnl_total: output.metrics.final_pnl_total,
            final_pnl_by_product: output.metrics.final_pnl_by_product.clone(),
            run_dir: Some(display_path(&output.run_dir)),
        });
        outputs.push(output);
    }

    let bundle_dir = if args.persist && outputs.len() > 1 {
        Some(write_combined_bundle(
            &output_root,
            &run_id_seed,
            &trader,
            &dataset,
            &rows,
            &outputs,
        )?)
    } else {
        None
    };

    print_summary(
        &rows,
        &trader,
        &dataset,
        args.persist,
        args.products,
        bundle_dir.as_deref(),
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct PlannedRun {
    dataset_file: PathBuf,
    day: Option<i64>,
    run_id: String,
}

#[derive(Debug, Clone)]
struct ResolvedTrader {
    path: PathBuf,
    auto_selected: bool,
}

#[derive(Debug, Clone)]
struct ResolvedDataset {
    roots: Vec<PathBuf>,
    label: String,
    auto_selected: bool,
    exclude_submission_when_day_filtered: bool,
}

#[derive(Debug)]
struct SummaryRow {
    dataset: String,
    day: Option<i64>,
    tick_count: usize,
    own_trade_count: usize,
    final_pnl_total: f64,
    final_pnl_by_product: IndexMap<String, f64>,
    run_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProductMatrixRow {
    product: String,
    values: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProductMatrix {
    columns: Vec<String>,
    rows: Vec<ProductMatrixRow>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
enum ProductDisplayMode {
    Off,
    Summary,
    Full,
}

fn build_run_plan(
    dataset_roots: &[PathBuf],
    requested_day: Option<i64>,
    requested_run_id: Option<&str>,
    exclude_submission_when_day_filtered: bool,
) -> Result<(String, Vec<PlannedRun>)> {
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();

    for dataset_root in dataset_roots {
        let dataset_files = collect_dataset_files(dataset_root)?;
        let input_is_file = dataset_root.is_file();

        for dataset_file in dataset_files {
            if !seen.insert(dataset_file.clone()) {
                continue;
            }
            if exclude_submission_when_day_filtered
                && requested_day.is_some()
                && is_submission_like_path(&dataset_file)
            {
                continue;
            }
            let dataset = load_dataset(&dataset_file)?;
            let days = collect_requested_days(&dataset, requested_day);
            if days.is_empty() {
                if input_is_file {
                    bail!(
                        "day {} not found in dataset {}",
                        requested_day.unwrap_or_default(),
                        display_path(&dataset_file)
                    );
                }
                continue;
            }
            for day in days {
                targets.push((dataset_file.clone(), day));
            }
        }
    }

    if targets.is_empty() {
        let label = dataset_roots
            .first()
            .map(|path| display_path(path))
            .unwrap_or_else(|| "dataset selection".to_string());
        bail!("no runnable datasets found at {label}");
    }

    targets.sort_by(|(left_path, left_day), (right_path, right_day)| {
        dataset_order_key(left_path, *left_day)
            .cmp(&dataset_order_key(right_path, *right_day))
            .then_with(|| left_path.cmp(right_path))
    });

    let run_id_seed = requested_run_id
        .map(ToOwned::to_owned)
        .unwrap_or_else(default_run_id_seed);
    let multiple_runs = targets.len() > 1;

    let plans = targets
        .into_iter()
        .map(|(dataset_file, day)| PlannedRun {
            run_id: if multiple_runs {
                format!("{run_id_seed}-{}", run_suffix(&dataset_file, day))
            } else {
                run_id_seed.clone()
            },
            dataset_file,
            day,
        })
        .collect();

    Ok((run_id_seed, plans))
}

fn collect_dataset_files(dataset_root: &Path) -> Result<Vec<PathBuf>> {
    if dataset_root.is_file() {
        if dataset_candidate_key(dataset_root).is_none() {
            bail!("unsupported dataset file {}", display_path(dataset_root));
        }
        return Ok(vec![dataset_root.to_path_buf()]);
    }
    if !dataset_root.is_dir() {
        bail!(
            "dataset path is not a file or directory: {}",
            dataset_root.display()
        );
    }

    let mut selected: IndexMap<String, (u8, PathBuf)> = IndexMap::new();
    for entry in fs::read_dir(dataset_root).with_context(|| {
        format!(
            "failed to read dataset directory {}",
            dataset_root.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(key) = dataset_candidate_key(&path) else {
            continue;
        };
        let rank = dataset_candidate_rank(&path);
        let replace = match selected.get(&key) {
            None => true,
            Some((best_rank, best_path)) => {
                rank > *best_rank || (rank == *best_rank && path < *best_path)
            }
        };
        if replace {
            selected.insert(key, (rank, path));
        }
    }
    let mut files: Vec<PathBuf> = selected.into_values().map(|(_, path)| path).collect();
    files.sort();

    if files.is_empty() {
        bail!(
            "no supported datasets found in {}",
            display_path(dataset_root)
        );
    }

    Ok(files)
}

fn resolve_trader(requested: Option<&Path>) -> Result<ResolvedTrader> {
    if let Some(path) = requested {
        return Ok(ResolvedTrader {
            path: path
                .canonicalize()
                .with_context(|| format!("failed to resolve trader {}", path.display()))?,
            auto_selected: false,
        });
    }

    let roots = candidate_trader_roots()?;
    for root in roots {
        let candidates = collect_trader_candidates(&root)?;
        if let Some(path) = latest_modified(candidates)? {
            return Ok(ResolvedTrader {
                path,
                auto_selected: true,
            });
        }
    }

    bail!(
        "no trader file found automatically; pass --trader <file.py> or place a Trader class in scripts/ or traders/"
    );
}

fn resolve_dataset_input(requested: Option<&str>) -> Result<ResolvedDataset> {
    let requested = requested.unwrap_or("latest");
    let normalized = requested.to_ascii_lowercase();
    let datasets_root = project_root().join("datasets");
    let latest_round = latest_round_root(&datasets_root)?;

    if let Some(round_name) = normalized.strip_suffix("-submission") {
        let round_root = round_root_for_name(&datasets_root, round_name)?;
        return Ok(ResolvedDataset {
            roots: vec![round_submission_entry(&round_root)?],
            label: format!("{round_name}-sub"),
            auto_selected: false,
            exclude_submission_when_day_filtered: false,
        });
    }

    let resolved = match normalized.as_str() {
        "latest" => ResolvedDataset {
            roots: vec![latest_round.clone()],
            label: short_round_label(&latest_round),
            auto_selected: true,
            exclude_submission_when_day_filtered: true,
        },
        "tutorial" | "tut" | "tutorial-round" | "tut-round" => ResolvedDataset {
            roots: vec![datasets_root.join("tutorial")],
            label: "tutorial".to_string(),
            auto_selected: false,
            exclude_submission_when_day_filtered: true,
        },
        "round1" | "r1" => round_dataset(&datasets_root, "round1")?,
        "round2" | "r2" => round_dataset(&datasets_root, "round2")?,
        "round3" | "r3" => round_dataset(&datasets_root, "round3")?,
        "round4" | "r4" => round_dataset(&datasets_root, "round4")?,
        "round5" | "r5" => round_dataset(&datasets_root, "round5")?,
        "round6" | "r6" => round_dataset(&datasets_root, "round6")?,
        "round7" | "r7" => round_dataset(&datasets_root, "round7")?,
        "round8" | "r8" => round_dataset(&datasets_root, "round8")?,
        "submission" | "tutorial-submission" | "tut-sub" | "sub" => ResolvedDataset {
            roots: vec![round_submission_entry(&latest_round)?],
            label: "tut-sub".to_string(),
            auto_selected: false,
            exclude_submission_when_day_filtered: false,
        },
        "tutorial-1" | "tut-1" | "tut-d-1" => ResolvedDataset {
            roots: vec![round_day_entry(&datasets_root.join("tutorial"), -1)?],
            label: "tut-d-1".to_string(),
            auto_selected: false,
            exclude_submission_when_day_filtered: false,
        },
        "tutorial-2" | "tut-2" | "tut-d-2" => ResolvedDataset {
            roots: vec![round_day_entry(&datasets_root.join("tutorial"), -2)?],
            label: "tut-d-2".to_string(),
            auto_selected: false,
            exclude_submission_when_day_filtered: false,
        },
        _ => {
            let path = PathBuf::from(requested);
            let canonical = path
                .canonicalize()
                .with_context(|| format!("failed to resolve dataset {}", path.display()))?;
            ResolvedDataset {
                label: short_dataset_label(&canonical),
                roots: vec![canonical],
                auto_selected: false,
                exclude_submission_when_day_filtered: false,
            }
        }
    };

    Ok(resolved)
}

fn round_dataset(datasets_root: &Path, round: &str) -> Result<ResolvedDataset> {
    let root = round_root_for_name(datasets_root, round)?;
    let has_submission = round_submission_entry(&root).is_ok();
    if !root.is_dir() {
        bail!("dataset round not found: {}", root.display());
    }
    Ok(ResolvedDataset {
        roots: vec![root],
        label: round.to_string(),
        auto_selected: false,
        exclude_submission_when_day_filtered: has_submission,
    })
}

fn round_root_for_name(datasets_root: &Path, round: &str) -> Result<PathBuf> {
    let root = datasets_root.join(round);
    if !root.is_dir() {
        bail!("dataset round not found: {}", root.display());
    }
    Ok(root)
}

fn latest_round_root(datasets_root: &Path) -> Result<PathBuf> {
    let mut candidates = vec![datasets_root.join("tutorial")];
    candidates.extend((1..=8).map(|index| datasets_root.join(format!("round{index}"))));

    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| {
            fs::read_dir(path)
                .ok()
                .into_iter()
                .flat_map(|entries| entries.flatten())
                .any(|entry| dataset_candidate_key(&entry.path()).is_some())
        })
        .last()
        .ok_or_else(|| anyhow::anyhow!("no populated round directories found under datasets/"))
}

fn short_round_label(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("latest-round")
        .to_string()
}

fn collect_requested_days(
    dataset: &crate::model::NormalizedDataset,
    requested_day: Option<i64>,
) -> Vec<Option<i64>> {
    let days: BTreeSet<i64> = dataset.ticks.iter().filter_map(|tick| tick.day).collect();
    if let Some(day) = requested_day {
        if days.contains(&day) {
            return vec![Some(day)];
        }
        return Vec::new();
    }
    if days.is_empty() {
        vec![None]
    } else {
        days.into_iter().map(Some).collect()
    }
}

fn default_run_id_seed() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("backtest-{millis}")
}

fn run_suffix(dataset_file: &Path, day: Option<i64>) -> String {
    let stem = dataset_stem_label(dataset_file);
    let day_label = day
        .map(|value| format!("day-{value}"))
        .unwrap_or_else(|| "all".to_string());
    sanitize_identifier(&format!("{}-{day_label}", stem))
}

fn sanitize_identifier(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn candidate_trader_roots() -> Result<Vec<PathBuf>> {
    let project = project_root();
    let mut roots = Vec::new();

    for base in [project] {
        for relative in ["scripts", "traders/submissions", "traders"] {
            let candidate = base.join(relative);
            if candidate.is_dir() && !roots.iter().any(|existing| existing == &candidate) {
                roots.push(candidate);
            }
        }
    }

    Ok(roots)
}

fn collect_trader_candidates(root: &Path) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read trader directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "py") {
                continue;
            }
            if looks_like_trader_file(&path)? {
                candidates.push(path);
            }
        }
    }
    Ok(candidates)
}

fn looks_like_trader_file(path: &Path) -> Result<bool> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read trader candidate {}", path.display()))?;
    Ok(text.contains("class Trader"))
}

fn latest_modified(paths: Vec<PathBuf>) -> Result<Option<PathBuf>> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in paths {
        let modified = fs::metadata(&path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?
            .modified()
            .with_context(|| format!("failed to read modified time for {}", path.display()))?;
        match &best {
            Some((best_time, _best_path)) if modified < *best_time => {}
            Some((best_time, best_path)) if modified == *best_time && path >= *best_path => {}
            _ => best = Some((modified, path)),
        }
    }
    Ok(best.map(|(_, path)| path))
}

fn short_dataset_label(path: &Path) -> String {
    match day_key_from_path(path).as_deref() {
        Some("day_-1") => "D-1".to_string(),
        Some("day_-2") => "D-2".to_string(),
        _ if is_submission_like_path(path) => "SUB".to_string(),
        _ => shorten_identifier(&dataset_stem_label(path).replace('_', "-"), 20),
    }
}

fn shorten_identifier(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        return value.to_string();
    }
    value.chars().take(max_len).collect()
}

fn dataset_order_key(path: &Path, day: Option<i64>) -> (i32, String) {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if is_submission_like_path(path) {
        return (10_000, file_name.to_string());
    }
    if let Some(day) = day {
        return (day as i32, file_name.to_string());
    }
    (0, file_name.to_string())
}

fn round_day_entry(round_root: &Path, day: i64) -> Result<PathBuf> {
    let wanted = format!("day_{day}");
    collect_dataset_files(round_root)?
        .into_iter()
        .find(|path| day_key_from_path(path).as_deref() == Some(wanted.as_str()))
        .with_context(|| {
            format!(
                "day {day} dataset not found in {}",
                display_path(round_root)
            )
        })
}

fn round_submission_entry(round_root: &Path) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = collect_dataset_files(round_root)?
        .into_iter()
        .filter(|path| !is_day_dataset_path(path))
        .collect();
    candidates.sort_by(|left, right| {
        submission_candidate_rank(right)
            .cmp(&submission_candidate_rank(left))
            .then_with(|| left.cmp(right))
    });
    candidates.into_iter().next().with_context(|| {
        format!(
            "submission dataset not found in {}",
            display_path(round_root)
        )
    })
}

fn dataset_candidate_key(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if is_trades_csv_name(&name) {
        return None;
    }
    if is_prices_csv_name(&name) {
        return day_key_from_name(&name).or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        });
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("log") | Some("json") => path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string()),
        _ => None,
    }
}

fn dataset_candidate_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".log") {
        return 3;
    }
    if is_prices_csv_name(&name) {
        return 2;
    }
    if name.ends_with(".json") {
        return 1;
    }
    0
}

fn dataset_stem_label(path: &Path) -> String {
    if let Some(day_key) = day_key_from_path(path) {
        return day_key;
    }
    if is_submission_like_path(path) {
        return "submission".to_string();
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("dataset")
        .to_string()
}

fn day_key_from_path(path: &Path) -> Option<String> {
    day_key_from_name(path.file_name()?.to_str()?)
}

fn day_key_from_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let start = lower.find("day_")?;
    let suffix = &lower[start + 4..];
    let end = suffix
        .find(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .unwrap_or(suffix.len());
    let token = &suffix[..end];
    if token.is_empty() {
        return None;
    }
    Some(format!("day_{token}"))
}

fn is_day_dataset_path(path: &Path) -> bool {
    day_key_from_path(path).is_some()
}

fn is_submission_like_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.ends_with(".log") || name.contains("submission")
}

fn submission_candidate_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "submission.log" => 4,
        "submission.json" => 3,
        _ if name.ends_with(".log") => 2,
        _ => 1,
    }
}

fn is_prices_csv_name(name: &str) -> bool {
    name.starts_with("prices_") && name.ends_with(".csv")
}

fn is_trades_csv_name(name: &str) -> bool {
    name.starts_with("trades_") && name.ends_with(".csv")
}

fn short_trader_label(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| display_path(path))
}

fn print_summary(
    rows: &[SummaryRow],
    trader: &ResolvedTrader,
    dataset: &ResolvedDataset,
    persist: bool,
    products: ProductDisplayMode,
    bundle_dir: Option<&str>,
) {
    println!(
        "trader: {}{}",
        short_trader_label(&trader.path),
        if trader.auto_selected { " [auto]" } else { "" }
    );
    println!(
        "dataset: {}{}",
        dataset.label,
        if dataset.auto_selected {
            " [default]"
        } else {
            ""
        }
    );
    println!("mode: fast");
    println!("artifacts: {}", if persist { "saved" } else { "log-only" });
    if let Some(bundle_dir) = bundle_dir {
        println!("bundle: {bundle_dir}");
    }
    println!(
        "{:<12} {:>6} {:>8} {:>11} {:>12}  RUN_DIR",
        "SET", "DAY", "TICKS", "OWN_TRADES", "FINAL_PNL"
    );
    for row in rows {
        println!(
            "{:<12} {:>6} {:>8} {:>11} {:>12.2}  {}",
            row.dataset,
            render_day(row.day),
            row.tick_count,
            row.own_trade_count,
            row.final_pnl_total,
            row.run_dir.as_deref().unwrap_or("-")
        );
    }
    print_product_table(rows, products);
}

fn render_day(day: Option<i64>) -> String {
    day.map(|value| value.to_string())
        .unwrap_or_else(|| "all".to_string())
}

fn print_product_table(rows: &[SummaryRow], mode: ProductDisplayMode) {
    let matrix = build_product_matrix(rows, mode);
    if matrix.rows.is_empty() {
        return;
    }

    let product_width = matrix
        .rows
        .iter()
        .map(|row| row.product.len())
        .max()
        .unwrap_or(7)
        .max("PRODUCT".len());
    let column_widths: Vec<usize> = matrix
        .columns
        .iter()
        .map(|label| label.len().max(10))
        .collect();

    println!();
    print!("{:<product_width$}", "PRODUCT");
    for (label, width) in matrix.columns.iter().zip(&column_widths) {
        print!(" {:>width$}", label, width = *width);
    }
    println!();

    for row in matrix.rows {
        print!("{:<product_width$}", row.product);
        for (value, width) in row.values.iter().zip(&column_widths) {
            print!(" {:>width$.2}", value, width = *width);
        }
        println!();
    }
}

fn write_combined_bundle(
    output_root: &Path,
    run_id_seed: &str,
    trader: &ResolvedTrader,
    dataset: &ResolvedDataset,
    rows: &[SummaryRow],
    outputs: &[crate::model::RunOutput],
) -> Result<String> {
    let bundle_dir = output_root.join(run_id_seed);
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("failed to create bundle directory {}", bundle_dir.display()))?;

    let submission_log = merge_submission_logs(run_id_seed, outputs)?;
    fs::write(bundle_dir.join("submission.log"), submission_log).with_context(|| {
        format!(
            "failed to write combined submission.log in {}",
            bundle_dir.display()
        )
    })?;

    let combined_log = merge_combined_logs(rows, outputs);
    fs::write(bundle_dir.join("combined.log"), combined_log)
        .with_context(|| format!("failed to write combined.log in {}", bundle_dir.display()))?;

    let summary_json = build_bundle_manifest(run_id_seed, trader, dataset, rows)?;
    fs::write(bundle_dir.join("manifest.json"), summary_json)
        .with_context(|| format!("failed to write manifest.json in {}", bundle_dir.display()))?;

    Ok(display_path(&bundle_dir))
}

fn merge_submission_logs(run_id: &str, outputs: &[crate::model::RunOutput]) -> Result<Vec<u8>> {
    let mut header: Option<String> = None;
    let mut activity_lines = Vec::new();
    let mut logs = Vec::new();
    let mut trade_history = Vec::new();

    for output in outputs {
        let artifacts = output
            .artifacts
            .as_ref()
            .context("combined bundle requires materialized artifacts")?;
        let payload: Value = serde_json::from_slice(&artifacts.submission_log)
            .context("failed to parse child submission.log")?;
        let object = payload
            .as_object()
            .context("child submission.log should be a JSON object")?;

        if let Some(text) = object.get("activitiesLog").and_then(Value::as_str) {
            let mut lines = text.lines();
            if let Some(first) = lines.next() {
                if !first.is_empty() && header.is_none() {
                    header = Some(first.to_string());
                }
            }
            activity_lines.extend(lines.filter(|line| !line.is_empty()).map(ToOwned::to_owned));
        }

        logs.extend(
            object
                .get("logs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        trade_history.extend(
            object
                .get("tradeHistory")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }

    let mut activities = Vec::new();
    if let Some(header) = header {
        activities.push(header);
    }
    activities.extend(activity_lines);

    pretty_json_bytes(&object(vec![
        ("submissionId", Value::String(run_id.to_string())),
        ("activitiesLog", Value::String(activities.join("\n"))),
        ("logs", Value::Array(logs)),
        ("tradeHistory", Value::Array(trade_history)),
    ]))
}

fn merge_combined_logs(rows: &[SummaryRow], outputs: &[crate::model::RunOutput]) -> Vec<u8> {
    let mut out = String::new();
    for (index, (row, output)) in rows.iter().zip(outputs).enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("=== {} ===\n", product_column_label(row)));
        if let Some(artifacts) = &output.artifacts {
            out.push_str(&String::from_utf8_lossy(&artifacts.combined_log));
        }
    }
    out.into_bytes()
}

fn build_bundle_manifest(
    run_id_seed: &str,
    trader: &ResolvedTrader,
    dataset: &ResolvedDataset,
    rows: &[SummaryRow],
) -> Result<Vec<u8>> {
    let rows_json = rows
        .iter()
        .map(|row| {
            object(vec![
                ("set", Value::String(row.dataset.clone())),
                (
                    "day",
                    row.day
                        .map(|value| Value::Number(value.into()))
                        .unwrap_or(Value::Null),
                ),
                ("tick_count", Value::Number((row.tick_count as u64).into())),
                (
                    "own_trade_count",
                    Value::Number((row.own_trade_count as u64).into()),
                ),
                (
                    "final_pnl_total",
                    serde_json::Number::from_f64(row.final_pnl_total)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                ),
                (
                    "run_dir",
                    row.run_dir
                        .as_ref()
                        .map(|value| Value::String(value.clone()))
                        .unwrap_or(Value::Null),
                ),
            ])
        })
        .collect();

    pretty_json_bytes(&object(vec![
        ("run_id", Value::String(run_id_seed.to_string())),
        (
            "trader",
            Value::String(short_trader_label(&trader.path).to_string()),
        ),
        ("dataset", Value::String(dataset.label.clone())),
        ("results", Value::Array(rows_json)),
    ]))
}

fn build_product_matrix(rows: &[SummaryRow], mode: ProductDisplayMode) -> ProductMatrix {
    if mode == ProductDisplayMode::Off {
        return ProductMatrix {
            columns: Vec::new(),
            rows: Vec::new(),
        };
    }

    let columns: Vec<String> = rows.iter().map(product_column_label).collect();
    let mut product_map: IndexMap<String, Vec<f64>> = IndexMap::new();
    for (col_idx, row) in rows.iter().enumerate() {
        for (product, pnl) in &row.final_pnl_by_product {
            let key = short_product_label(product).to_string();
            let entry = product_map
                .entry(key)
                .or_insert_with(|| vec![0.0; rows.len()]);
            entry[col_idx] = *pnl;
        }
    }

    let mut ranked: Vec<(String, Vec<f64>, f64)> = product_map
        .into_iter()
        .map(|(product, values)| {
            let max_abs = values.iter().map(|value| value.abs()).fold(0.0, f64::max);
            (product, values, max_abs)
        })
        .collect();
    ranked.sort_by(|(left_name, _, left_abs), (right_name, _, right_abs)| {
        right_abs
            .partial_cmp(left_abs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left_name.cmp(right_name))
    });

    let matrix_rows = match mode {
        ProductDisplayMode::Off => Vec::new(),
        ProductDisplayMode::Full => ranked
            .into_iter()
            .map(|(product, values, _)| ProductMatrixRow { product, values })
            .collect(),
        ProductDisplayMode::Summary => {
            let shown_count = ranked.len().min(6);
            let mut out: Vec<ProductMatrixRow> = ranked
                .iter()
                .take(shown_count)
                .map(|(product, values, _)| ProductMatrixRow {
                    product: product.clone(),
                    values: values.clone(),
                })
                .collect();
            let remaining = &ranked[shown_count..];
            if !remaining.is_empty() {
                let mut values = vec![0.0; columns.len()];
                for (_, row_values, _) in remaining {
                    for (index, value) in row_values.iter().enumerate() {
                        values[index] += value;
                    }
                }
                out.push(ProductMatrixRow {
                    product: format!("OTHER(+{})", remaining.len()),
                    values,
                });
            }
            out
        }
    };

    ProductMatrix {
        columns,
        rows: matrix_rows,
    }
}

fn product_column_label(row: &SummaryRow) -> String {
    row.dataset.clone()
}

fn short_product_label(product: &str) -> &'static str {
    match product {
        "EMERALDS" => "EMR",
        "TOMATOES" => "TOM",
        "RAINFOREST_RESIN" => "RESIN",
        "KELP" => "KELP",
        "SQUID_INK" => "SQUID",
        "CROISSANTS" => "CROISS",
        "JAMS" => "JAMS",
        "DJEMBES" => "DJEMBE",
        "PICNIC_BASKET1" => "PB1",
        "PICNIC_BASKET2" => "PB2",
        "VOLCANIC_ROCK" => "ROCK",
        "VOLCANIC_ROCK_VOUCHER_9500" => "V9500",
        "VOLCANIC_ROCK_VOUCHER_9750" => "V9750",
        "VOLCANIC_ROCK_VOUCHER_10000" => "V10000",
        "VOLCANIC_ROCK_VOUCHER_10250" => "V10250",
        "VOLCANIC_ROCK_VOUCHER_10500" => "V10500",
        "MAGNIFICENT_MACARONS" => "MACARON",
        _ => Box::leak(product.to_string().into_boxed_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProductDisplayMode, ProductMatrix, ProductMatrixRow, SummaryRow, build_product_matrix,
        collect_requested_days, merge_submission_logs, resolve_dataset_input, run_suffix,
        short_dataset_label,
    };
    use crate::model::{ArtifactSet, MatchingConfig, RunMetrics, RunOutput, load_dataset};
    use crate::runner::project_root;
    use indexmap::IndexMap;
    use std::path::PathBuf;

    #[test]
    fn product_split_formats_compact_table() {
        let row = SummaryRow {
            dataset: "SUB".to_string(),
            day: Some(-1),
            tick_count: 0,
            own_trade_count: 0,
            final_pnl_total: 9.25,
            final_pnl_by_product: {
                let mut values = IndexMap::new();
                values.insert("VOLCANIC_ROCK".to_string(), 12.5);
                values.insert("VOLCANIC_ROCK_VOUCHER_9500".to_string(), -3.25);
                values
            },
            run_dir: None,
        };
        assert_eq!(
            build_product_matrix(&[row], ProductDisplayMode::Summary),
            ProductMatrix {
                columns: vec!["SUB".to_string()],
                rows: vec![
                    ProductMatrixRow {
                        product: "ROCK".to_string(),
                        values: vec![12.5],
                    },
                    ProductMatrixRow {
                        product: "V9500".to_string(),
                        values: vec![-3.25],
                    },
                ],
            }
        );
    }

    #[test]
    fn product_split_summarizes_long_lists() {
        let row = SummaryRow {
            dataset: "D-1".to_string(),
            day: Some(-1),
            tick_count: 0,
            own_trade_count: 0,
            final_pnl_total: 5.5,
            final_pnl_by_product: {
                let mut values = IndexMap::new();
                values.insert("EMERALDS".to_string(), 1.0);
                values.insert("TOMATOES".to_string(), 2.0);
                values.insert("VOLCANIC_ROCK".to_string(), 5.0);
                values.insert("VOLCANIC_ROCK_VOUCHER_9500".to_string(), -3.0);
                values.insert("VOLCANIC_ROCK_VOUCHER_9750".to_string(), 0.5);
                values
            },
            run_dir: None,
        };
        assert_eq!(
            build_product_matrix(&[row], ProductDisplayMode::Summary),
            ProductMatrix {
                columns: vec!["D-1".to_string()],
                rows: vec![
                    ProductMatrixRow {
                        product: "ROCK".to_string(),
                        values: vec![5.0],
                    },
                    ProductMatrixRow {
                        product: "V9500".to_string(),
                        values: vec![-3.0],
                    },
                    ProductMatrixRow {
                        product: "TOM".to_string(),
                        values: vec![2.0],
                    },
                    ProductMatrixRow {
                        product: "EMR".to_string(),
                        values: vec![1.0],
                    },
                    ProductMatrixRow {
                        product: "V9750".to_string(),
                        values: vec![0.5],
                    },
                ],
            }
        );
    }

    #[test]
    fn product_split_off_hides_table() {
        let row = SummaryRow {
            dataset: "SUB".to_string(),
            day: Some(-1),
            tick_count: 0,
            own_trade_count: 0,
            final_pnl_total: 0.0,
            final_pnl_by_product: IndexMap::new(),
            run_dir: None,
        };
        assert!(
            build_product_matrix(&[row], ProductDisplayMode::Off)
                .rows
                .is_empty()
        );
    }

    #[test]
    fn tutorial_dataset_returns_single_day() {
        let dataset =
            load_dataset(&project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"))
                .expect("dataset should load");
        assert_eq!(collect_requested_days(&dataset, None), vec![Some(-1)]);
    }

    #[test]
    fn run_suffix_includes_dataset_and_day() {
        let suffix = run_suffix(
            std::path::Path::new("datasets/tutorial/prices_round_0_day_-2.csv"),
            Some(-2),
        );
        assert_eq!(suffix, "day-2-day-2");
    }

    #[test]
    fn dataset_alias_defaults_to_latest_round() {
        let dataset = resolve_dataset_input(None).expect("dataset should resolve");
        assert_eq!(dataset.label, "tutorial");
        assert_eq!(dataset.roots.len(), 1);
        assert!(dataset.auto_selected);
    }

    #[test]
    fn short_label_uses_known_aliases() {
        let label = short_dataset_label(std::path::Path::new("submission.json"));
        assert_eq!(label, "SUB");
    }

    #[test]
    fn merged_submission_log_combines_rows_without_repeating_header() {
        let outputs = vec![
            fake_output(
                br#"{
  "submissionId": "one",
  "activitiesLog": "hdr\na1",
  "logs": ["l1"],
  "tradeHistory": [{"symbol":"EMERALDS"}]
}"#,
            ),
            fake_output(
                br#"{
  "submissionId": "two",
  "activitiesLog": "hdr\na2",
  "logs": ["l2"],
  "tradeHistory": [{"symbol":"TOMATOES"}]
}"#,
            ),
        ];

        let merged = merge_submission_logs("bundle-1", &outputs).expect("merge should succeed");
        let value: serde_json::Value =
            serde_json::from_slice(&merged).expect("merged JSON should parse");

        assert_eq!(value["submissionId"], "bundle-1");
        assert_eq!(value["activitiesLog"], "hdr\na1\na2");
        assert_eq!(value["logs"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["tradeHistory"].as_array().map(Vec::len), Some(2));
    }

    fn fake_output(submission_log: &[u8]) -> RunOutput {
        RunOutput {
            run_id: "child".to_string(),
            run_dir: PathBuf::from("runs/child"),
            metrics: RunMetrics {
                run_id: "child".to_string(),
                dataset_id: "dataset".to_string(),
                dataset_path: "datasets/tutorial/prices_round_0_day_-1.csv".to_string(),
                trader_path: "traders/latest_trader.py".to_string(),
                day: Some(-1),
                matching: MatchingConfig::default(),
                tick_count: 1,
                own_trade_count: 0,
                final_pnl_total: 0.0,
                final_pnl_by_product: IndexMap::new(),
                generated_at: "2026-03-21T00:00:00Z".to_string(),
            },
            result_json: Vec::new(),
            artifacts: Some(ArtifactSet {
                metrics_json: Vec::new(),
                bundle_json: Vec::new(),
                submission_log: submission_log.to_vec(),
                activity_csv: Vec::new(),
                pnl_by_product_csv: Vec::new(),
                combined_log: Vec::new(),
                trades_csv: Vec::new(),
            }),
        }
    }
}
