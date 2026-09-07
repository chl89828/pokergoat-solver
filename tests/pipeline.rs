//! 솔브 -> blob 쓰기 -> 다시 읽기 라운드트립.
//!
//! 팟과 스택을 같게 둬서 트리를 작게 만든 플랍 게임을 돌린다.

use pokergoat_solver::blob::{
    dequantize_ev, dequantize_strategy, Blob, CHILD_IMPOSSIBLE, CHILD_NOT_STORED, FLAG_HAS_EV,
    FLAG_PRUNED, KIND_CHANCE, KIND_PLAYER,
};
use pokergoat_solver::config::JobConfig;
use pokergoat_solver::export::{decompress, export, ExportOptions, DEFAULT_PRUNE_EPSILON};
use pokergoat_solver::solve::run_solver;
use pokergoat_solver::{blob::BlobHeader, cards, tree, SOLVER_VERSION};

const JOB: &str = r#"{
    "board": "As7d2c",
    "ranges": ["AA,KK", "QQ,JJ"],
    "pot": 10,
    "effectiveStack": 10,
    "sizing": {
        "flop": { "oop": { "bet": [100] }, "ip": { "bet": [100] } },
        "turn": { "oop": { "bet": [100] }, "ip": { "bet": [100] } },
        "river": { "oop": { "bet": [100] }, "ip": { "bet": [100] } }
    },
    "allInThreshold": 0,
    "addAllInThreshold": 0,
    "mergingThreshold": 0,
    "iterations": 40,
    "targetExploitability": 0.5,
    "scenarioId": 42,
    "templateId": 7
}"#;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pokergoat-solver-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn solve_export_and_read_back() {
    let config = JobConfig::from_str(JOB).unwrap();
    let mut built = tree::build_game(&config).unwrap();
    built.game.allocate_memory(false);
    let outcome = run_solver(
        &mut built.game,
        config.iterations,
        config.target_exploitability_chips(),
        None,
        false,
    )
    .unwrap();
    assert!(outcome.iterations > 0);

    // 루트 전략과 EV를 엔진에서 직접 받아 둔다 (blob과 비교용)
    built.game.back_to_root();
    built.game.cache_normalized_weights();
    let engine_strategy = built.game.strategy();
    let engine_ev: Vec<f32> = built
        .game
        .expected_values_detail(0)
        .iter()
        .map(|v| v / 100.0)
        .collect();
    let n_hands = built.game.private_cards(0).len();
    let n_actions = built.game.available_actions().len();
    assert_eq!(n_actions, 2, "체크와 올인만 있어야 한다");

    let out = temp_dir("pipeline");
    let board = config.board_cards().unwrap();
    let header = BlobHeader {
        scenario_id: config.scenario_id,
        template_id: config.template_id,
        solver_version: SOLVER_VERSION,
        board: board.clone(),
        street: 0,
        starting_pot: config.pot as f32,
        effective_stack: config.effective_stack as f32,
        rake_percent: 0.0,
        rake_cap: 0.0,
        exploitability_pct: 0.0,
        iterations: outcome.iterations,
    };
    let summary = export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: false,
            prune_epsilon: DEFAULT_PRUNE_EPSILON,
            compress: true,
            out_dir: out.clone(),
        },
    )
    .unwrap();

    assert!(summary.node_count > 0);
    assert!(!summary.turn_cards.is_empty());
    assert!(summary
        .files
        .iter()
        .any(|f| f.path == "flop.bin.br"));

    // 플랍 blob을 다시 읽는다
    let packed = std::fs::read(out.join("flop.bin.br")).unwrap();
    let bytes = decompress(&packed).unwrap();
    let blob = Blob::parse(&bytes).unwrap();
    assert_eq!(blob.header.board, board);
    assert_eq!(blob.header.scenario_id, 42);
    assert_eq!(blob.header.template_id, 7);
    assert_eq!(blob.header.street, 0);
    assert_eq!(blob.hands[0].len(), n_hands);

    let root = &blob.nodes[0];
    assert_eq!(root.kind, KIND_PLAYER);
    assert_eq!(root.player, 0);
    assert_eq!(root.actions.len(), 2);
    assert_eq!(root.flags & FLAG_HAS_EV, FLAG_HAS_EV);

    // 전략 오차는 양자화 한 스텝 이내
    let decoded = dequantize_strategy(&root.strategy, n_actions, n_hands);
    for index in 0..n_actions * n_hands {
        assert!(
            (decoded[index] - engine_strategy[index]).abs() <= 1.0 / 255.0 + 1e-6,
            "전략 오차: {} vs {}",
            decoded[index],
            engine_strategy[index]
        );
    }
    // EV 오차는 스케일 한 스텝 이내
    let decoded_ev = dequantize_ev(&root.ev, root.ev_scale);
    for index in 0..n_actions * n_hands {
        assert!(
            (decoded_ev[index] - engine_ev[index]).abs() <= root.ev_scale + 1e-6,
            "EV 오차: {} vs {}",
            decoded_ev[index],
            engine_ev[index]
        );
    }

    // 찬스 노드는 카드 52칸을 채운다. 보드 카드는 불가능, 리버는 미저장.
    let chance = blob
        .nodes
        .iter()
        .find(|n| n.kind == KIND_CHANCE)
        .expect("플랍 blob에 턴 찬스 노드가 있어야 한다");
    assert_eq!(chance.children.len(), 52);
    for &card in &board {
        assert_eq!(chance.children[card as usize], CHILD_IMPOSSIBLE);
    }
    let dealt = chance
        .children
        .iter()
        .filter(|&&c| c != CHILD_IMPOSSIBLE && c != CHILD_NOT_STORED)
        .count();
    assert_eq!(dealt, 49, "보드 3장을 뺀 49장이 딜 가능해야 한다");

    // 턴 파일이 대표 카드마다 있고, 각 파일도 파싱된다
    for name in &summary.turn_cards {
        let path = out.join(format!("turn/{name}.bin.br"));
        assert!(path.exists(), "턴 파일 없음: {}", path.display());
        let turn_blob = Blob::parse(&decompress(&std::fs::read(&path).unwrap()).unwrap()).unwrap();
        assert_eq!(turn_blob.header.street, 1);
        assert_eq!(turn_blob.header.board.len(), 4);
        assert_eq!(
            turn_blob.header.board[3],
            cards::card_from_str(name).unwrap()
        );
        // 리버 찬스 노드는 자식을 담지 않는다
        for node in &turn_blob.nodes {
            if node.kind == KIND_CHANCE {
                assert!(node
                    .children
                    .iter()
                    .all(|&c| c == CHILD_IMPOSSIBLE || c == CHILD_NOT_STORED));
            }
        }
    }

    // 턴 동형 매핑이 있으면 대상은 반드시 대표 카드다
    for (card, repr) in &summary.turn_isomorphism {
        assert!(summary.turn_cards.contains(repr), "{card} -> {repr}");
    }

    std::fs::remove_dir_all(&out).ok();
}

