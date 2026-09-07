//! §12.2 토이 게임을 `validate` 서브커맨드와 같은 경로로 돌린다.

use pokergoat_solver::validate::run_toy_game;

#[test]
fn toy_game_reproduces_theory_values() {
    let result = run_toy_game(600).expect("토이 게임 실행 실패");
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    assert!(
        (result.ip_call_frequency - 0.5).abs() <= 0.02,
        "IP 콜 빈도 {:.4}",
        result.ip_call_frequency
    );
    assert!(
        (result.bluff_share_of_bets - 1.0 / 3.0).abs() <= 0.02,
        "블러프 비율 {:.4}",
        result.bluff_share_of_bets
    );
    assert!(
        result.exploitability_pct_pot <= 0.1,
        "익스플로이터빌리티 {:.4}%",
        result.exploitability_pct_pot
    );
    assert!(result.pass);
}
