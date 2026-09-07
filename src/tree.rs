//! 잡 설정 -> postflop-solver `CardConfig` / `TreeConfig` / `ActionTree` (설계서 §4.3)

use anyhow::{bail, Context, Result};
use postflop_solver::{
    ActionTree, BetSizeOptions, BoardState, Card, CardConfig, DonkSizeOptions, PostFlopGame, Range,
    TreeConfig, NOT_DEALT,
};

use crate::cards;
use crate::config::{JobConfig, Street};
use crate::range::parse_range;

/// 트리 빌드 결과. 경고는 manifest와 stderr로 나간다.
pub struct BuiltGame {
    pub game: PostFlopGame,
    pub warnings: Vec<String>,
    pub isomorphism: Isomorphism,
    pub street: Street,
}

/// 사이즈 목록(% pot)을 라이브러리 표기 문자열로. "33%, 75%"
fn sizes_to_string(sizes: &[f64]) -> String {
    sizes
        .iter()
        .map(|pct| format!("{}%", trim_float(*pct)))
        .collect::<Vec<_>>()
        .join(",")
}

fn trim_float(value: f64) -> String {
    if (value - value.round()).abs() < 1e-9 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

fn bet_options(sizing: &crate::config::BetSizing) -> Result<BetSizeOptions> {
    let bet = sizes_to_string(&sizing.bet);
    let raise = sizes_to_string(&sizing.raise);
    BetSizeOptions::try_from((bet.as_str(), raise.as_str()))
        .map_err(|e| anyhow::anyhow!("벳 사이즈 변환 실패 ({bet} / {raise}): {e}"))
}

fn donk_options(sizes: &[f64]) -> Result<Option<DonkSizeOptions>> {
    if sizes.is_empty() {
        return Ok(None);
    }
    let text = sizes_to_string(sizes);
    DonkSizeOptions::try_from(text.as_str())
        .map(Some)
        .map_err(|e| anyhow::anyhow!("돈벳 사이즈 변환 실패 ({text}): {e}"))
}

pub fn build_card_config(config: &JobConfig) -> Result<CardConfig> {
    let board = config.board_cards()?;
    let oop = parse_range(&config.ranges[0]).context("OOP 레인지")?;
    let ip = parse_range(&config.ranges[1]).context("IP 레인지")?;
    let flop = [
        cards::to_lib(board[0]),
        cards::to_lib(board[1]),
        cards::to_lib(board[2]),
    ];
    let turn = board.get(3).map_or(NOT_DEALT, |&c| cards::to_lib(c));
    let river = board.get(4).map_or(NOT_DEALT, |&c| cards::to_lib(c));
    Ok(CardConfig {
        range: [oop, ip],
        flop,
        turn,
        river,
    })
}

pub fn build_tree_config(config: &JobConfig) -> Result<(TreeConfig, Vec<String>)> {
    let mut warnings = Vec::new();
    let street = config.street()?;
    let initial_state = match street {
        Street::Flop => BoardState::Flop,
        Street::Turn => BoardState::Turn,
        Street::River => BoardState::River,
    };

    let donk = config.donk.clone().unwrap_or_default();
    let tree_config = TreeConfig {
        initial_state,
        starting_pot: config.pot_chips(),
        effective_stack: config.stack_chips(),
        rake_rate: config.rake_rate(),
        rake_cap: config.rake_cap_chips(),
        flop_bet_sizes: [
            bet_options(&config.sizing.flop.oop)?,
            bet_options(&config.sizing.flop.ip)?,
        ],
        turn_bet_sizes: [
            bet_options(&config.sizing.turn.oop)?,
            bet_options(&config.sizing.turn.ip)?,
        ],
        river_bet_sizes: [
            bet_options(&config.sizing.river.oop)?,
            bet_options(&config.sizing.river.ip)?,
        ],
        turn_donk_sizes: donk_options(&donk.turn)?,
        river_donk_sizes: donk_options(&donk.river)?,
        add_allin_threshold: config.add_all_in_threshold,
        force_allin_threshold: config.all_in_threshold,
        merging_threshold: config.merging_threshold,
    };

    // maxRaisesPerStreet은 postflop-solver에 대응 옵션이 없다 (설계서 §14).
    // 퍼센트 사이즈에는 raise cap이 걸리지 않으므로 무시하고 경고만 남긴다.
    if config.max_raises_per_street != 0 {
        warnings.push(format!(
            "maxRaisesPerStreet={}는 엔진에 대응 옵션이 없어 무시했다. 레이즈 횟수는 스택과 사이즈 목록이 결정한다.",
            config.max_raises_per_street
        ));
    }

    Ok((tree_config, warnings))
}

pub fn build_action_tree(config: &JobConfig) -> Result<(ActionTree, Vec<String>)> {
    let (tree_config, warnings) = build_tree_config(config)?;
    let tree = ActionTree::new(tree_config).map_err(|e| anyhow::anyhow!("액션 트리 생성 실패: {e}"))?;
    Ok((tree, warnings))
}

pub fn build_game(config: &JobConfig) -> Result<BuiltGame> {
    let card_config = build_card_config(config)?;
    let (action_tree, warnings) = build_action_tree(config)?;
    let street = config.street()?;
    let isomorphism = Isomorphism::new(&card_config);
    let game = PostFlopGame::with_config(card_config, action_tree)
        .map_err(|e| anyhow::anyhow!("게임 생성 실패: {e}"))?;
    if game.private_cards(0).is_empty() || game.private_cards(1).is_empty() {
        bail!("보드와 겹치지 않는 핸드가 없다. 레인지를 확인해라");
    }
    Ok(BuiltGame {
        game,
        warnings,
        isomorphism,
        street,
    })
}

/// 수트 동형 계산. postflop-solver `CardConfig::isomorphism`과 같은 규칙을 우리 쪽에서 다시 만든다.
///
/// 라이브러리는 대표 카드 목록을 공개 API로 내주지 않는다. 대신 찬스 노드의
/// `available_actions()`가 대표 액션만 돌려주므로, 여기서 만든 표를 그 목록과 대조해
/// 일치할 때만 동형을 쓴다 (`verify_turn`).
#[derive(Debug, Clone)]
pub struct Isomorphism {
    /// suit 하나가 대표 suit으로 접히면 Some(대표 suit). 라이브러리 suit 인코딩 기준.
    turn_isomorphic_suit: [Option<u8>; 4],
    suit_class: [u8; 4],
    flop_rankset: [u16; 4],
    flop_mask: u64,
    turn_fixed: bool,
}

impl Isomorphism {
    pub fn new(card_config: &CardConfig) -> Self {
        let suit_class = suit_isomorphism_classes(&card_config.range);
        let mut flop_rankset = [0u16; 4];
        let mut flop_mask = 0u64;
        for &card in &card_config.flop {
            flop_rankset[(card & 3) as usize] |= 1 << (card >> 2);
            flop_mask |= 1 << card;
        }

        let turn_fixed = card_config.turn != NOT_DEALT;
        let mut turn_isomorphic_suit = [None; 4];
        if !turn_fixed {
            for suit1 in 1..4u8 {
                for suit2 in 0..suit1 {
                    if flop_rankset[suit1 as usize] == flop_rankset[suit2 as usize]
                        && suit_class[suit1 as usize] == suit_class[suit2 as usize]
                    {
                        turn_isomorphic_suit[suit1 as usize] = Some(suit2);
                        break;
                    }
                }
            }
        }

        Self {
            turn_isomorphic_suit,
            suit_class,
            flop_rankset,
            flop_mask,
            turn_fixed,
        }
    }

    /// 턴 카드의 대표 카드 (라이브러리 인코딩). 자기 자신이 대표면 그대로 돌려준다.
    pub fn turn_representative(&self, card: Card) -> Card {
        let suit = card & 3;
        match self.turn_isomorphic_suit[suit as usize] {
            Some(repr_suit) => card - suit + repr_suit,
            None => card,
        }
    }

    /// 플랍 다음에 나올 수 있는 대표 턴 카드 목록 (라이브러리 인코딩, 오름차순)
    pub fn turn_representatives(&self) -> Vec<Card> {
        (0..52u8)
            .filter(|&c| self.flop_mask & (1 << c) == 0)
            .filter(|&c| self.turn_representative(c) == c)
            .collect()
    }

    /// 주어진 턴에서 리버 카드의 대표 카드
    pub fn river_representative(&self, turn: Card, card: Card) -> Card {
        let mut turn_rankset = self.flop_rankset;
        turn_rankset[(turn & 3) as usize] |= 1 << (turn >> 2);
        let suit = card & 3;
        for suit2 in 0..suit {
            let flop_ok = self.turn_fixed
                || self.flop_rankset[suit as usize] == self.flop_rankset[suit2 as usize];
            if flop_ok
                && turn_rankset[suit as usize] == turn_rankset[suit2 as usize]
                && self.suit_class[suit as usize] == self.suit_class[suit2 as usize]
            {
                return card - suit + suit2;
            }
        }
        card
    }

    pub fn river_representatives(&self, turn: Card) -> Vec<Card> {
        let dead = self.flop_mask | (1 << turn);
        (0..52u8)
            .filter(|&c| dead & (1 << c) == 0)
            .filter(|&c| self.river_representative(turn, c) == c)
            .collect()
    }

    /// 우리 표가 라이브러리의 대표 목록과 같은지 확인한다.
    /// `available` 는 찬스 노드의 `available_actions()`에서 뽑은 대표 카드 (라이브러리 인코딩).
    pub fn verify(expected: &[Card], available: &[Card]) -> bool {
        let mut a = expected.to_vec();
        let mut b = available.to_vec();
        a.sort_unstable();
        b.sort_unstable();
        a == b
    }
}

/// 두 수트가 레인지 관점에서 동형인지. 라이브러리 `Range::is_suit_isomorphic`과 같은 판정을
/// 공개 API(`get_weight_by_cards`)로 다시 구현했다.
fn is_suit_isomorphic(range: &Range, suit1: u8, suit2: u8) -> bool {
    let replace = |suit: u8| {
        if suit == suit1 {
            suit2
        } else if suit == suit2 {
            suit1
        } else {
            suit
        }
    };
    for card1 in 0..52u8 {
        for card2 in (card1 + 1)..52u8 {
            let c1 = (card1 & !3) | replace(card1 & 3);
            let c2 = (card2 & !3) | replace(card2 & 3);
            if range.get_weight_by_cards(card1, card2) != range.get_weight_by_cards(c1, c2) {
                return false;
            }
        }
    }
    true
}

fn suit_isomorphism_classes(ranges: &[Range; 2]) -> [u8; 4] {
    let mut classes = [0u8; 4];
    let mut next = 1u8;
    'outer: for suit2 in 1..4u8 {
        for suit1 in 0..suit2 {
            if is_suit_isomorphic(&ranges[0], suit1, suit2)
                && is_suit_isomorphic(&ranges[1], suit1, suit2)
            {
                classes[suit2 as usize] = classes[suit1 as usize];
                continue 'outer;
            }
        }
        classes[suit2 as usize] = next;
        next += 1;
    }
    classes
}

