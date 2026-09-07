//! 솔버 구동. 라이브러리의 `solve()`는 시간 상한을 받지 않아서 `solve_step` 루프를 직접 돈다.

use anyhow::Result;
use postflop_solver::{compute_exploitability, finalize, solve_step, PostFlopGame};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct SolveOutcome {
    /// 칩 단위 익스플로이터빌리티
    pub exploitability: f32,
    pub iterations: u32,
    pub elapsed_sec: f64,
    /// 시간 상한에 걸려 목표 정확도 전에 끊겼는지
    pub hit_time_limit: bool,
}

/// 익스플로이터빌리티 재계산 주기 (라이브러리 `solve`와 같은 10회)
const CHECK_INTERVAL: u32 = 10;

pub fn run_solver(
    game: &mut PostFlopGame,
    max_iterations: u32,
    target_exploitability: f32,
    time_limit_sec: Option<f64>,
    progress: bool,
) -> Result<SolveOutcome> {
    let started = Instant::now();
    let mut exploitability = compute_exploitability(game);
    let mut iterations = 0u32;
    let mut hit_time_limit = false;

    for t in 0..max_iterations {
        if exploitability <= target_exploitability {
            break;
        }
        if let Some(limit) = time_limit_sec {
            if started.elapsed().as_secs_f64() >= limit {
                hit_time_limit = true;
                break;
            }
        }
        solve_step(game, t);
        iterations = t + 1;
        if iterations % CHECK_INTERVAL == 0 || iterations == max_iterations {
            exploitability = compute_exploitability(game);
            if progress {
                eprint!(
                    "\r반복 {iterations}/{max_iterations} 익스플로이터빌리티 {exploitability:.4}"
                );
            }
        }
    }

    if iterations % CHECK_INTERVAL != 0 {
        exploitability = compute_exploitability(game);
    }
    if progress {
        eprintln!();
    }

    finalize(game);

    Ok(SolveOutcome {
        exploitability,
        iterations,
        elapsed_sec: started.elapsed().as_secs_f64(),
        hit_time_limit,
    })
}
