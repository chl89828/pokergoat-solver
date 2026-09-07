//! 솔브가 끝난 게임 트리를 전부 순회해 §5.2 blob으로 떨군다.
//!
//! 파일 분할은 스트리트 단위다. 시작 스트리트 파일 하나, 플랍에서 시작하면 턴 카드마다
//! 파일 하나, `storeRiver`면 (턴, 리버)마다 파일 하나. 찬스 노드는 자기 스트리트 파일에
//! 남고 자식 인덱스는 다음 스트리트 파일 안의 노드 번호를 가리킨다.

use anyhow::{bail, Context, Result};
use postflop_solver::{Action, Card, PostFlopGame};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::blob::*;
use crate::cards;
use crate::config::CHIP_SCALE;
use crate::tree::Isomorphism;

/// 도달 확률이 이 값보다 낮은 플레이어 노드는 본문 없이 헤더만 남긴다.
pub const DEFAULT_PRUNE_EPSILON: f64 = 1e-5;

/// 리버 플레이어 노드에 EV 배열을 담을지의 기본값.
pub const DEFAULT_RIVER_EV: bool = true;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BlobKey {
    /// 시작 스트리트 파일
    Start,
    /// 턴 카드별 파일 (우리 카드 인코딩)
    Turn(u8),
    /// (턴, 리버) 파일 (우리 카드 인코딩)
    River(u8, u8),
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub path: String,
    pub nodes: u32,
    pub bytes_raw: u64,
    pub bytes_stored: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub files: Vec<FileInfo>,
    pub node_count: u64,
    pub pruned_nodes: u64,
    pub turn_cards: Vec<String>,
    pub turn_isomorphism: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub bytes_raw: u64,
    pub bytes_stored: u64,
    /// 본문(전략)을 담은 리버 플레이어 노드 수. 프룬된 노드는 세지 않는다.
    pub river_player_nodes: u64,
    /// blob 직렬화 + 압축 + 파일 쓰기에 걸린 시간
    pub write_sec: f64,
}

pub struct ExportOptions {
    pub header: BlobHeader,
    pub store_river: bool,
    pub prune_epsilon: f64,
    /// 리버 스트리트 플레이어 노드에만 적용하는 프룬 임계값.
    /// `prune_epsilon`과 같은 값을 넣으면 동작이 예전과 같다.
    pub river_prune_epsilon: f64,
    /// 리버 플레이어 노드에 EV 배열을 담을지. false면 전략만 쓰고 `FLAG_HAS_EV`를 끈다.
    pub river_ev: bool,
    pub compress: bool,
    pub out_dir: PathBuf,
}

impl ExportOptions {
    /// 리버 옵션을 기본값(플랍·턴과 같은 프룬 임계값, EV 포함)으로 채운다.
    pub fn new(
        header: BlobHeader,
        store_river: bool,
        prune_epsilon: f64,
        compress: bool,
        out_dir: PathBuf,
    ) -> Self {
        Self {
            header,
            store_river,
            prune_epsilon,
            river_prune_epsilon: prune_epsilon,
            river_ev: DEFAULT_RIVER_EV,
            compress,
            out_dir,
        }
    }
}

pub fn export(
    game: &mut PostFlopGame,
    iso: &Isomorphism,
    options: ExportOptions,
) -> Result<ExportSummary> {
    let mut exporter = Exporter::new(game, iso, options)?;
    exporter.run()
}

struct Exporter<'a> {
    game: &'a mut PostFlopGame,
    iso: &'a Isomorphism,
    options: ExportOptions,
    blobs: BTreeMap<BlobKey, Blob>,
    hands: [Vec<u16>; 2],
    root_weight_sum: [f64; 2],
    flop: Vec<u8>,
    summary: ExportSummary,
    iso_trusted: bool,
}

impl<'a> Exporter<'a> {
    fn new(
        game: &'a mut PostFlopGame,
        iso: &'a Isomorphism,
        options: ExportOptions,
    ) -> Result<Self> {
        let mut hands: [Vec<u16>; 2] = [Vec::new(), Vec::new()];
        for player in 0..2 {
            hands[player] = game
                .private_cards(player)
                .iter()
                .map(|&(c1, c2)| cards::combo_index(cards::from_lib(c1), cards::from_lib(c2)))
                .collect::<Result<Vec<_>>>()?;
        }
        game.back_to_root();
        let root_weight_sum = [
            game.weights(0).iter().map(|&w| w as f64).sum::<f64>(),
            game.weights(1).iter().map(|&w| w as f64).sum::<f64>(),
        ];
        if root_weight_sum[0] <= 0.0 || root_weight_sum[1] <= 0.0 {
            bail!("루트 도달 가중치가 0이다. 레인지가 보드와 전부 겹친다");
        }
        let flop: Vec<u8> = game
            .current_board()
            .iter()
            .take(3)
            .map(|&c| cards::from_lib(c))
            .collect();
        Ok(Self {
            game,
            iso,
            options,
            blobs: BTreeMap::new(),
            hands,
            root_weight_sum,
            flop,
            summary: ExportSummary::default(),
            iso_trusted: true,
        })
    }

