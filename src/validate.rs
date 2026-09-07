//! §12.2 토이 게임. 정답이 손으로 계산되는 케이스라 엔진과 래퍼가 같이 검증된다.
//!
//! 보드 2c3d4h8s9c. OOP는 65(넛)와 JT(에어), IP는 QQ(블러프캐처)뿐이다.
//! 팟사이즈 벳 하나만 허용하고 레이즈와 레이크는 없다. 스택을 팟과 같게 두면
//! 팟 벳이 곧 올인이라 레이즈가 원천 봉쇄된다.
//!
//! 이론값. OOP 벳 중 블러프 비율 1/3 (JT 16콤보의 50%가 벳), IP 콜 빈도 50%.

use anyhow::{bail, Result};
use postflop_solver::{
    compute_exploitability, ActionTree, BetSizeOptions, BoardState, CardConfig, PostFlopGame,
    TreeConfig,
};
use serde::Serialize;

use crate::cards;
use crate::range::parse_range;
use crate::solve::run_solver;

/// 허용 오차 ±2%p
pub const TOLERANCE: f64 = 0.02;
/// 익스플로이터빌리티 상한 (% pot)
pub const MAX_EXPLOITABILITY_PCT: f64 = 0.1;

const POT: i32 = 100;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub value: f64,
    pub expected: f64,
    pub tolerance: f64,
    pub pass: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToyGameResult {
    pub board: String,
    pub oop_range: String,
    pub ip_range: String,
    pub iterations: u32,
    pub elapsed_sec: f64,
    pub exploitability_pct_pot: f64,
    pub ip_call_frequency: f64,
    pub bluff_share_of_bets: f64,
    pub oop_bet_frequency_nuts: f64,
    pub oop_bet_frequency_bluff: f64,
    pub checks: Vec<Check>,
    pub pass: bool,
}

