//! POKER GOAT 솔버 래퍼 CLI (설계서 §4.5)

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

use pokergoat_solver::blob::BlobHeader;
use pokergoat_solver::config::JobConfig;
use pokergoat_solver::export::{ExportOptions, DEFAULT_PRUNE_EPSILON};
use pokergoat_solver::{
    aggregate, blob, cards, export, solve, tree, validate, ENGINE_REV, SOLVER_VERSION,
};

#[derive(Parser)]
#[command(
    name = "pokergoat-solver",
    version,
    about = "POKER GOAT GTO 솔버 래퍼 (postflop-solver 기반, AGPL-3.0-or-later)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 트리 노드 수와 메모리 사용량을 추정해 JSON으로 출력한다
    Estimate {
        #[arg(long)]
        config: PathBuf,
    },
    /// 솔브하고 out 디렉토리에 manifest와 blob을 쓴다
    Solve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// rayon 스레드 수 (기본: 코어 수)
        #[arg(long)]
        threads: Option<usize>,
        /// 초 단위 시간 상한. 넘으면 그때까지의 전략으로 마감한다
        #[arg(long = "time-limit")]
        time_limit: Option<f64>,
        /// brotli 압축 없이 .bin 그대로 쓴다 (디버깅용)
        #[arg(long = "no-compress")]
        no_compress: bool,
        /// 노드락 (자리만 있고 아직 구현하지 않았다, §4.6)
        #[arg(long)]
        lock: Option<String>,
    },
    /// 라인 프리픽스별 애그리게이트 리포트 (§6.4, 미구현)
    Aggregate {
        #[arg(long = "scenario-dir")]
        scenario_dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// §12.2 토이 게임 자가 점검
    Validate {
        /// 최대 반복
        #[arg(long, default_value_t = 1000)]
        iterations: u32,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("오류: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Estimate { config } => {
            estimate(&config)?;
            Ok(0)
        }
        Command::Solve {
            config,
            out,
            threads,
            time_limit,
            no_compress,
            lock,
        } => {
            if lock.is_some() {
                bail!("--lock은 아직 구현하지 않았다 (§4.6)");
            }
            solve_job(&config, &out, threads, time_limit, !no_compress)?;
            Ok(0)
        }
        Command::Aggregate { scenario_dir, out } => {
            aggregate::run(&scenario_dir, &out)?;
            Ok(0)
        }
        Command::Validate { iterations } => {
            let result = validate::run_toy_game(iterations)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(if result.pass { 0 } else { 1 })
        }
    }
}