    fn run(&mut self) -> Result<ExportSummary> {
        self.game.back_to_root();
        let mut history: Vec<usize> = Vec::new();
        self.walk(BlobKey::Start, &mut history)?;
        let started = std::time::Instant::now();
        self.write_files()?;
        self.summary.write_sec = started.elapsed().as_secs_f64();
        let mut summary = std::mem::take(&mut self.summary);
        summary.turn_cards.sort();
        summary.turn_cards.dedup();
        Ok(summary)
    }

    fn blob_for(&mut self, key: BlobKey) -> &mut Blob {
        let header = self.options.header.clone();
        let flop = self.flop.clone();
        let hands = self.hands.clone();
        self.blobs.entry(key).or_insert_with(|| {
            let mut header = header;
            match key {
                BlobKey::Start => {}
                BlobKey::Turn(card) => {
                    header.board = flop.clone();
                    header.board.push(card);
                    header.street = 1;
                }
                BlobKey::River(turn, river) => {
                    header.board = flop.clone();
                    header.board.push(turn);
                    header.board.push(river);
                    header.street = 2;
                }
            }
            Blob::new(header, hands)
        })
    }

    fn reserve(&mut self, key: BlobKey) -> u32 {
        let blob = self.blob_for(key);
        let index = blob.nodes.len() as u32;
        blob.nodes.push(BlobNode {
            id: index,
            kind: KIND_TERMINAL,
            player: PLAYER_NONE,
            flags: 0,
            actions: Vec::new(),
            children: Vec::new(),
            invest: [0.0, 0.0],
            strategy: Vec::new(),
            ev: Vec::new(),
            ev_scale: 1.0,
        });
        self.summary.node_count += 1;
        index
    }

    fn invest(&self) -> [f32; 2] {
        let amounts = self.game.total_bet_amount();
        [
            amounts[0] as f32 / CHIP_SCALE as f32,
            amounts[1] as f32 / CHIP_SCALE as f32,
        ]
    }

    fn walk(&mut self, key: BlobKey, history: &mut Vec<usize>) -> Result<u32> {
        let index = self.reserve(key);

        if self.game.is_terminal_node() {
            let invest = self.invest();
            let node = &mut self.blobs.get_mut(&key).unwrap().nodes[index as usize];
            node.kind = KIND_TERMINAL;
            node.invest = invest;
            return Ok(index);
        }

        if self.game.is_chance_node() {
            return self.walk_chance(key, index, history);
        }

        self.walk_player(key, index, history)
    }

    fn walk_chance(&mut self, key: BlobKey, index: u32, history: &mut Vec<usize>) -> Result<u32> {
        let invest = self.invest();
        let board = self.game.current_board();
        let dealing_turn = board.len() == 3;
        let turn_card = if dealing_turn { None } else { Some(board[3]) };

        let possible_mask = self.game.possible_cards();
        let possible: Vec<Card> = (0..52u8)
            .filter(|&c| possible_mask & (1u64 << c) != 0)
            .collect();

        let library_reps: Vec<Card> = self
            .game
            .available_actions()
            .iter()
            .filter_map(|action| match action {
                Action::Chance(card) => Some(*card),
                _ => None,
            })
            .filter(|&card| possible_mask & (1u64 << card) != 0)
            .collect();

        // 우리 동형 표와 라이브러리 대표 목록을 대조한다. 어긋나면 동형을 쓰지 않고
        // 가능한 카드를 전부 대표로 취급한다 (파일 수는 늘어나지만 값은 정확하다).
        let mut computed_reps: Vec<Card> = possible
            .iter()
            .copied()
            .filter(|&c| self.representative(c, turn_card) == c)
            .collect();
        if !Isomorphism::verify(&computed_reps, &library_reps) {
            if self.iso_trusted {
                self.summary.warnings.push(
                    "수트 동형 표가 엔진 대표 목록과 달라 동형 축약을 끄고 모든 카드를 저장한다"
                        .to_string(),
                );
            }
            self.iso_trusted = false;
            computed_reps = possible.clone();
        }

        let store_children = dealing_turn || self.options.store_river;
        let mut child_of_card: BTreeMap<Card, u32> = BTreeMap::new();

        if store_children {
            for &rep in &computed_reps {
                let child_key = if dealing_turn {
                    BlobKey::Turn(cards::from_lib(rep))
                } else {
                    let turn = turn_card.expect("리버 딜인데 턴이 없다");
                    BlobKey::River(cards::from_lib(turn), cards::from_lib(rep))
                };
                history.push(rep as usize);
                self.game.play(rep as usize);
                let child = self.walk(child_key, history)?;
                child_of_card.insert(rep, child);
                history.pop();
                self.game.apply_history(history);
            }
        }

        let mut children = vec![CHILD_IMPOSSIBLE; CHANCE_CHILDREN];
        for &card in &possible {
            let our_card = cards::from_lib(card) as usize;
            if !store_children {
                children[our_card] = CHILD_NOT_STORED;
                continue;
            }
            let rep = if self.iso_trusted {
                self.representative(card, turn_card)
            } else {
                card
            };
            children[our_card] = child_of_card
                .get(&rep)
                .copied()
                .unwrap_or(CHILD_NOT_STORED);
            if dealing_turn {
                let card_name = cards::card_to_string(cards::from_lib(card))?;
                let rep_name = cards::card_to_string(cards::from_lib(rep))?;
                if rep == card {
                    self.summary.turn_cards.push(card_name);
                } else {
                    self.summary.turn_isomorphism.insert(card_name, rep_name);
                }
            }
        }

        let node = &mut self.blobs.get_mut(&key).unwrap().nodes[index as usize];
        node.kind = KIND_CHANCE;
        node.player = PLAYER_NONE;
        node.invest = invest;
        node.children = children;
        Ok(index)
    }

