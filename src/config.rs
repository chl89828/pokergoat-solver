//! 잡 설정 JSON 파싱과 검증 (설계서 §4.2)

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::cards;

/// bb를 정수 칩으로 바꿀 때 쓰는 배율. 5.5bb -> 550칩.
/// postflop-solver의 `starting_pot`, `effective_stack`, 액션 금액이 모두 i32라서 필요하다.
pub const CHIP_SCALE: f64 = 100.0;

/// 반복 상한 (설계서 §4.2)
pub const MAX_ITERATIONS: u32 = 5000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BoardInput {
    /// "As7d2c"
    Text(String),
    /// [48, 22, 3] (우리 카드 인코딩)
    Ids(Vec<u8>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BetSizing {
    /// 첫 벳 사이즈 (% pot)
    #[serde(default)]
    pub bet: Vec<f64>,
    /// 레이즈 사이즈 (% pot)
    #[serde(default)]
    pub raise: Vec<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreetSizing {
    #[serde(default)]
    pub oop: BetSizing,
    #[serde(default)]
    pub ip: BetSizing,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sizing {
    #[serde(default)]
    pub flop: StreetSizing,
    #[serde(default)]
    pub turn: StreetSizing,
    #[serde(default)]
    pub river: StreetSizing,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rake {
    /// 레이크 비율 (%)
    pub percent: f64,
    /// 레이크 상한 (bb)
    pub cap_bb: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Donk {
    /// 턴 돈벳 사이즈 (% pot)
    #[serde(default)]
    pub turn: Vec<f64>,
    /// 리버 돈벳 사이즈 (% pot)
    #[serde(default)]
    pub river: Vec<f64>,
}

fn default_max_raises() -> u32 {
    2
}
fn default_all_in_threshold() -> f64 {
    0.67
}
fn default_add_all_in_threshold() -> f64 {
    1.5
}
fn default_merging_threshold() -> f64 {
    0.1
}
fn default_target_exploitability() -> f64 {
    0.3
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobConfig {
    /// 3~5장 보드 (우리 카드 인코딩 또는 "As7d2c" 표기)
    pub board: BoardInput,
    /// [OOP, IP] 레인지 표기 (부록 A)
    pub ranges: [String; 2],
    /// 시작 팟 (bb)
    pub pot: f64,
    /// 유효 스택 (bb)
    pub effective_stack: f64,
    pub sizing: Sizing,
    #[serde(default = "default_max_raises")]
    pub max_raises_per_street: u32,
    /// 0..1. postflop-solver의 force_allin_threshold
    #[serde(default = "default_all_in_threshold")]
    pub all_in_threshold: f64,
    /// postflop-solver의 add_allin_threshold
    #[serde(default = "default_add_all_in_threshold")]
    pub add_all_in_threshold: f64,
    /// 최대 반복
    pub iterations: u32,
    /// 목표 익스플로이터빌리티 (% pot)
    #[serde(default = "default_target_exploitability")]
    pub target_exploitability: f64,
    #[serde(default)]
    pub rake: Option<Rake>,
    #[serde(default)]
    pub donk: Option<Donk>,
    #[serde(default = "default_merging_threshold")]
    pub merging_threshold: f64,
    /// 리버 blob 저장 여부
    #[serde(default)]
    pub store_river: bool,
    /// 리버 노드 전용 프룬 도달 임계값. 없으면 일반 프룬 임계값을 그대로 쓴다.
    /// CLI의 `--river-prune-reach`가 이 값을 덮어쓴다.
    #[serde(default)]
    pub river_prune_reach: Option<f64>,
    /// 리버 플레이어 노드에 EV를 담을지. 없으면 담는다(기존 동작).
    /// CLI의 `--river-ev`가 이 값을 덮어쓴다.
    #[serde(default)]
    pub river_ev: Option<bool>,
    /// blob 헤더에 적히는 메타
    #[serde(default)]
    pub scenario_id: u32,
    #[serde(default)]
    pub template_id: u16,
    /// 사람이 읽는 식별자 (manifest에만 들어간다)
    #[serde(default)]
    pub scenario: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
}

impl JobConfig {
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("잡 설정 파일을 읽지 못했다: {}", path.display()))?;
        Self::from_str(&text)
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Self> {
        let config: JobConfig = serde_json::from_str(text).context("잡 설정 JSON 파싱 실패")?;
        config.validate()?;
        Ok(config)
    }

    /// 보드를 우리 카드 인코딩 벡터로
    pub fn board_cards(&self) -> Result<Vec<u8>> {
        match &self.board {
            BoardInput::Text(s) => cards::board_from_str(s),
            BoardInput::Ids(ids) => Ok(ids.clone()),
        }
    }

    /// 시작 스트리트 (보드 장수로 결정)
    pub fn street(&self) -> Result<Street> {
        Ok(match self.board_cards()?.len() {
            3 => Street::Flop,
            4 => Street::Turn,
            5 => Street::River,
            n => bail!("보드는 3~5장이어야 한다: {n}장"),
        })
    }

    pub fn pot_chips(&self) -> i32 {
        (self.pot * CHIP_SCALE).round() as i32
    }

    pub fn stack_chips(&self) -> i32 {
        (self.effective_stack * CHIP_SCALE).round() as i32
    }

    pub fn rake_rate(&self) -> f64 {
        self.rake.as_ref().map_or(0.0, |r| r.percent / 100.0)
    }

    pub fn rake_cap_chips(&self) -> f64 {
        self.rake.as_ref().map_or(0.0, |r| r.cap_bb * CHIP_SCALE)
    }

    /// 목표 익스플로이터빌리티를 칩 단위 절대값으로 (라이브러리 solve의 인자 규격)
    pub fn target_exploitability_chips(&self) -> f32 {
        (self.pot_chips() as f64 * self.target_exploitability / 100.0) as f32
    }

    pub fn validate(&self) -> Result<()> {
        let board = self.board_cards()?;
        if !(3..=5).contains(&board.len()) {
            bail!("보드는 3~5장이어야 한다: {}장", board.len());
        }
        for &card in &board {
            if card >= 52 {
                bail!("보드 카드 id 범위 초과: {card}");
            }
        }
        let mut sorted = board.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() != board.len() {
            bail!("보드에 중복 카드가 있다");
        }

        if !(self.pot > 0.0) {
            bail!("pot은 0보다 커야 한다: {}", self.pot);
        }
        if !(self.effective_stack > 0.0) {
            bail!(
                "effectiveStack은 0보다 커야 한다: {}",
                self.effective_stack
            );
        }
        if self.pot_chips() <= 0 || self.stack_chips() <= 0 {
            bail!("pot과 effectiveStack이 칩 단위로 0이 된다 (배율 {CHIP_SCALE})");
        }
        if self.iterations == 0 || self.iterations > MAX_ITERATIONS {
            bail!(
                "iterations는 1..={MAX_ITERATIONS} 범위여야 한다: {}",
                self.iterations
            );
        }
        if !(0.0..=1.0).contains(&self.all_in_threshold) {
            bail!(
                "allInThreshold는 0..1 범위여야 한다: {}",
                self.all_in_threshold
            );
        }
        if self.add_all_in_threshold < 0.0 {
            bail!("addAllInThreshold는 0 이상이어야 한다");
        }
        if self.merging_threshold < 0.0 {
            bail!("mergingThreshold는 0 이상이어야 한다");
        }
        if self.target_exploitability < 0.0 {
            bail!("targetExploitability는 0 이상이어야 한다");
        }
        if let Some(reach) = self.river_prune_reach {
            if !(0.0..=1.0).contains(&reach) {
                bail!("riverPruneReach는 0..1 범위여야 한다: {reach}");
            }
        }
        if let Some(rake) = &self.rake {
            if !(0.0..=100.0).contains(&rake.percent) {
                bail!("rake.percent는 0..100 범위여야 한다: {}", rake.percent);
            }
            if rake.cap_bb < 0.0 {
                bail!("rake.capBb는 0 이상이어야 한다");
            }
        }
        for (name, range) in ["oop", "ip"].iter().zip(self.ranges.iter()) {
            if range.trim().is_empty() {
                bail!("{name} 레인지가 비어 있다");
            }
            crate::range::parse_range(range)
                .with_context(|| format!("{name} 레인지 파싱 실패"))?;
        }
        for (street, sizing) in [
            ("flop", &self.sizing.flop),
            ("turn", &self.sizing.turn),
            ("river", &self.sizing.river),
        ] {
            for (player, bs) in [("oop", &sizing.oop), ("ip", &sizing.ip)] {
                for &pct in bs.bet.iter().chain(bs.raise.iter()) {
                    if !(pct > 0.0) || pct > 10_000.0 {
                        bail!("{street}.{player} 사이즈가 이상하다: {pct}%");
                    }
                }
            }
        }
        if let Some(donk) = &self.donk {
            for &pct in donk.turn.iter().chain(donk.river.iter()) {
                if !(pct > 0.0) || pct > 10_000.0 {
                    bail!("donk 사이즈가 이상하다: {pct}%");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Street {
    Flop,
    Turn,
    River,
}

impl Street {
    pub fn as_u8(self) -> u8 {
        match self {
            Street::Flop => 0,
            Street::Turn => 1,
            Street::River => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Street::Flop => "flop",
            Street::Turn => "turn",
            Street::River => "river",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "board": "As7d2c",
        "ranges": ["22+,A2s+", "QQ-22,AQs-A2s"],
        "pot": 5.5,
        "effectiveStack": 97.5,
        "sizing": {
            "flop": { "oop": { "bet": [33, 75], "raise": [50] }, "ip": { "bet": [33, 75], "raise": [50] } },
            "turn": { "oop": { "bet": [50, 100], "raise": [50] }, "ip": { "bet": [50, 100], "raise": [50] } },
            "river": { "oop": { "bet": [50, 100, 150], "raise": [50] }, "ip": { "bet": [50, 100, 150], "raise": [50] } }
        },
        "maxRaisesPerStreet": 2,
        "allInThreshold": 0.67,
        "iterations": 1000,
        "targetExploitability": 0.3,
        "rake": { "percent": 5, "capBb": 3 },
        "donk": { "turn": [50], "river": [50] },
        "mergingThreshold": 0.1,
        "storeRiver": false
    }"#;

    #[test]
    fn parses_sample() {
        let config = JobConfig::from_str(SAMPLE).unwrap();
        assert_eq!(config.board_cards().unwrap(), vec![48, 22, 3]);
        assert_eq!(config.street().unwrap(), Street::Flop);
        assert_eq!(config.pot_chips(), 550);
        assert_eq!(config.stack_chips(), 9750);
        assert_eq!(config.rake_rate(), 0.05);
        assert_eq!(config.rake_cap_chips(), 300.0);
        assert_eq!(config.sizing.flop.oop.bet, vec![33.0, 75.0]);
        assert!(!config.store_river);
        assert_eq!(config.max_raises_per_street, 2);
    }

    #[test]
    fn board_as_ids_is_equivalent() {
        let text = SAMPLE.replace("\"As7d2c\"", "[48, 22, 3]");
        let config = JobConfig::from_str(&text).unwrap();
        assert_eq!(config.board_cards().unwrap(), vec![48, 22, 3]);
    }

    #[test]
    fn defaults_apply() {
        let text = r#"{
            "board": "As7d2c",
            "ranges": ["AA", "KK"],
            "pot": 10,
            "effectiveStack": 100,
            "sizing": { "flop": { "oop": { "bet": [50] }, "ip": { "bet": [50] } } },
            "iterations": 10
        }"#;
        let config = JobConfig::from_str(text).unwrap();
        assert_eq!(config.max_raises_per_street, 2);
        assert_eq!(config.all_in_threshold, 0.67);
        assert_eq!(config.add_all_in_threshold, 1.5);
        assert_eq!(config.merging_threshold, 0.1);
        assert_eq!(config.target_exploitability, 0.3);
        assert!(config.rake.is_none());
    }

    #[test]
    fn rejects_bad_board() {
        let text = SAMPLE.replace("\"As7d2c\"", "\"AsAs2c\"");
        assert!(JobConfig::from_str(&text).is_err());
        let text = SAMPLE.replace("\"As7d2c\"", "\"As2c\"");
        assert!(JobConfig::from_str(&text).is_err());
    }

    #[test]
    fn rejects_bad_numbers() {
        assert!(JobConfig::from_str(&SAMPLE.replace("\"pot\": 5.5", "\"pot\": 0")).is_err());
        assert!(JobConfig::from_str(
            &SAMPLE.replace("\"iterations\": 1000", "\"iterations\": 99999")
        )
        .is_err());
        assert!(JobConfig::from_str(
            &SAMPLE.replace("\"allInThreshold\": 0.67", "\"allInThreshold\": 1.5")
        )
        .is_err());
    }

    #[test]
    fn rejects_bad_range() {
        assert!(JobConfig::from_str(&SAMPLE.replace("\"22+,A2s+\"", "\"ZZ\"")).is_err());
    }

    #[test]
    fn river_options_default_to_none() {
        let config = JobConfig::from_str(SAMPLE).unwrap();
        assert!(config.river_prune_reach.is_none());
        assert!(config.river_ev.is_none());
    }

    #[test]
    fn parses_river_options() {
        let text = SAMPLE.replace(
            "\"storeRiver\": false",
            "\"storeRiver\": true, \"riverPruneReach\": 0.001, \"riverEv\": false",
        );
        let config = JobConfig::from_str(&text).unwrap();
        assert_eq!(config.river_prune_reach, Some(0.001));
        assert_eq!(config.river_ev, Some(false));
    }

    #[test]
    fn rejects_river_prune_reach_out_of_range() {
        let text = SAMPLE.replace("\"storeRiver\": false", "\"riverPruneReach\": 2.5");
        assert!(JobConfig::from_str(&text).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        let text = SAMPLE.replace("\"pot\": 5.5", "\"pot\": 5.5, \"potato\": 1");
        assert!(JobConfig::from_str(&text).is_err());
    }

    #[test]
    fn street_from_board_length() {
        let turn = SAMPLE.replace("\"As7d2c\"", "\"As7d2cKh\"");
        assert_eq!(
            JobConfig::from_str(&turn).unwrap().street().unwrap(),
            Street::Turn
        );
        let river = SAMPLE.replace("\"As7d2c\"", "\"As7d2cKh9s\"");
        assert_eq!(
            JobConfig::from_str(&river).unwrap().street().unwrap(),
            Street::River
        );
    }
}
