//! 솔루션 blob v0 직렬화와 역직렬화 (설계서 §5.2).
//!
//! 바이트 순서는 전부 little-endian. 자세한 레이아웃은 `docs/blob-format.md`.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"GTOB";
pub const FORMAT_VERSION: u8 = 0;

/// 자식 슬롯 특수값
pub const CHILD_IMPOSSIBLE: u32 = u32::MAX;
/// 카드는 나올 수 있지만 서브트리를 이 배포에 담지 않았다 (리버 미저장, 프룬 등)
pub const CHILD_NOT_STORED: u32 = u32::MAX - 1;
/// 오프셋 없음
pub const OFFSET_NONE: u64 = u64::MAX;

pub const KIND_PLAYER: u8 = 0;
pub const KIND_CHANCE: u8 = 1;
pub const KIND_TERMINAL: u8 = 2;

pub const FLAG_PRUNED: u8 = 1;
pub const FLAG_HAS_EV: u8 = 2;
pub const FLAG_LOCKED: u8 = 4;

pub const PLAYER_NONE: u8 = 255;

pub const ACTION_FOLD: u8 = 0;
pub const ACTION_CHECK: u8 = 1;
pub const ACTION_CALL: u8 = 2;
pub const ACTION_BET: u8 = 3;
pub const ACTION_RAISE: u8 = 4;
pub const ACTION_ALLIN: u8 = 5;

/// 찬스 노드의 자식 슬롯 수 (카드 52장 고정)
pub const CHANCE_CHILDREN: usize = 52;