    fn representative(&self, card: Card, turn: Option<Card>) -> Card {
        match turn {
            None => self.iso.turn_representative(card),
            Some(turn_card) => self.iso.river_representative(turn_card, card),
        }
    }

    fn walk_player(&mut self, key: BlobKey, index: u32, history: &mut Vec<usize>) -> Result<u32> {
        let invest = self.invest();
        let player = self.game.current_player();
        // 보드 5장이면 리버다. 시작 스트리트가 어디든 이 판정이 성립한다.
        let is_river = self.game.current_board().len() == 5;
        let actions: Vec<Action> = self.game.available_actions();
        let blob_actions: Vec<BlobAction> = actions.iter().map(action_to_blob).collect();
        let n_actions = actions.len();
        let n_hands = self.game.private_cards(player).len();

        let weight_sum: f64 = self.game.weights(player).iter().map(|&w| w as f64).sum();
        let reach = weight_sum / self.root_weight_sum[player];

        let prune_epsilon = if is_river {
            self.options.river_prune_epsilon
        } else {
            self.options.prune_epsilon
        };
        if reach < prune_epsilon {
            let node = &mut self.blobs.get_mut(&key).unwrap().nodes[index as usize];
            node.kind = KIND_PLAYER;
            node.player = player as u8;
            node.flags = FLAG_PRUNED;
            node.actions = blob_actions;
            node.children = vec![CHILD_NOT_STORED; n_actions];
            node.invest = invest;
            self.summary.pruned_nodes += 1;
            return Ok(index);
        }

        // 리버 EV를 끄면 계산 자체를 건너뛴다. 파일 크기와 export 시간이 같이 줄어든다.
        // 정규화 가중치 캐시는 EV 계산에만 필요해서 같이 건너뛴다. 전략 배열은 영향받지 않는다
        // (테스트 river_ev_off_drops_ev_only_on_the_river가 두 경로의 전략 바이트를 맞춰 본다).
        let store_ev = !is_river || self.options.river_ev;
        let strategy = self.game.strategy();
        if strategy.len() != n_actions * n_hands {
            bail!(
                "전략 길이가 예상과 다르다 (actions {n_actions}, hands {n_hands}, strategy {})",
                strategy.len()
            );
        }
        let quantized = quantize_strategy(&strategy, n_actions, n_hands);
        let (ev, ev_scale) = if store_ev {
            self.game.cache_normalized_weights();
            let ev_chips = self.game.expected_values_detail(player);
            if ev_chips.len() != n_actions * n_hands {
                bail!(
                    "EV 길이가 예상과 다르다 (actions {n_actions}, hands {n_hands}, ev {})",
                    ev_chips.len()
                );
            }
            let ev_bb: Vec<f32> = ev_chips.iter().map(|v| v / CHIP_SCALE as f32).collect();
            quantize_ev(&ev_bb)
        } else {
            // EV 없음. 오프셋은 직렬화 때 u64::MAX가 되고 스케일은 의미가 없어 0으로 둔다.
            (Vec::new(), 0.0)
        };
        if is_river {
            self.summary.river_player_nodes += 1;
        }

        let mut children = Vec::with_capacity(n_actions);
        for action_index in 0..n_actions {
            history.push(action_index);
            self.game.play(action_index);
            let child = self.walk(key, history)?;
            children.push(child);
            history.pop();
            self.game.apply_history(history);
        }

        let node = &mut self.blobs.get_mut(&key).unwrap().nodes[index as usize];
        node.kind = KIND_PLAYER;
        node.player = player as u8;
        node.flags = if store_ev { FLAG_HAS_EV } else { 0 };
        node.actions = blob_actions;
        node.children = children;
        node.invest = invest;
        node.strategy = quantized;
        node.ev = ev;
        node.ev_scale = ev_scale;
        Ok(index)
    }