/// 확장 노드 수 추정 (estimate 서브커맨드). 액션 트리를 순회하면서 찬스 노드를
/// 대표 카드 수만큼 곱한다. 실제 `PostFlopGame`의 노드 수와 같은 규칙이다.
pub fn count_nodes(
    tree: &mut ActionTree,
    iso: &Isomorphism,
    fixed_turn: Option<Card>,
) -> Result<NodeCount> {
    let mut count = NodeCount::default();
    walk_count(tree, iso, fixed_turn, &mut count, false)?;
    Ok(count)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NodeCount {
    pub total: u64,
    pub player: u64,
    pub chance: u64,
    pub terminal: u64,
    pub action_tree: u64,
}

/// `ActionTree`는 찬스 노드를 자동으로 건너뛴다. `available_actions()`가 이미 찬스 이후의
/// 액션을 주고 `play()`가 찬스 액션을 대신 소비한다. 그래서 같은 위치를 두 번 본다.
/// 한 번은 찬스 노드로, 다음은 `skip_chance = true`로 그 뒤 플레이어 노드로.
fn walk_count(
    tree: &mut ActionTree,
    iso: &Isomorphism,
    turn: Option<Card>,
    count: &mut NodeCount,
    skip_chance: bool,
) -> Result<u64> {
    count.action_tree += 1;

    // 올인 뒤 남은 찬스 체인은 라이브러리가 터미널로 본다 (PostFlopGame도 같다).
    if tree.is_terminal_node() {
        count.total += 1;
        count.terminal += 1;
        return Ok(1);
    }

    if !skip_chance && tree.is_chance_node() {
        count.total += 1;
        count.chance += 1;
        let mut total = 1u64;
        match turn {
            None => {
                for repr in iso.turn_representatives() {
                    total += walk_count(tree, iso, Some(repr), count, true)?;
                }
            }
            Some(turn_card) => {
                // 리버 서브트리는 카드마다 구조가 같다. 한 번 세고 곱한다.
                let rivers = iso.river_representatives(turn_card).len() as u64;
                let mut sub = NodeCount::default();
                let one = walk_count(tree, iso, turn, &mut sub, true)?;
                count.total += sub.total * rivers;
                count.player += sub.player * rivers;
                count.chance += sub.chance * rivers;
                count.terminal += sub.terminal * rivers;
                count.action_tree += sub.action_tree;
                total += one * rivers;
            }
        }
        return Ok(total);
    }

    count.total += 1;
    count.player += 1;
    let mut total = 1u64;
    let actions = tree.available_actions().to_vec();
    for action in actions {
        tree.play(action)
            .map_err(|e| anyhow::anyhow!("액션 트리 play 실패: {e}"))?;
        total += walk_count(tree, iso, turn, count, false)?;
        tree.undo()
            .map_err(|e| anyhow::anyhow!("액션 트리 undo 실패: {e}"))?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JobConfig;
    use postflop_solver::Action;

    /// 손으로 셀 수 있는 작은 트리. 리버 단독, OOP 팟벳 하나, 레이즈 없음.
    const TINY: &str = r#"{
        "board": "2c3d4h8s9c",
        "ranges": ["65,JT", "QQ"],
        "pot": 1.0,
        "effectiveStack": 1.0,
        "sizing": {
            "river": { "oop": { "bet": [100] }, "ip": {} }
        },
        "allInThreshold": 0,
        "addAllInThreshold": 0,
        "mergingThreshold": 0,
        "iterations": 1
    }"#;

    #[test]
    fn tiny_tree_actions_and_amounts() {
        let config = JobConfig::from_str(TINY).unwrap();
        let (mut tree, _) = build_action_tree(&config).unwrap();
        // 루트 = OOP. 체크와 (스택 = 팟이라 올인으로 승격된) 팟 벳.
        let actions = tree.available_actions().to_vec();
        assert_eq!(actions, vec![Action::Check, Action::AllIn(100)]);

        // 체크하면 IP는 벳 옵션이 없어 체크만 가능하다.
        tree.play(Action::Check).unwrap();
        assert_eq!(tree.available_actions(), &[Action::Check]);
        tree.play(Action::Check).unwrap();
        assert!(tree.is_terminal_node());
        tree.back_to_root();

        // 올인에 대한 IP 응답은 폴드와 콜뿐 (레이즈 목록이 비어 있다).
        tree.play(Action::AllIn(100)).unwrap();
        assert_eq!(tree.available_actions(), &[Action::Fold, Action::Call]);
        tree.back_to_root();
    }

    #[test]
    fn tiny_tree_node_count() {
        let config = JobConfig::from_str(TINY).unwrap();
        let (mut tree, _) = build_action_tree(&config).unwrap();
        let card_config = build_card_config(&config).unwrap();
        let iso = Isomorphism::new(&card_config);
        let count = count_nodes(&mut tree, &iso, Some(card_config.turn)).unwrap();
        // 루트(OOP), 체크 뒤 IP, 체크-체크 터미널, 올인 뒤 IP, 폴드 터미널, 콜 터미널
        assert_eq!(count.total, 6);
        assert_eq!(count.player, 3);
        assert_eq!(count.terminal, 3);
        assert_eq!(count.chance, 0);
    }

    #[test]
    fn bet_size_strings() {
        assert_eq!(sizes_to_string(&[33.0, 75.0]), "33%,75%");
        assert_eq!(sizes_to_string(&[]), "");
        assert_eq!(sizes_to_string(&[12.5]), "12.5%");
    }

    #[test]
    fn max_raises_warns() {
        let config = JobConfig::from_str(TINY).unwrap();
        let (_, warnings) = build_tree_config(&config).unwrap();
        assert!(warnings.iter().any(|w| w.contains("maxRaisesPerStreet")));
    }

    #[test]
    fn monotone_flop_has_isomorphic_turn_suits() {
        let text = r#"{
            "board": "AsKsQs",
            "ranges": ["22+", "22+"],
            "pot": 10, "effectiveStack": 100,
            "sizing": { "flop": { "oop": { "bet": [50] }, "ip": { "bet": [50] } } },
            "iterations": 1
        }"#;
        let config = JobConfig::from_str(text).unwrap();
        let card_config = build_card_config(&config).unwrap();
        let iso = Isomorphism::new(&card_config);
        // 보드에 없는 세 수트가 하나로 접힌다: 49장 -> 대표 카드는 훨씬 적다
        let reprs = iso.turn_representatives();
        // 보드에 없는 수트 세 개가 하나로 접힌다: 대표 13장 + 남은 스페이드 10장
        assert_eq!(reprs.len(), 23, "대표 카드 수: {}", reprs.len());
    }

    #[test]
    fn rainbow_flop_has_no_turn_isomorphism_when_ranges_symmetric() {
        let text = r#"{
            "board": "As7d2c",
            "ranges": ["22+", "22+"],
            "pot": 10, "effectiveStack": 100,
            "sizing": { "flop": { "oop": { "bet": [50] }, "ip": { "bet": [50] } } },
            "iterations": 1
        }"#;
        let config = JobConfig::from_str(text).unwrap();
        let card_config = build_card_config(&config).unwrap();
        let iso = Isomorphism::new(&card_config);
        // 하트만 보드에 없다. 다른 세 수트는 각각 rankset이 달라 접히지 않는다.
        assert_eq!(iso.turn_representatives().len(), 49);
    }
}