const TURN_JOB: &str = r#"{
    "board": "As7d2cKh",
    "ranges": ["AA,KK", "QQ,JJ"],
    "pot": 10,
    "effectiveStack": 10,
    "sizing": {
        "turn": { "oop": { "bet": [100] }, "ip": { "bet": [100] } },
        "river": { "oop": { "bet": [100] }, "ip": { "bet": [100] } }
    },
    "allInThreshold": 0,
    "addAllInThreshold": 0,
    "mergingThreshold": 0,
    "iterations": 20,
    "storeRiver": true
}"#;

/// 턴에서 시작하고 리버까지 저장하는 경우. 파일 이름과 river 디렉토리 구조를 본다.
#[test]
fn turn_start_stores_river_files() {
    let config = JobConfig::from_str(TURN_JOB).unwrap();
    let mut built = tree::build_game(&config).unwrap();
    built.game.allocate_memory(false);
    run_solver(
        &mut built.game,
        config.iterations,
        config.target_exploitability_chips(),
        None,
        false,
    )
    .unwrap();

    let out = temp_dir("turn");
    let board = config.board_cards().unwrap();
    let header = BlobHeader {
        scenario_id: 0,
        template_id: 0,
        solver_version: SOLVER_VERSION,
        board: board.clone(),
        street: 1,
        starting_pot: config.pot as f32,
        effective_stack: config.effective_stack as f32,
        rake_percent: 0.0,
        rake_cap: 0.0,
        exploitability_pct: 0.0,
        iterations: 0,
    };
    let summary = export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: true,
            prune_epsilon: DEFAULT_PRUNE_EPSILON,
            compress: true,
            out_dir: out.clone(),
        },
    )
    .unwrap();

    assert!(summary.files.iter().any(|f| f.path == "turn.bin.br"));
    let river_files: Vec<_> = summary
        .files
        .iter()
        .filter(|f| f.path.starts_with("river/Kh/"))
        .collect();
    assert_eq!(river_files.len(), 48, "리버 파일은 48장이어야 한다");
    assert!(summary.turn_cards.is_empty(), "턴 시작이면 턴 파일이 없다");

    let turn_blob =
        Blob::parse(&decompress(&std::fs::read(out.join("turn.bin.br")).unwrap()).unwrap())
            .unwrap();
    assert_eq!(turn_blob.header.street, 1);
    let chance = turn_blob
        .nodes
        .iter()
        .find(|n| n.kind == KIND_CHANCE)
        .unwrap();
    let stored = chance
        .children
        .iter()
        .filter(|&&c| c != CHILD_IMPOSSIBLE && c != CHILD_NOT_STORED)
        .count();
    assert_eq!(stored, 48);

    let sample = &river_files[0].path;
    let river_blob =
        Blob::parse(&decompress(&std::fs::read(out.join(sample)).unwrap()).unwrap()).unwrap();
    assert_eq!(river_blob.header.street, 2);
    assert_eq!(river_blob.header.board.len(), 5);
    assert!(river_blob.nodes.iter().any(|n| n.kind == KIND_PLAYER));

    std::fs::remove_dir_all(&out).ok();
}

