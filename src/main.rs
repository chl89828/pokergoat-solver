//! POKER GOAT 솔버 래퍼 CLI (설계서 §4.5)

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

use pokergoat_solver::blob::BlobHeader;
use pokergoat_solver::config::JobConfig;
use pokergoat_solver::export::{ExportOptions, DEFAULT_PRUNE_EPSILON, DEFAULT_RIVER_EV};
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
        /// 엔진 메모리 모드: auto(8GiB 초과 시 압축) / full / compressed
        #[arg(long = "memory", default_value = "auto")]
        memory: String,
        /// 리버 노드 전용 프룬 도달 임계값. 없으면 잡 JSON의 riverPruneReach,
        /// 그것도 없으면 일반 프룬 임계값과 같다
        #[arg(long = "river-prune-reach")]
        river_prune_reach: Option<f64>,
        /// 리버 플레이어 노드에 EV를 담을지 (on / off). 없으면 잡 JSON의 riverEv, 기본 on
        #[arg(long = "river-ev", value_parser = parse_on_off)]
        river_ev: Option<bool>,
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
            memory,
            river_prune_reach,
            river_ev,
            lock,
        } => {
            if lock.is_some() {
                bail!("--lock은 아직 구현하지 않았다 (§4.6)");
            }
            let memory_mode = MemoryMode::parse(&memory)?;
            if let Some(value) = river_prune_reach {
                if !(0.0..=1.0).contains(&value) {
                    bail!("--river-prune-reach는 0..1 범위여야 한다: {value}");
                }
            }
            solve_job(
                &config,
                &out,
                threads,
                time_limit,
                !no_compress,
                memory_mode,
                river_prune_reach,
                river_ev,
            )?;
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

/// `--river-ev on|off`. 헷갈릴 여지를 줄이려고 true/false와 1/0도 받는다.
fn parse_on_off(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("on 또는 off여야 한다: {other}")),
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

/// 엔진 리그렛·전략 누적 저장 방식. compressed는 i16 + 스케일로 메모리를 절반으로 줄인다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryMode {
    Auto,
    Full,
    Compressed,
}

impl MemoryMode {
    const AUTO_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

    fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "full" => Ok(Self::Full),
            "compressed" => Ok(Self::Compressed),
            other => bail!("--memory 값이 잘못됐다: {other} (auto / full / compressed)"),
        }
    }

    fn use_compressed(self, uncompressed_bytes: u64) -> bool {
        match self {
            Self::Auto => uncompressed_bytes > Self::AUTO_THRESHOLD_BYTES,
            Self::Full => false,
            Self::Compressed => true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn solve_job(
    config_path: &PathBuf,
    out_dir: &PathBuf,
    threads: Option<usize>,
    time_limit: Option<f64>,
    compress: bool,
    memory_mode: MemoryMode,
    river_prune_reach: Option<f64>,
    river_ev: Option<bool>,
) -> Result<()> {
    set_threads(threads)?;
    let config = JobConfig::from_path(config_path)?;
    let street = config.street()?;
    let mut built = tree::build_game(&config)?;
    for warning in &built.warnings {
        eprintln!("경고: {warning}");
    }

    let (uncompressed, compressed_bytes) = built.game.memory_usage();
    let use_compressed = memory_mode.use_compressed(uncompressed);
    eprintln!(
        "메모리 예상 {:.2}GB (압축 시 {:.2}GB, 모드 {}), 핸드 OOP {} / IP {}",
        uncompressed as f64 / (1024.0 * 1024.0 * 1024.0),
        compressed_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        if use_compressed { "compressed" } else { "full" },
        built.game.private_cards(0).len(),
        built.game.private_cards(1).len()
    );

    built.game.allocate_memory(use_compressed);
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

    // CLI 플래그가 잡 JSON보다 우선한다. 둘 다 없으면 예전과 같은 동작.
    let river_prune_epsilon = river_prune_reach
        .or(config.river_prune_reach)
        .unwrap_or(DEFAULT_PRUNE_EPSILON);
    let river_ev = river_ev.or(config.river_ev).unwrap_or(DEFAULT_RIVER_EV);
    eprintln!(
        "export 시작 (리버 프룬 {river_prune_epsilon}, 리버 EV {})",
        if river_ev { "on" } else { "off" }
    );

    let export_started = std::time::Instant::now();
    let summary = export::export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: config.store_river,
            prune_epsilon: DEFAULT_PRUNE_EPSILON,
            river_prune_epsilon,
            river_ev,
            compress,
            out_dir: out_dir.clone(),
        },
    )?;
    let export_sec = export_started.elapsed().as_secs_f64();
    eprintln!(
        "export 완료 {export_sec:.1}s (그중 직렬화·압축·쓰기 {:.1}s), 파일 {}개, 노드 {}",
        summary.write_sec,
        summary.files.len(),
        summary.node_count
    );

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
        "riverPruneReach": river_prune_epsilon,
        "riverEv": river_ev,
        "riverPlayerNodes": summary.river_player_nodes,
        "exportSec": export_sec,
        "exportWriteSec": summary.write_sec,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn solve_flags(argv: &[&str]) -> (Option<f64>, Option<bool>) {
        let cli = Cli::try_parse_from(argv).expect("파싱 실패");
        match cli.command {
            Command::Solve {
                river_prune_reach,
                river_ev,
                ..
            } => (river_prune_reach, river_ev),
            _ => panic!("solve 서브커맨드가 아니다"),
        }
    }

    const BASE: [&str; 6] = [
        "pokergoat-solver",
        "solve",
        "--config",
        "job.json",
        "--out",
        "out",
    ];

    #[test]
    fn river_flags_default_to_none() {
        assert_eq!(solve_flags(&BASE), (None, None));
    }

    #[test]
    fn river_flags_parse() {
        let mut argv = BASE.to_vec();
        argv.extend(["--river-prune-reach", "0.001", "--river-ev", "off"]);
        let (reach, ev) = solve_flags(&argv);
        assert_eq!(reach, Some(0.001));
        assert_eq!(ev, Some(false));

        let mut argv = BASE.to_vec();
        argv.extend(["--river-ev", "on"]);
        assert_eq!(solve_flags(&argv).1, Some(true));
    }

    #[test]
    fn river_ev_rejects_garbage() {
        let mut argv = BASE.to_vec();
        argv.extend(["--river-ev", "maybe"]);
        assert!(Cli::try_parse_from(&argv).is_err());
    }

    #[test]
    fn on_off_accepts_common_spellings() {
        for value in ["on", "ON", "true", "1", "yes"] {
            assert_eq!(parse_on_off(value), Ok(true), "{value}");
        }
        for value in ["off", "OFF", "false", "0", "no"] {
            assert_eq!(parse_on_off(value), Ok(false), "{value}");
        }
        assert!(parse_on_off("").is_err());
        assert!(parse_on_off("nope").is_err());
    }

    #[test]
    fn memory_mode_parses() {
        assert_eq!(MemoryMode::parse("auto").unwrap(), MemoryMode::Auto);
        assert_eq!(MemoryMode::parse("full").unwrap(), MemoryMode::Full);
        assert!(MemoryMode::parse("half").is_err());
    }
}