pub fn run_toy_game(max_iterations: u32) -> Result<ToyGameResult> {
    let board = "2c3d4h8s9c";
    let board_cards = cards::board_from_str(board)?;
    let oop_range_text = "65,JT";
    let ip_range_text = "QQ";

    let card_config = CardConfig {
        range: [parse_range(oop_range_text)?, parse_range(ip_range_text)?],
        flop: [
            cards::to_lib(board_cards[0]),
            cards::to_lib(board_cards[1]),
            cards::to_lib(board_cards[2]),
        ],
        turn: cards::to_lib(board_cards[3]),
        river: cards::to_lib(board_cards[4]),
    };

    // OOP: 체크와 팟 벳. IP: 벳 옵션 없음 (체크 백만 가능), 레이즈 목록도 비어 있다.
    let oop_sizes = BetSizeOptions::try_from(("100%", ""))
        .map_err(|e| anyhow::anyhow!("토이 게임 벳 사이즈 실패: {e}"))?;
    let ip_sizes = BetSizeOptions::default();

    let tree_config = TreeConfig {
        initial_state: BoardState::River,
        starting_pot: POT,
        effective_stack: POT,
        rake_rate: 0.0,
        rake_cap: 0.0,
        flop_bet_sizes: Default::default(),
        turn_bet_sizes: Default::default(),
        river_bet_sizes: [oop_sizes, ip_sizes],
        turn_donk_sizes: None,
        river_donk_sizes: None,
        add_allin_threshold: 0.0,
        force_allin_threshold: 0.0,
        merging_threshold: 0.0,
    };

    let action_tree = ActionTree::new(tree_config)
        .map_err(|e| anyhow::anyhow!("토이 게임 트리 생성 실패: {e}"))?;
    let mut game = PostFlopGame::with_config(card_config, action_tree)
        .map_err(|e| anyhow::anyhow!("토이 게임 생성 실패: {e}"))?;
    game.allocate_memory(false);

    let target = POT as f32 * (MAX_EXPLOITABILITY_PCT as f32 / 100.0) * 0.5;
    let outcome = run_solver(&mut game, max_iterations, target, None, false)?;

    game.back_to_root();
    game.cache_normalized_weights();

    let actions = game.available_actions();
    if actions.len() != 2 || actions[0] != postflop_solver::Action::Check {
        bail!("루트 액션이 [체크, 팟 올인]이 아니다: {actions:?}");
    }
    let bet_index = 1;

    let oop_hands = game.private_cards(0).to_vec();
    let n_oop = oop_hands.len();
    let weights = game.normalized_weights(0).to_vec();
    let strategy = game.strategy();

    let mut nuts_total = 0.0f64;
    let mut nuts_bet = 0.0f64;
    let mut bluff_total = 0.0f64;
    let mut bluff_bet = 0.0f64;
    for (index, &(c1, c2)) in oop_hands.iter().enumerate() {
        let mut ranks = [c1 >> 2, c2 >> 2];
        ranks.sort_unstable();
        let weight = weights[index] as f64;
        let bet = weight * strategy[bet_index * n_oop + index] as f64;
        match ranks {
            [3, 4] => {
                // 65 = 넛 스트레이트
                nuts_total += weight;
                nuts_bet += bet;
            }
            [8, 9] => {
                // JT = 에어
                bluff_total += weight;
                bluff_bet += bet;
            }
            other => bail!("토이 게임 OOP 레인지에 예상 밖 핸드가 있다: {other:?}"),
        }
    }
    if nuts_total <= 0.0 || bluff_total <= 0.0 {
        bail!("토이 게임 OOP 레인지가 비었다");
    }
    let total_bet = nuts_bet + bluff_bet;
    if total_bet <= 0.0 {
        bail!("OOP가 전혀 벳하지 않는다");
    }
    let bluff_share = bluff_bet / total_bet;

    game.play(bet_index);
    game.cache_normalized_weights();
    let ip_actions = game.available_actions();
    if ip_actions.len() != 2 || ip_actions[0] != postflop_solver::Action::Fold {
        bail!("IP 액션이 [폴드, 콜]이 아니다: {ip_actions:?}");
    }
    let n_ip = game.private_cards(1).len();
    let ip_weights = game.normalized_weights(1).to_vec();
    let ip_strategy = game.strategy();
    let call_index = 1;
    let mut ip_total = 0.0f64;
    let mut ip_call = 0.0f64;
    for index in 0..n_ip {
        let weight = ip_weights[index] as f64;
        ip_total += weight;
        ip_call += weight * ip_strategy[call_index * n_ip + index] as f64;
    }
    if ip_total <= 0.0 {
        bail!("IP 레인지가 비었다");
    }
    let call_frequency = ip_call / ip_total;

    game.back_to_root();
    let exploitability_pct = (compute_exploitability(&game) as f64) / POT as f64 * 100.0;

    let checks = vec![
        Check {
            name: "ipCallFrequency".to_string(),
            value: call_frequency,
            expected: 0.5,
            tolerance: TOLERANCE,
            pass: (call_frequency - 0.5).abs() <= TOLERANCE,
        },
        Check {
            name: "bluffShareOfBets".to_string(),
            value: bluff_share,
            expected: 1.0 / 3.0,
            tolerance: TOLERANCE,
            pass: (bluff_share - 1.0 / 3.0).abs() <= TOLERANCE,
        },
        Check {
            name: "exploitabilityPctPot".to_string(),
            value: exploitability_pct,
            expected: 0.0,
            tolerance: MAX_EXPLOITABILITY_PCT,
            pass: exploitability_pct <= MAX_EXPLOITABILITY_PCT,
        },
    ];
    let pass = checks.iter().all(|c| c.pass);

    Ok(ToyGameResult {
        board: board.to_string(),
        oop_range: oop_range_text.to_string(),
        ip_range: ip_range_text.to_string(),
        iterations: outcome.iterations,
        elapsed_sec: outcome.elapsed_sec,
        exploitability_pct_pot: exploitability_pct,
        ip_call_frequency: call_frequency,
        bluff_share_of_bets: bluff_share,
        oop_bet_frequency_nuts: nuts_bet / nuts_total,
        oop_bet_frequency_bluff: bluff_bet / bluff_total,
        checks,
        pass,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toy_game_matches_theory() {
        let result = run_toy_game(1000).unwrap();
        assert!(
            result.pass,
            "토이 게임 실패: call={:.4} bluff={:.4} expl={:.4}%",
            result.ip_call_frequency, result.bluff_share_of_bets, result.exploitability_pct_pot
        );
        // 넛은 항상 벳한다
        assert!(result.oop_bet_frequency_nuts > 0.98);
        // 블러프는 절반쯤 벳한다
        assert!((result.oop_bet_frequency_bluff - 0.5).abs() < 0.04);
    }
}