    fn write_files(&mut self) -> Result<()> {
        let start_street = self.options.header.street;
        for (key, blob) in &self.blobs {
            let relative = blob_path(*key, start_street)?;
            let bytes = blob.to_bytes()?;
            let stored = if self.options.compress {
                compress(&bytes)?
            } else {
                bytes.clone()
            };
            let name = if self.options.compress {
                format!("{relative}.br")
            } else {
                relative.clone()
            };
            let full = self.options.out_dir.join(&name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("디렉토리 생성 실패: {}", parent.display()))?;
            }
            std::fs::write(&full, &stored)
                .with_context(|| format!("blob 쓰기 실패: {}", full.display()))?;
            self.summary.bytes_raw += bytes.len() as u64;
            self.summary.bytes_stored += stored.len() as u64;
            self.summary.files.push(FileInfo {
                path: name,
                nodes: blob.nodes.len() as u32,
                bytes_raw: bytes.len() as u64,
                bytes_stored: stored.len() as u64,
            });
        }
        Ok(())
    }
}

fn blob_path(key: BlobKey, start_street: u8) -> Result<String> {
    Ok(match key {
        BlobKey::Start => match start_street {
            0 => "flop.bin".to_string(),
            1 => "turn.bin".to_string(),
            _ => "river.bin".to_string(),
        },
        BlobKey::Turn(card) => format!("turn/{}.bin", cards::card_to_string(card)?),
        BlobKey::River(turn, river) => format!(
            "river/{}/{}.bin",
            cards::card_to_string(turn)?,
            cards::card_to_string(river)?
        ),
    })
}

fn action_to_blob(action: &Action) -> BlobAction {
    let (kind, amount) = match *action {
        Action::Fold => (ACTION_FOLD, 0),
        Action::Check => (ACTION_CHECK, 0),
        Action::Call => (ACTION_CALL, 0),
        Action::Bet(amount) => (ACTION_BET, amount),
        Action::Raise(amount) => (ACTION_RAISE, amount),
        Action::AllIn(amount) => (ACTION_ALLIN, amount),
        Action::Chance(_) | Action::None => (ACTION_CHECK, 0),
    };
    BlobAction {
        kind,
        amount: amount as f32 / CHIP_SCALE as f32,
    }
}

/// brotli 레벨 9로 사전 압축한다 (§5.2). 서빙은 `Content-Encoding: br`.
pub fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 9, 22);
        writer.write_all(bytes)?;
        writer.flush()?;
    }
    Ok(out)
}

/// 테스트와 러너 검증용 압축 해제
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut reader = brotli::Decompressor::new(bytes, 4096);
    std::io::Read::read_to_end(&mut reader, &mut out)?;
    Ok(out)
}

pub fn write_manifest(path: &Path, manifest: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(manifest)?;
    std::fs::write(path, text)
        .with_context(|| format!("manifest 쓰기 실패: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_spec() {
        assert_eq!(blob_path(BlobKey::Start, 0).unwrap(), "flop.bin");
        assert_eq!(blob_path(BlobKey::Start, 1).unwrap(), "turn.bin");
        assert_eq!(blob_path(BlobKey::Start, 2).unwrap(), "river.bin");
        assert_eq!(blob_path(BlobKey::Turn(48), 0).unwrap(), "turn/As.bin");
        assert_eq!(
            blob_path(BlobKey::River(48, 3), 0).unwrap(),
            "river/As/2c.bin"
        );
    }

    #[test]
    fn brotli_round_trip() {
        let data: Vec<u8> = (0..10_000u32).map(|v| (v % 251) as u8).collect();
        let packed = compress(&data).unwrap();
        assert!(packed.len() < data.len());
        assert_eq!(decompress(&packed).unwrap(), data);
    }

    #[test]
    fn action_amounts_are_in_bb() {
        let action = action_to_blob(&Action::Bet(183));
        assert_eq!(action.kind, ACTION_BET);
        assert!((action.amount - 1.83).abs() < 1e-6);
        assert_eq!(action_to_blob(&Action::Fold).kind, ACTION_FOLD);
        assert_eq!(action_to_blob(&Action::Call).amount, 0.0);
    }
}