fn set_threads(threads: Option<usize>) -> Result<()> {
    if let Some(n) = threads {
        if n == 0 {
            bail!("--threads는 1 이상이어야 한다");
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .context("rayon 스레드 풀 설정 실패")?;
    }
    Ok(())
}

fn estimate(config_path: &PathBuf) -> Result<()> {
    let config = JobConfig::from_path(config_path)?;
    let street = config.street()?;
    let card_config = tree::build_card_config(&config)?;
    let isomorphism = tree::Isomorphism::new(&card_config);
    let fixed_turn = if card_config.turn == postflop_solver::NOT_DEALT {
        None
    } else {
        Some(card_config.turn)
    };

    let (mut action_tree, warnings) = tree::build_action_tree(&config)?;
    let counts = tree::count_nodes(&mut action_tree, &isomorphism, fixed_turn)?;

    let built = tree::build_game(&config)?;
    let (uncompressed, compressed) = built.game.memory_usage();
    let hands = [
        built.game.private_cards(0).len(),
        built.game.private_cards(1).len(),
    ];

    let turn_reps = if fixed_turn.is_none() {
        isomorphism.turn_representatives().len()
    } else {
        0
    };

    let output = json!({
        "board": cards::board_to_string(&config.board_cards()?)?,
        "street": street.as_str(),
        "nodeCount": counts.total,
        "nodeBreakdown": {
            "player": counts.player,
            "chance": counts.chance,
            "terminal": counts.terminal,
            "actionTree": counts.action_tree,
        },
        "memoryBytes": {
            "uncompressed": uncompressed,
            "compressed": compressed,
        },
        "handsPerPlayer": hands,
        "turnRepresentatives": turn_reps,
        "storeRiver": config.store_river,
        "solverVersion": SOLVER_VERSION,
        "warnings": warnings,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn solve_job(
    config_path: &PathBuf,
    out_dir: &PathBuf,
    threads: Option<usize>,
    time_limit: Option<f64>,
    compress: bool,
) -> Result<()> {
    set_threads(threads)?;
    let config = JobConfig::from_path(config_path)?;
    let street = config.street()?;
    let mut built = tree::build_game(&config)?;
    for warning in &built.warnings {
        eprintln!("경고: {warning}");
    }

    let (uncompressed, _) = built.game.memory_usage();
    eprintln!(
        "메모리 예상 {:.2}GB, 핸드 OOP {} / IP {}",
        uncompressed as f64 / (1024.0 * 1024.0 * 1024.0),
        built.game.private_cards(0).len(),
        built.game.private_cards(1).len()
    );

    built.game.allocate_memory(false);
    let outcome = solve::run_solver(
        &mut built.game,
        config.iterations,
        config.target_exploitability_chips(),
        time_limit,
        true,
    )?;

    let exploitability_pct =
        outcome.exploitability as f64 / config.pot_chips() as f64 * 100.0;

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("출력 디렉토리 생성 실패: {}", out_dir.display()))?;

    let board = config.board_cards()?;
    let header = BlobHeader {
        scenario_id: config.scenario_id,
        template_id: config.template_id,
        solver_version: SOLVER_VERSION,
        board: board.clone(),
        street: street.as_u8(),
        starting_pot: config.pot as f32,
        effective_stack: config.effective_stack as f32,
        rake_percent: config.rake.as_ref().map_or(0.0, |r| r.percent) as f32,
        rake_cap: config.rake.as_ref().map_or(0.0, |r| r.cap_bb) as f32,
        exploitability_pct: exploitability_pct as f32,
        iterations: outcome.iterations,
    };

    let summary = export::export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: config.store_river,
            prune_epsilon: DEFAULT_PRUNE_EPSILON,
            compress,
            out_dir: out_dir.clone(),
        },
    )?;

    let mut warnings = built.warnings.clone();
    warnings.extend(summary.warnings.iter().cloned());
    if outcome.hit_time_limit {
        warnings.push(format!(
            "시간 상한 {}초에 걸려 목표 정확도 전에 마감했다",
            time_limit.unwrap_or_default()
        ));
    }

    let manifest = json!({
        "formatVersion": blob::FORMAT_VERSION,
        "solverVersion": SOLVER_VERSION,
        "engine": format!("postflop-solver@{ENGINE_REV}"),
        "cliVersion": env!("CARGO_PKG_VERSION"),
        "scenario": config.scenario,
        "scenarioId": config.scenario_id,
        "template": config.template,
        "templateId": config.template_id,
        "board": cards::board_to_string(&board)?,
        "boardCards": board,
        "street": street.as_str(),
        "startingPot": config.pot,
        "effectiveStack": config.effective_stack,
        "rake": config.rake,
        "storeRiver": config.store_river,
        "iterations": outcome.iterations,
        "maxIterations": config.iterations,
        "exploitability": outcome.exploitability,
        "exploitabilityPctPot": exploitability_pct,
        "targetExploitabilityPctPot": config.target_exploitability,
        "elapsedSec": outcome.elapsed_sec,
        "memoryBytes": uncompressed,
        "nodeCount": summary.node_count,
        "prunedNodes": summary.pruned_nodes,
        "pruneEpsilon": DEFAULT_PRUNE_EPSILON,
        "hands": [
            built.game.private_cards(0).len(),
            built.game.private_cards(1).len()
        ],
        "compression": if compress { "brotli-9" } else { "none" },
        "turnCards": summary.turn_cards,
        "turnIsomorphism": summary.turn_isomorphism,
        "bytesRaw": summary.bytes_raw,
        "bytesStored": summary.bytes_stored,
        "files": summary.files,
        "warnings": warnings,
    });
    export::write_manifest(&out_dir.join("manifest.json"), &manifest)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}