const MONOTONE_JOB: &str = r#"{
    "board": "AsKsQs",
    "ranges": ["AA,KK", "QQ,JJ"],
    "pot": 10,
    "effectiveStack": 10,
    "sizing": {
        "flop": { "oop": { "bet": [100] }, "ip": { "bet": [100] } },
        "turn": { "oop": { "bet": [100] }, "ip": { "bet": [100] } },
        "river": { "oop": { "bet": [100] }, "ip": { "bet": [100] } }
    },
    "allInThreshold": 0,
    "addAllInThreshold": 0,
    "mergingThreshold": 0,
    "iterations": 20
}"#;

/// 모노톤 플랍에서는 보드에 없는 세 수트가 하나로 접힌다.
/// 우리 동형 표가 엔진 대표 목록과 어긋나면 export가 경고를 남기므로 그걸로 검증한다.
#[test]
fn monotone_flop_uses_chance_isomorphism() {
    let config = JobConfig::from_str(MONOTONE_JOB).unwrap();
    let mut built = tree::build_game(&config).unwrap();
    built.game.allocate_memory(false);
    run_solver(
        &mut built.game,
        config.iterations,
        config.target_exploitability_chips(),
        None,
        false,
    )
    .unwrap();

    let out = temp_dir("monotone");
    let board = config.board_cards().unwrap();
    let header = BlobHeader {
        scenario_id: 0,
        template_id: 0,
        solver_version: SOLVER_VERSION,
        board,
        street: 0,
        starting_pot: config.pot as f32,
        effective_stack: config.effective_stack as f32,
        rake_percent: 0.0,
        rake_cap: 0.0,
        exploitability_pct: 0.0,
        iterations: 0,
    };
    let summary = export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: false,
            prune_epsilon: DEFAULT_PRUNE_EPSILON,
            compress: true,
            out_dir: out.clone(),
        },
    )
    .unwrap();

    assert!(
        summary.warnings.is_empty(),
        "동형 경고가 나오면 안 된다: {:?}",
        summary.warnings
    );
    // 대표 23장 (접힌 수트 13 + 남은 스페이드 10), 나머지 26장은 매핑으로
    assert_eq!(summary.turn_cards.len(), 23);
    assert_eq!(summary.turn_isomorphism.len(), 26);
    for (card, repr) in &summary.turn_isomorphism {
        assert_ne!(card, repr);
        assert!(summary.turn_cards.contains(repr));
        assert_eq!(card.chars().next(), repr.chars().next(), "랭크는 같아야 한다");
    }
    let turn_file_count = summary
        .files
        .iter()
        .filter(|f| f.path.starts_with("turn/"))
        .count();
    assert_eq!(turn_file_count, 23);

    std::fs::remove_dir_all(&out).ok();
}

/// 프룬 임계값을 1.0으로 올리면 루트를 뺀 모든 플레이어 노드가 잘린다.
/// 저도달 노드의 표현(플래그, 빈 본문, 자식 미저장)을 확인하는 용도다.
/// 압축 없이 쓰는 경로도 같이 본다.
#[test]
fn pruned_nodes_keep_only_the_header() {
    let config = JobConfig::from_str(TURN_JOB).unwrap();
    let mut built = tree::build_game(&config).unwrap();
    built.game.allocate_memory(false);
    run_solver(&mut built.game, 10, 0.0, None, false).unwrap();

    let out = temp_dir("prune");
    let board = config.board_cards().unwrap();
    let header = BlobHeader {
        scenario_id: 0,
        template_id: 0,
        solver_version: SOLVER_VERSION,
        board,
        street: 1,
        starting_pot: config.pot as f32,
        effective_stack: config.effective_stack as f32,
        rake_percent: 0.0,
        rake_cap: 0.0,
        exploitability_pct: 0.0,
        iterations: 0,
    };
    let summary = export(
        &mut built.game,
        &built.isomorphism,
        ExportOptions {
            header,
            store_river: false,
            prune_epsilon: 1.0,
            compress: false,
            out_dir: out.clone(),
        },
    )
    .unwrap();
    assert!(summary.pruned_nodes > 0);
    assert!(summary.files.iter().all(|f| f.path == "turn.bin"));

    let blob = Blob::parse(&std::fs::read(out.join("turn.bin")).unwrap()).unwrap();
    let root = &blob.nodes[0];
    assert_eq!(root.flags & FLAG_PRUNED, 0, "루트는 도달 1.0이라 남는다");
    let pruned = blob
        .nodes
        .iter()
        .find(|n| n.flags & FLAG_PRUNED != 0)
        .expect("잘린 노드가 있어야 한다");
    assert_eq!(pruned.kind, KIND_PLAYER);
    assert!(pruned.strategy.is_empty());
    assert!(pruned.ev.is_empty());
    assert!(!pruned.actions.is_empty());
    assert!(pruned.children.iter().all(|&c| c == CHILD_NOT_STORED));

    std::fs::remove_dir_all(&out).ok();
}