#[derive(Debug, Clone, PartialEq)]
pub struct BlobHeader {
    pub scenario_id: u32,
    pub template_id: u16,
    pub solver_version: u16,
    /// 우리 카드 인코딩
    pub board: Vec<u8>,
    pub street: u8,
    pub starting_pot: f32,
    pub effective_stack: f32,
    pub rake_percent: f32,
    pub rake_cap: f32,
    pub exploitability_pct: f32,
    pub iterations: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlobAction {
    pub kind: u8,
    /// bb 단위. 해당 스트리트에서 그 플레이어가 넣은 누적 금액 (엔진 표기 그대로)
    pub amount: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlobNode {
    pub id: u32,
    pub kind: u8,
    pub player: u8,
    pub flags: u8,
    pub actions: Vec<BlobAction>,
    pub children: Vec<u32>,
    /// [OOP, IP] 누적 투자 (bb)
    pub invest: [f32; 2],
    /// u8[(n_actions - 1) * hands], 액션 우선 배치. 비었으면 없음.
    pub strategy: Vec<u8>,
    /// i16[n_actions * hands], 액션 우선 배치. 비었으면 없음.
    pub ev: Vec<i16>,
    pub ev_scale: f32,
}

impl BlobNode {
    pub fn expected_children(kind: u8, n_actions: usize) -> usize {
        match kind {
            KIND_CHANCE => CHANCE_CHILDREN,
            KIND_TERMINAL => 0,
            _ => n_actions,
        }
    }

    fn record_len(&self) -> usize {
        4 + 1 + 1 + 1 + 1 + self.actions.len() * 5 + self.children.len() * 4 + 8 + 8 + 8 + 4
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Blob {
    pub header: BlobHeader,
    /// [OOP, IP] 핸드 목록 (우리 콤보 인덱스)
    pub hands: [Vec<u16>; 2],
    pub nodes: Vec<BlobNode>,
}

impl Blob {
    pub fn new(header: BlobHeader, hands: [Vec<u16>; 2]) -> Self {
        Self {
            header,
            hands,
            nodes: Vec::new(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.header.board.len() < 3 || self.header.board.len() > 5 {
            bail!("보드는 3~5장이어야 한다: {}장", self.header.board.len());
        }
        for node in &self.nodes {
            let expected = BlobNode::expected_children(node.kind, node.actions.len());
            if node.children.len() != expected {
                bail!(
                    "노드 {} 자식 수 불일치: {} (기대 {expected})",
                    node.id,
                    node.children.len()
                );
            }
            if node.actions.len() > 255 {
                bail!("노드 {} 액션 수 초과", node.id);
            }
        }

        let mut head = Vec::new();
        head.extend_from_slice(&MAGIC);
        head.push(FORMAT_VERSION);
        head.extend_from_slice(&self.header.scenario_id.to_le_bytes());
        head.extend_from_slice(&self.header.template_id.to_le_bytes());
        head.extend_from_slice(&self.header.solver_version.to_le_bytes());
        head.push(self.header.board.len() as u8);
        head.extend_from_slice(&self.header.board);
        head.push(self.header.street);
        head.extend_from_slice(&self.header.starting_pot.to_le_bytes());
        head.extend_from_slice(&self.header.effective_stack.to_le_bytes());
        head.extend_from_slice(&self.header.rake_percent.to_le_bytes());
        head.extend_from_slice(&self.header.rake_cap.to_le_bytes());
        head.extend_from_slice(&self.header.exploitability_pct.to_le_bytes());
        head.extend_from_slice(&self.header.iterations.to_le_bytes());

        for player in 0..2 {
            let hands = &self.hands[player];
            if hands.len() > u16::MAX as usize {
                bail!("핸드 수가 u16을 넘는다");
            }
            head.extend_from_slice(&(hands.len() as u16).to_le_bytes());
            for &combo in hands {
                head.extend_from_slice(&combo.to_le_bytes());
            }
        }

        head.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());

        let nodes_len: usize = self.nodes.iter().map(|n| n.record_len()).sum();
        let strategy_start = head.len() + nodes_len;
        let strategy_len: usize = self.nodes.iter().map(|n| n.strategy.len()).sum();
        let ev_start = strategy_start + strategy_len;

        let mut strategy_cursor = strategy_start as u64;
        let mut ev_cursor = ev_start as u64;

        let mut body = Vec::with_capacity(nodes_len);
        for node in &self.nodes {
            body.extend_from_slice(&node.id.to_le_bytes());
            body.push(node.kind);
            body.push(node.player);
            body.push(node.actions.len() as u8);
            body.push(node.flags);
            for action in &node.actions {
                body.push(action.kind);
                body.extend_from_slice(&action.amount.to_le_bytes());
            }
            for &child in &node.children {
                body.extend_from_slice(&child.to_le_bytes());
            }
            body.extend_from_slice(&node.invest[0].to_le_bytes());
            body.extend_from_slice(&node.invest[1].to_le_bytes());
            let strategy_offset = if node.strategy.is_empty() {
                OFFSET_NONE
            } else {
                let offset = strategy_cursor;
                strategy_cursor += node.strategy.len() as u64;
                offset
            };
            let ev_offset = if node.ev.is_empty() {
                OFFSET_NONE
            } else {
                let offset = ev_cursor;
                ev_cursor += (node.ev.len() * 2) as u64;
                offset
            };
            body.extend_from_slice(&strategy_offset.to_le_bytes());
            body.extend_from_slice(&ev_offset.to_le_bytes());
            body.extend_from_slice(&node.ev_scale.to_le_bytes());
        }

        let mut out = head;
        out.extend_from_slice(&body);
        for node in &self.nodes {
            out.extend_from_slice(&node.strategy);
        }
        for node in &self.nodes {
            for &value in &node.ev {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        Ok(out)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let magic = r.take(4)?;
        if magic != MAGIC {
            bail!("magic이 GTOB가 아니다");
        }
        let version = r.u8()?;
        if version != FORMAT_VERSION {
            bail!("지원하지 않는 blob 버전: {version}");
        }
        let scenario_id = r.u32()?;
        let template_id = r.u16()?;
        let solver_version = r.u16()?;
        let board_len = r.u8()? as usize;
        if !(3..=5).contains(&board_len) {
            bail!("보드 길이가 이상하다: {board_len}");
        }
        let board = r.take(board_len)?.to_vec();
        let street = r.u8()?;
        let header = BlobHeader {
            scenario_id,
            template_id,
            solver_version,
            board,
            street,
            starting_pot: r.f32()?,
            effective_stack: r.f32()?,
            rake_percent: r.f32()?,
            rake_cap: r.f32()?,
            exploitability_pct: r.f32()?,
            iterations: r.u32()?,
        };

        let mut hands: [Vec<u16>; 2] = [Vec::new(), Vec::new()];
        for player in 0..2 {
            let count = r.u16()? as usize;
            let mut list = Vec::with_capacity(count);
            for _ in 0..count {
                list.push(r.u16()?);
            }
            hands[player] = list;
        }

        let node_count = r.u32()? as usize;
        let mut nodes = Vec::with_capacity(node_count);
        let mut pending: Vec<(u64, u64)> = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let id = r.u32()?;
            let kind = r.u8()?;
            let player = r.u8()?;
            let n_actions = r.u8()? as usize;
            let flags = r.u8()?;
            let mut actions = Vec::with_capacity(n_actions);
            for _ in 0..n_actions {
                let kind = r.u8()?;
                let amount = r.f32()?;
                actions.push(BlobAction { kind, amount });
            }
            let n_children = BlobNode::expected_children(kind, n_actions);
            let mut children = Vec::with_capacity(n_children);
            for _ in 0..n_children {
                children.push(r.u32()?);
            }
            let invest = [r.f32()?, r.f32()?];
            let strategy_offset = r.u64()?;
            let ev_offset = r.u64()?;
            let ev_scale = r.f32()?;
            pending.push((strategy_offset, ev_offset));
            nodes.push(BlobNode {
                id,
                kind,
                player,
                flags,
                actions,
                children,
                invest,
                strategy: Vec::new(),
                ev: Vec::new(),
                ev_scale,
            });
        }

        for (index, (strategy_offset, ev_offset)) in pending.into_iter().enumerate() {
            let node = &mut nodes[index];
            let hands_len = if node.player < 2 {
                hands[node.player as usize].len()
            } else {
                0
            };
            if strategy_offset != OFFSET_NONE {
                let len = node.actions.len().saturating_sub(1) * hands_len;
                let start = strategy_offset as usize;
                let end = start + len;
                if end > bytes.len() {
                    bail!("전략 오프셋이 파일 밖을 가리킨다");
                }
                node.strategy = bytes[start..end].to_vec();
            }
            if ev_offset != OFFSET_NONE {
                let len = node.actions.len() * hands_len;
                let start = ev_offset as usize;
                let end = start + len * 2;
                if end > bytes.len() {
                    bail!("EV 오프셋이 파일 밖을 가리킨다");
                }
                node.ev = bytes[start..end]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
            }
        }

        Ok(Blob {
            header,
            hands,
            nodes,
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.bytes.len() {
            bail!("blob이 잘렸다 (offset {}, {len}바이트 요청)", self.pos);
        }
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f32(&mut self) -> Result<f32> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// 전략을 u8로 양자화한다. 핸드마다 액션 확률 합이 정확히 255가 되도록
/// 최대잔여법으로 배분하고 마지막 액션은 저장하지 않는다 (255 - 나머지 합).
pub fn quantize_strategy(strategy: &[f32], n_actions: usize, n_hands: usize) -> Vec<u8> {
    if n_actions <= 1 || n_hands == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; (n_actions - 1) * n_hands];
    let mut scaled = vec![0f64; n_actions];
    let mut quota = vec![0u16; n_actions];
    for hand in 0..n_hands {
        let mut sum = 0f64;
        for action in 0..n_actions {
            let value = strategy[action * n_hands + hand];
            let value = if value.is_finite() && value > 0.0 {
                value as f64
            } else {
                0.0
            };
            scaled[action] = value;
            sum += value;
        }
        if sum <= 0.0 {
            // 도달 0이거나 보드와 겹치는 핸드. 균등 분포로 채운다.
            for action in 0..n_actions {
                scaled[action] = 1.0;
            }
            sum = n_actions as f64;
        }
        let mut assigned = 0u16;
        for action in 0..n_actions {
            let exact = scaled[action] / sum * 255.0;
            let floor = exact.floor();
            quota[action] = floor as u16;
            assigned += floor as u16;
            scaled[action] = exact - floor;
        }
        while assigned < 255 {
            let mut best = 0usize;
            let mut best_value = -1.0;
            for action in 0..n_actions {
                if scaled[action] > best_value {
                    best_value = scaled[action];
                    best = action;
                }
            }
            quota[best] += 1;
            scaled[best] = -1.0;
            assigned += 1;
        }
        for action in 0..n_actions - 1 {
            out[action * n_hands + hand] = quota[action].min(255) as u8;
        }
    }
    out
}

/// 저장된 u8 전략을 확률로 되돌린다 (마지막 액션은 255에서 뺀 값).
pub fn dequantize_strategy(bytes: &[u8], n_actions: usize, n_hands: usize) -> Vec<f32> {
    let mut out = vec![0f32; n_actions * n_hands];
    if n_hands == 0 || n_actions == 0 {
        return out;
    }
    // 본문이 없는 노드(pruned)는 균등 전략으로 본다
    if n_actions > 1 && bytes.len() < (n_actions - 1) * n_hands {
        return vec![1.0 / n_actions as f32; n_actions * n_hands];
    }
    if n_actions == 1 {
        for hand in 0..n_hands {
            out[hand] = 1.0;
        }
        return out;
    }
    for hand in 0..n_hands {
        let mut rest = 255i32;
        for action in 0..n_actions - 1 {
            let value = bytes[action * n_hands + hand] as i32;
            out[action * n_hands + hand] = value as f32 / 255.0;
            rest -= value;
        }
        out[(n_actions - 1) * n_hands + hand] = rest.max(0) as f32 / 255.0;
    }
    out
}

/// EV를 i16 + 스케일로 양자화한다.
pub fn quantize_ev(values: &[f32]) -> (Vec<i16>, f32) {
    let max_abs = values
        .iter()
        .filter(|v| v.is_finite())
        .fold(0f32, |acc, v| acc.max(v.abs()));
    if max_abs <= 0.0 {
        return (vec![0i16; values.len()], 1.0);
    }
    let scale = max_abs / i16::MAX as f32;
    let encoded = values
        .iter()
        .map(|v| {
            if v.is_finite() {
                (v / scale).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
            } else {
                0
            }
        })
        .collect();
    (encoded, scale)
}

pub fn dequantize_ev(values: &[i16], scale: f32) -> Vec<f32> {
    values.iter().map(|&v| v as f32 * scale).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_blob() -> Blob {
        let header = BlobHeader {
            scenario_id: 7,
            template_id: 1,
            solver_version: 0,
            board: vec![48, 22, 3],
            street: 0,
            starting_pot: 5.5,
            effective_stack: 97.5,
            rake_percent: 5.0,
            rake_cap: 3.0,
            exploitability_pct: 0.27,
            iterations: 1000,
        };
        let mut blob = Blob::new(header, [vec![0, 5, 1325], vec![10, 20]]);

        let strategy_probs: Vec<f32> = vec![0.7, 0.25, 0.1, 0.3, 0.75, 0.9];
        let strategy = quantize_strategy(&strategy_probs, 2, 3);
        let (ev, ev_scale) = quantize_ev(&[1.5, -2.25, 0.0, 3.75, 4.0, -1.0]);
        blob.nodes.push(BlobNode {
            id: 0,
            kind: KIND_PLAYER,
            player: 0,
            flags: FLAG_HAS_EV,
            actions: vec![
                BlobAction {
                    kind: ACTION_CHECK,
                    amount: 0.0,
                },
                BlobAction {
                    kind: ACTION_BET,
                    amount: 1.83,
                },
            ],
            children: vec![1, 2],
            invest: [0.0, 0.0],
            strategy,
            ev,
            ev_scale,
        });
        blob.nodes.push(BlobNode {
            id: 1,
            kind: KIND_CHANCE,
            player: PLAYER_NONE,
            flags: 0,
            actions: Vec::new(),
            children: {
                let mut c = vec![CHILD_IMPOSSIBLE; CHANCE_CHILDREN];
                c[7] = 4;
                c[8] = CHILD_NOT_STORED;
                c
            },
            invest: [1.83, 1.83],
            strategy: Vec::new(),
            ev: Vec::new(),
            ev_scale: 1.0,
        });
        blob.nodes.push(BlobNode {
            id: 2,
            kind: KIND_TERMINAL,
            player: PLAYER_NONE,
            flags: 0,
            actions: Vec::new(),
            children: Vec::new(),
            invest: [1.83, 0.0],
            strategy: Vec::new(),
            ev: Vec::new(),
            ev_scale: 1.0,
        });
        blob
    }

    #[test]
    fn round_trip() {
        let blob = sample_blob();
        let bytes = blob.to_bytes().unwrap();
        assert_eq!(&bytes[..4], b"GTOB");
        let parsed = Blob::parse(&bytes).unwrap();
        assert_eq!(parsed, blob);
    }

    #[test]
    fn strategy_within_quantization_error() {
        let probs: Vec<f32> = vec![0.7, 0.25, 0.1, 0.3, 0.75, 0.9];
        let bytes = quantize_strategy(&probs, 2, 3);
        let back = dequantize_strategy(&bytes, 2, 3);
        for (a, b) in probs.iter().zip(back.iter()) {
            assert!((a - b).abs() <= 1.0 / 255.0, "{a} vs {b}");
        }
        // 각 핸드의 합은 정확히 1
        for hand in 0..3 {
            let sum: f32 = (0..2).map(|a| back[a * 3 + hand]).sum();
            assert!((sum - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn strategy_three_actions() {
        let probs: Vec<f32> = vec![0.2, 0.0, 0.5, 1.0, 0.3, 0.0];
        let bytes = quantize_strategy(&probs, 3, 2);
        assert_eq!(bytes.len(), 4);
        let back = dequantize_strategy(&bytes, 3, 2);
        for (a, b) in probs.iter().zip(back.iter()) {
            assert!((a - b).abs() <= 1.0 / 255.0, "{a} vs {b}");
        }
    }

    #[test]
    fn zero_strategy_becomes_uniform() {
        let probs: Vec<f32> = vec![0.0, 0.0, 0.0];
        let bytes = quantize_strategy(&probs, 3, 1);
        let back = dequantize_strategy(&bytes, 3, 1);
        for value in back {
            assert!((value - 1.0 / 3.0).abs() < 0.01);
        }
    }

    #[test]
    fn ev_within_scale() {
        let values = vec![1.5f32, -2.25, 0.0, 3.75, 120.0, -0.001];
        let (encoded, scale) = quantize_ev(&values);
        let back = dequantize_ev(&encoded, scale);
        for (a, b) in values.iter().zip(back.iter()) {
            assert!((a - b).abs() <= scale, "{a} vs {b} (scale {scale})");
        }
    }

    #[test]
    fn rejects_truncated_bytes() {
        let bytes = sample_blob().to_bytes().unwrap();
        assert!(Blob::parse(&bytes[..10]).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample_blob().to_bytes().unwrap();
        bytes[0] = b'X';
        assert!(Blob::parse(&bytes).is_err());
    }
}
