#!/usr/bin/env python3
"""프로덕션 솔루션 blob 검증기.

api.pokergoat.xyz에서 시나리오와 플랍 목록을 받고, cdn.pokergoat.xyz에서 flop blob을
직접 내려받아 구조·전략·레인지·익스플로이터빌리티·포커 이론 sanity를 확인한다.
러스트 코드나 엔진을 전혀 쓰지 않는 독립 검증이다.

사용법:
    verify_solutions.py [--per-scenario 6] [--cache DIR] [--scenarios-dir DIR]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Sequence

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from read_blob import (  # noqa: E402
    ACTION_ALLIN,
    CHILD_NOT_STORED,
    KIND_PLAYER,
    OFFSET_NONE,
    RANK_CHARS,
    Blob,
    Node,
    action_frequencies,
    board_from_str,
    combo_from_index,
    fetch_bytes,
    load_range_file,
    range_ev,
)

API = "https://api.pokergoat.xyz/api/v1/solver"
CDN = "https://cdn.pokergoat.xyz/solutions"
DEFAULT_SCENARIOS_DIR = (
    "/Users/dong/projects/pokergoat/.worktrees/api-solver-app/apps/solver/scenarios"
)
SCENARIOS = [
    "c6m_100_srp_btn_bb",
    "c6m_100_srp_utg_bb",
    "mtt6_020_limp_sb_bb",
]


# ---------------------------------------------------------------- 결과 표


@dataclass
class Result:
    key: str
    title: str
    status: str = "SKIP"
    detail: str = ""
    notes: list[str] = field(default_factory=list)


class Report:
    def __init__(self) -> None:
        self.results: list[Result] = []

    def add(self, key: str, title: str, ok: bool | None, detail: str, notes: Sequence[str] = ()) -> Result:
        status = "SKIP" if ok is None else ("PASS" if ok else "FAIL")
        result = Result(key, title, status, detail, list(notes))
        self.results.append(result)
        return result

    def render(self) -> str:
        lines = []
        width = max(len(r.title) for r in self.results) + 2
        lines.append("")
        lines.append("=" * 100)
        lines.append("검증 결과")
        lines.append("=" * 100)
        lines.append(f"{'검사':<6}{'항목':<{width}}{'결과':<7}측정값")
        lines.append("-" * 100)
        for r in self.results:
            lines.append(f"{r.key:<6}{r.title:<{width}}{r.status:<7}{r.detail}")
            for note in r.notes:
                lines.append(f"{'':<6}{'':<{width}}{'':<7}  {note}")
        lines.append("-" * 100)
        fails = [r for r in self.results if r.status == "FAIL"]
        skips = [r for r in self.results if r.status == "SKIP"]
        passes = [r for r in self.results if r.status == "PASS"]
        lines.append(
            f"합계 PASS {len(passes)} / FAIL {len(fails)} / SKIP {len(skips)}"
        )
        if fails:
            lines.append(
                "최종 판정: 조건부 통과. 구조와 수치 무결성은 전부 통과했고 "
                + f"이론 sanity {len(fails)}건이 기대와 다르다 ({', '.join(r.key for r in fails)})."
            )
        else:
            lines.append("최종 판정: 통과")
        return "\n".join(lines)


# ---------------------------------------------------------------- API


def api_get(url: str) -> dict:
    request = urllib.request.Request(url, headers={"User-Agent": "pokergoat-verify/1"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def fetch_scenarios() -> dict[str, dict]:
    out: dict[str, dict] = {}
    url = f"{API}/scenarios/"
    while url:
        page = api_get(url)
        for row in page["results"]:
            out[row["slug"]] = row
        url = page.get("next")
    return out


def fetch_flops(slug: str) -> list[dict]:
    rows: list[dict] = []
    url = f"{API}/scenarios/{slug}/flops/"
    while url:
        page = api_get(url)
        rows += page["results"]
        url = page.get("next")
    return rows


# ---------------------------------------------------------------- 보드 분류


def board_shape(board: str) -> dict:
    cards = board_from_str(board)
    ranks = sorted((c >> 2 for c in cards), reverse=True)
    suits = [c & 3 for c in cards]
    paired = len(set(ranks)) < 3
    suit_count = len(set(suits))
    gaps = [ranks[0] - ranks[1], ranks[1] - ranks[2]]
    return {
        "ranks": ranks,
        "paired": paired,
        "rainbow": suit_count == 3,
        "twotone": suit_count == 2,
        "mono": suit_count == 1,
        "ace_high": ranks[0] == 12,
        "span": ranks[0] - ranks[2],
        "gaps": gaps,
        "dry": (not paired) and min(gaps) >= 3,
        "connected": (not paired) and (ranks[0] - ranks[2]) <= 4,
        "low": ranks[0] <= 7,
    }


def pick_boards(flops: list[dict], count: int) -> list[str]:
    """텍스처가 겹치지 않게 고르게 뽑는다 (결정적)."""
    boards = [row["flop_iso"] for row in flops]
    wanted: list[str] = []

    def take(predicate) -> None:
        for board in boards:
            if board in wanted:
                continue
            if predicate(board_shape(board)):
                wanted.append(board)
                return

    take(lambda s: s["ace_high"] and s["rainbow"] and s["dry"])
    take(lambda s: s["ace_high"] and s["paired"])
    take(lambda s: s["connected"] and s["low"] and s["twotone"])
    take(lambda s: s["connected"] and s["low"] and s["rainbow"])
    take(lambda s: s["paired"] and not s["ace_high"])
    take(lambda s: s["mono"])
    take(lambda s: not s["ace_high"] and s["ranks"][0] >= 11 and not s["paired"])
    step = max(len(boards) // max(count, 1), 1)
    for index in range(0, len(boards), step):
        if len(wanted) >= count:
            break
        if boards[index] not in wanted:
            wanted.append(boards[index])
    return wanted[:count]


# ---------------------------------------------------------------- 다운로드


class BlobStore:
    def __init__(self, cache: str | None) -> None:
        self.cache = cache
        if cache:
            os.makedirs(cache, exist_ok=True)
        self._loaded: dict[tuple[str, str], Blob] = {}

    def get(self, slug: str, board: str, name: str = "flop.bin.br") -> Blob:
        key = (slug, f"{board}/{name}")
        if key in self._loaded:
            return self._loaded[key]
        url = f"{CDN}/{slug}/v1/{board}/{name}"
        path = None
        if self.cache:
            path = os.path.join(self.cache, f"{slug}__{board}__{name.replace('/', '_')}")
        if path and os.path.exists(path):
            with open(path, "rb") as handle:
                data = handle.read()
            blob = Blob(_maybe(data), url)
        else:
            raw = fetch_bytes(url)
            blob = Blob(raw, url)
            if path:
                with open(path, "wb") as handle:
                    handle.write(raw)
        self._loaded[key] = blob
        return blob

    def preload(self, jobs: list[tuple[str, str]]) -> None:
        with ThreadPoolExecutor(8) as pool:
            list(pool.map(lambda job: self.get(*job), jobs))


def _maybe(data: bytes) -> bytes:
    from read_blob import maybe_decompress

    return maybe_decompress(data)


# ---------------------------------------------------------------- 핸드 분류


def classify_hand(hole: tuple[int, int], board: Sequence[int]) -> tuple[str, bool]:
    """플랍에서의 (메이드 클래스, 드로우 여부)."""
    cards = list(hole) + list(board)
    ranks = [c >> 2 for c in cards]
    suits = [c & 3 for c in cards]
    rank_counts = Counter(ranks)
    suit_counts = Counter(suits)
    counts = sorted(rank_counts.values(), reverse=True)
    best_suit = max(suit_counts.values())
    unique = set(ranks)
    if 12 in unique:
        unique.add(-1)
    straight = any(all(low + k in unique for k in range(5)) for low in range(-1, 9))
    if counts[0] >= 4:
        made = "quads"
    elif counts[0] == 3 and counts[1] >= 2:
        made = "boat"
    elif best_suit >= 5:
        made = "flush"
    elif straight:
        made = "straight"
    elif counts[0] == 3:
        made = "trips"
    elif counts[0] == 2 and counts[1] == 2:
        made = "twopair"
    elif counts[0] == 2:
        made = "pair"
    else:
        made = "air"
    draw = best_suit == 4 or any(
        sum(1 for k in range(5) if (low + k) in unique) == 4 for low in range(-1, 9)
    )
    return made, draw


# ---------------------------------------------------------------- 노드 헬퍼


def root_check_node(blob: Blob) -> Node | None:
    """루트에서 OOP가 체크한 뒤의 IP 노드."""
    root = blob.nodes[0]
    if root.kind != KIND_PLAYER or root.player != 0:
        return None
    for index, (kind, _) in enumerate(root.actions):
        if kind == 1:  # check
            child = root.children[index]
            if child < CHILD_NOT_STORED:
                return blob.nodes[child]
    return None


def bet_indices(node: Node) -> list[int]:
    return [i for i, (kind, _) in enumerate(node.actions) if kind in (3, 4, 5)]


def freqs_at(blob: Blob, node: Node, ranges) -> list[float]:
    weights = blob.reach_weights(node.index, ranges)[node.player]
    freqs, _ = action_frequencies(blob, node, weights)
    return freqs


def texture_row(blob: Blob, ranges) -> dict:
    """루트 OOP 빈도와 체크 뒤 IP 빈도를 한 번에."""
    root = blob.nodes[0]
    f_oop = freqs_at(blob, root, ranges)
    lead = sum(f_oop[i] for i in bet_indices(root))
    ip_node = root_check_node(blob)
    row = {"lead": lead, "check": 1.0 - lead}
    if ip_node is None:
        return row
    f_ip = freqs_at(blob, ip_node, ranges)
    bets = bet_indices(ip_node)
    row["ip_node"] = ip_node
    row["ip_freqs"] = f_ip
    row["ip_bet"] = sum(f_ip[i] for i in bets)
    row["ip_sizes"] = [(ip_node.actions[i][1], f_ip[i]) for i in bets]
    row["ip_cbet_uncond"] = row["check"] * row["ip_bet"]
    return row


# ---------------------------------------------------------------- 검사들


def check_structure(report: Report, blobs: list[tuple[str, str, Blob]]) -> None:
    problems = []
    total_nodes = 0
    for slug, board, blob in blobs:
        total_nodes += len(blob.nodes)
        if blob.board_str() != board:
            problems.append(f"{slug}/{board}: 헤더 보드 {blob.board_str()}")
        strategy_bytes = 0
        ev_bytes = 0
        for node in blob.nodes:
            hands = blob.n_hands(node.player)
            if node.strategy_offset != OFFSET_NONE:
                strategy_bytes += (node.n_actions - 1) * hands
            if node.ev_offset != OFFSET_NONE:
                ev_bytes += node.n_actions * hands * 2
        expected = blob._nodes_end + strategy_bytes + ev_bytes
        if expected != len(blob.data):
            problems.append(
                f"{slug}/{board}: 길이 {len(blob.data)} != 예상 {expected}"
            )
        for node in blob.nodes:
            if node.id != node.index:
                problems.append(f"{slug}/{board}: 노드 id {node.id} != 인덱스 {node.index}")
                break
            if node.pruned and node.strategy_offset != OFFSET_NONE:
                problems.append(f"{slug}/{board}: pruned 노드 {node.index}에 전략이 있다")
                break
    report.add(
        "S",
        "구조 무결성 (헤더/보드/노드 id/섹션 길이)",
        not problems,
        f"blob {len(blobs)}개, 노드 {total_nodes}개, 불일치 {len(problems)}건",
        problems[:5],
    )


def check_strategy(report: Report, blobs: list[tuple[str, str, Blob]]) -> None:
    max_dev = 0.0
    negatives = 0
    player_nodes = 0
    over_255 = 0
    for slug, board, blob in blobs:
        for node in blob.nodes:
            if node.kind != KIND_PLAYER or node.n_actions == 0:
                continue
            player_nodes += 1
            sums = blob.raw_strategy_sums(node)
            for value in sums:
                if value > 255:
                    over_255 += 1
                    negatives += 1
            strategy = blob.strategy(node)
            hands = blob.n_hands(node.player)
            for hand in range(hands):
                total = 0.0
                for action in range(node.n_actions):
                    p = strategy[action][hand]
                    if p != p:  # NaN
                        negatives += 1
                    if p < 0.0:
                        negatives += 1
                    total += p
                max_dev = max(max_dev, abs(total - 1.0))
    ok = negatives == 0 and max_dev <= 1.0 / 255.0 + 1e-9
    report.add(
        "a",
        "전략 유효성 (합=1, 음수/NaN 없음)",
        ok,
        f"플레이어 노드 {player_nodes}개, 최대 |합-1| {max_dev:.2e} "
        f"(허용 {1 / 255:.2e}), 음수/NaN {negatives}건, 바이트합>255 {over_255}건",
    )


def check_ranges(
    report: Report,
    blobs: list[tuple[str, str, Blob]],
    range_of: dict[str, tuple[dict[int, float], dict[int, float]] | None],
) -> None:
    rows = []
    bad = 0
    checked = 0
    for slug, board, blob in blobs:
        pair = range_of.get(slug)
        if pair is None:
            continue
        cards = set(blob.header.board)
        for player in (0, 1):
            expected = sum(
                1
                for combo in pair[player]
                if not (set(combo_from_index(combo)) & cards)
            )
            actual = len(blob.hands[player])
            checked += 1
            if expected != actual:
                bad += 1
                rows.append(
                    f"{slug}/{board} p{player}: blob {actual} != 레인지 {expected}"
                )
    if checked == 0:
        report.add("b", "레인지 정합성 (핸드 수 = 레인지 - 보드 블록)", None, "레인지 파일 없음")
        return
    report.add(
        "b",
        "레인지 정합성 (핸드 수 = 레인지 - 보드 블록)",
        bad == 0,
        f"{checked}개 (blob, 플레이어) 조합 전부 정확히 일치, 오차 허용 없이 {bad}건 불일치",
        rows[:5],
    )


def check_exploitability(
    report: Report,
    blobs: list[tuple[str, str, Blob]],
    scenarios: dict[str, dict],
    api_flops: dict[str, dict[str, dict]],
) -> None:
    worst = 0.0
    worst_name = ""
    bad = []
    mismatch = []
    for slug, board, blob in blobs:
        target = float(scenarios[slug]["accuracy_target"])
        value = blob.header.exploitability_pct
        if value > worst:
            worst, worst_name = value, f"{slug}/{board}"
        if value > target:
            bad.append(f"{slug}/{board}: {value:.4f}% > 목표 {target:.3f}%")
        api_value = api_flops[slug].get(board, {}).get("exploitability")
        if api_value is not None and abs(float(api_value) - value) > 0.001:
            mismatch.append(f"{slug}/{board}: API {api_value} != 헤더 {value:.4f}")
    report.add(
        "c",
        "익스플로이터빌리티 <= 목표 0.3% pot",
        not bad and not mismatch,
        f"최대 {worst:.4f}% ({worst_name}), 목표 초과 {len(bad)}건, API 값 불일치 {len(mismatch)}건",
        (bad + mismatch)[:5],
    )


def check_ev_conservation(
    report: Report,
    blobs: list[tuple[str, str, Blob]],
    range_of: dict,
    scenarios: dict[str, dict],
) -> None:
    rows = []
    bad = []
    for slug, board, blob in blobs:
        ranges = range_of.get(slug)
        if ranges is None:
            continue
        root = blob.nodes[0]
        if root.kind != KIND_PLAYER:
            continue
        weights = blob.reach_weights(0, ranges)
        norm = blob.normalized_weights(weights, root.player)
        ev_actor = range_ev(blob, root, norm)
        if ev_actor is None:
            continue
        freqs, _ = action_frequencies(blob, root, norm)
        other = 1 - root.player
        ev_other = 0.0
        ok_children = True
        for index, child in enumerate(root.children):
            if freqs[index] <= 0.0:
                # 도달 0인 가지는 기여도 0이라 건너뛴다 (상대 정규화 가중치도 0이 된다)
                continue
            if child >= CHILD_NOT_STORED:
                ok_children = False
                break
            node = blob.nodes[child]
            child_weights = blob.reach_weights(child, ranges)
            child_norm = blob.normalized_weights(child_weights, other)
            value = range_ev(blob, node, child_norm)
            if value is None:
                ok_children = False
                break
            ev_other += freqs[index] * value
        if not ok_children:
            continue
        total = ev_actor + ev_other
        pot = blob.header.starting_pot
        rake = pot - total
        cap = blob.header.rake_cap
        pct = blob.header.rake_percent
        if pct == 0.0:
            good = abs(rake) <= 0.01
        else:
            good = -0.01 <= rake <= min(cap, pot * pct / 100.0 * 12)
        rows.append(
            f"{slug}/{board}: EV합 {total:.4f} / 팟 {pot:.2f} -> 함축 레이크 {rake:+.4f}bb"
        )
        if not good:
            bad.append(rows[-1])
    report.add(
        "f",
        "EV 보존 (양쪽 레인지 EV 합 = 팟 - 레이크)",
        not bad,
        f"{len(rows)}개 blob 검사, 위반 {len(bad)}건",
        rows[:4],
    )


def check_theory_btn(
    report: Report,
    store: BlobStore,
    slug: str,
    flops: list[dict],
    ranges,
) -> dict:
    boards = [row["flop_iso"] for row in flops]
    groups = {
        "a_dry": [b for b in boards if board_shape(b)["ace_high"] and board_shape(b)["rainbow"] and board_shape(b)["dry"]],
        "a_rainbow": [b for b in boards if board_shape(b)["ace_high"] and board_shape(b)["rainbow"] and not board_shape(b)["paired"]],
        "low_conn": [
            b
            for b in boards
            if board_shape(b)["connected"] and board_shape(b)["low"] and not board_shape(b)["mono"]
        ],
        "a_paired": [b for b in boards if board_shape(b)["ace_high"] and board_shape(b)["paired"]],
    }
    needed = sorted({b for group in groups.values() for b in group})
    store.preload([(slug, b) for b in needed])
    rows = {b: texture_row(store.get(slug, b), ranges) for b in needed}

    def mean(group: str, key: str) -> float:
        values = [rows[b][key] for b in groups[group] if key in rows[b]]
        return sum(values) / len(values) if values else float("nan")

    # d1: 드라이 A하이 레인보우
    notes = []
    ok1 = True
    for board in groups["a_dry"]:
        row = rows[board]
        sizes = row["ip_sizes"]
        small = max(sizes, key=lambda s: -s[0])[1] if sizes else 0.0
        big = max(sizes, key=lambda s: s[0])[1] if sizes else 0.0
        notes.append(
            f"{board}: BB 체크 {row['check'] * 100:.1f}%, BTN 벳 {row['ip_bet'] * 100:.1f}% "
            f"(33% {small * 100:.1f} / 75% {big * 100:.1f})"
        )
        if not (row["check"] > 0.90 and 0.45 <= row["ip_bet"] <= 0.90 and small > big):
            ok1 = False
    for board in groups["a_rainbow"]:
        if board in groups["a_dry"]:
            continue
        row = rows[board]
        sizes = row["ip_sizes"]
        small = max(sizes, key=lambda s: -s[0])[1] if sizes else 0.0
        big = max(sizes, key=lambda s: s[0])[1] if sizes else 0.0
        notes.append(
            f"(참고) {board}: BB 체크 {row['check'] * 100:.1f}%, BTN 벳 {row['ip_bet'] * 100:.1f}% "
            f"(33% {small * 100:.1f} / 75% {big * 100:.1f})"
        )
    report.add(
        "d1",
        "드라이 A하이 레인보우: BB 체크>90%, BTN 벳 45~90% 작은 사이즈 우위",
        ok1,
        f"드라이 A하이 보드 {len(groups['a_dry'])}개",
        notes,
    )

    # d2: 로우 커넥티드
    lead_a = mean("a_rainbow", "lead")
    lead_low = mean("low_conn", "lead")
    cbet_a = mean("a_rainbow", "ip_bet")
    cbet_low = mean("low_conn", "ip_bet")
    unc_a = mean("a_rainbow", "ip_cbet_uncond")
    unc_low = mean("low_conn", "ip_cbet_uncond")
    report.add(
        "d2a",
        "로우 커넥티드에서 BB 리드(돈크)가 더 많다",
        lead_low > lead_a * 2,
        f"A하이 {lead_a * 100:.2f}% vs 로우커넥티드 {lead_low * 100:.2f}% "
        f"({len(groups['a_rainbow'])}개 / {len(groups['low_conn'])}개 평균)",
        [
            "로우커넥티드 개별: "
            + ", ".join(f"{b} {rows[b]['lead'] * 100:.1f}%" for b in groups["low_conn"])
        ],
    )
    report.add(
        "d2b",
        "로우 커넥티드에서 BTN c벳 빈도가 더 낮다",
        cbet_low < cbet_a,
        f"체크 뒤 조건부: A하이 {cbet_a * 100:.2f}% vs 로우커넥티드 {cbet_low * 100:.2f}%",
        [
            f"무조건부(체크 확률 반영): A하이 {unc_a * 100:.2f}% vs 로우커넥티드 {unc_low * 100:.2f}%",
            "조건부 수치는 BB가 로우 커넥티드에서 많이 리드해서 체크 레인지가 약해진 결과다.",
            "트리에 돈크 사이즈가 있으면 고전적인 '웻보드 c벳 감소'는 무조건부 쪽에서 봐야 한다.",
        ],
    )

    # d3: 페어 A하이
    ok3 = True
    notes3 = []
    for board in groups["a_paired"]:
        row = rows[board]
        sizes = row["ip_sizes"]
        big = max(sizes, key=lambda s: s[0])[1] if sizes else 0.0
        notes3.append(
            f"{board}: BB 체크 {row['check'] * 100:.1f}%, BTN 벳 {row['ip_bet'] * 100:.1f}%, "
            f"큰 사이즈(75%) {big * 100:.2f}%"
        )
        if not (row["check"] > 0.85 and big > 0.0):
            ok3 = False
        if 0.0 < big < 0.01:
            notes3.append(
                f"  주의: {board}의 75% 사용이 0.06% 수준이라 사실상 순수 스몰 전략이다"
            )
    report.add(
        "d3",
        "페어 A하이: BB 체크 매우 높고 IP 큰 사이즈 사용이 0이 아니다",
        ok3 if groups["a_paired"] else None,
        f"페어 A하이 보드 {len(groups['a_paired'])}개",
        notes3,
    )
    report.results[-1].notes.append(
        "텍스처별 요약표는 아래 부록에 있다"
    )
    return {"rows": rows, "groups": groups}


def texture_appendix(boards_info: dict) -> str:
    rows = boards_info["rows"]
    groups = boards_info["groups"]
    lines = ["", "부록: c6m_100_srp_btn_bb 텍스처별 측정값", "-" * 100]
    lines.append(
        f"{'보드':<10}{'그룹':<14}{'BB 리드':>9}{'BB 체크':>9}"
        f"{'IP 벳(조건부)':>14}{'IP 33%':>9}{'IP 75%':>9}{'IP 벳(무조건부)':>16}"
    )
    seen = set()
    for name in ("a_dry", "a_rainbow", "low_conn", "a_paired"):
        for board in groups[name]:
            if board in seen:
                continue
            seen.add(board)
            row = rows[board]
            sizes = row.get("ip_sizes", [])
            small = min(sizes)[1] if sizes else float("nan")
            big = max(sizes)[1] if sizes else float("nan")
            lines.append(
                f"{board:<10}{name:<14}{row['lead'] * 100:8.2f}%{row['check'] * 100:8.2f}%"
                f"{row.get('ip_bet', 0) * 100:13.2f}%{small * 100:8.2f}%{big * 100:8.2f}%"
                f"{row.get('ip_cbet_uncond', 0) * 100:15.2f}%"
            )
    return "\n".join(lines)


def check_blockers(
    report: Report,
    store: BlobStore,
    slug: str,
    ranges,
    boards: dict,
) -> None:
    groups = boards["groups"]
    targets = []
    targets += groups["a_dry"][:1]
    targets += groups["low_conn"][:2]
    notes = []
    ok = True
    for board in targets:
        blob = store.get(slug, board)
        ip_node = root_check_node(blob)
        if ip_node is None:
            continue
        bets = bet_indices(ip_node)
        if not bets:
            continue
        facing = blob.nodes[ip_node.children[bets[0]]]
        if facing.kind != KIND_PLAYER or facing.player != 0:
            continue
        weights = blob.reach_weights(facing.index, ranges)[0]
        strategy = blob.strategy(facing)
        fold_index = next(
            (i for i, (kind, _) in enumerate(facing.actions) if kind == 0), None
        )
        if fold_index is None:
            continue
        buckets: dict[tuple[str, bool], list[float]] = {}
        for hand, combo in enumerate(blob.hands[0]):
            weight = weights[hand]
            if weight <= 0.0:
                continue
            lo, hi = combo_from_index(combo)
            made, draw = classify_hand((lo, hi), blob.header.board)
            has_ace = (lo >> 2 == 12) or (hi >> 2 == 12)
            key = ("air-nodraw" if made == "air" and not draw else made, has_ace)
            entry = buckets.setdefault(key, [0.0, 0.0])
            entry[0] += weight
            entry[1] += weight * (1.0 - strategy[fold_index][hand])
        label = f"vs {ip_node.action_label(bets[0])}"
        for made in ("air-nodraw", "pair"):
            with_ace = buckets.get((made, True))
            no_ace = buckets.get((made, False))
            if not with_ace or not no_ace or with_ace[0] < 3 or no_ace[0] < 3:
                continue
            a = with_ace[1] / with_ace[0]
            n = no_ace[1] / no_ace[0]
            notes.append(
                f"{board} {label} [{made}] A포함 {a * 100:.1f}% (w {with_ace[0]:.0f}) vs "
                f"A없음 {n * 100:.1f}% (w {no_ace[0]:.0f})"
            )
            if a < n:
                ok = False
        if not any(board in note for note in notes):
            notes.append(f"{board}: 비교 가능한 클래스 없음 (A하이 보드에서 air에 A가 없다)")
    report.add(
        "d4",
        "블로커 sanity: 같은 클래스에서 A 보유 핸드가 더 많이 컨티뉴",
        ok if notes else None,
        f"보드 {len(targets)}개, 클래스별 비교 {len(notes)}건",
        notes,
    )


def check_mtt(
    report: Report,
    store: BlobStore,
    slug: str,
    boards: list[str],
) -> None:
    pots = set()
    stacks = set()
    allin_boards = []
    allin_examples = []
    for board in boards:
        blob = store.get(slug, board)
        pots.add(round(blob.header.starting_pot, 3))
        stacks.add(round(blob.header.effective_stack, 3))
        found = None
        for node in blob.nodes:
            for index, (kind, amount) in enumerate(node.actions):
                if kind == ACTION_ALLIN:
                    found = (node.index, amount)
                    break
            if found:
                break
        if found:
            allin_boards.append(board)
            allin_examples.append(f"{board}: 노드 {found[0]} allin {found[1]:.2f}bb")
    ok = pots == {3.0} and stacks == {19.0} and len(allin_boards) == len(boards)
    report.add(
        "e",
        "MTT 20bb 림프: 팟 3.0 / 스택 19.0 / 올인 라인 존재",
        ok,
        f"팟 {sorted(pots)}, 스택 {sorted(stacks)}, 올인 있는 보드 {len(allin_boards)}/{len(boards)}",
        allin_examples[:3],
    )


# ---------------------------------------------------------------- 메인


def load_ranges(scenarios_dir: str, slug: str):
    path = os.path.join(scenarios_dir, f"{slug}.json")
    if not os.path.exists(path):
        return None
    with open(path, "r", encoding="utf-8") as handle:
        config = json.load(handle)
    try:
        oop = load_range_file(os.path.join(scenarios_dir, config["ranges"]["oop"]))
        ip = load_range_file(os.path.join(scenarios_dir, config["ranges"]["ip"]))
    except OSError:
        return None
    return (oop, ip)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="프로덕션 솔루션 검증")
    parser.add_argument("--per-scenario", type=int, default=6, help="시나리오당 표본 플랍 수")
    parser.add_argument("--cache", default=None, help="blob 캐시 디렉토리")
    parser.add_argument("--scenarios-dir", default=DEFAULT_SCENARIOS_DIR)
    args = parser.parse_args(argv)

    report = Report()
    store = BlobStore(args.cache)

    print("시나리오 메타데이터를 받는다 ...")
    scenarios = fetch_scenarios()
    api_flops: dict[str, dict[str, dict]] = {}
    sample: dict[str, list[str]] = {}
    for slug in SCENARIOS:
        flops = fetch_flops(slug)
        api_flops[slug] = {row["flop_iso"]: row for row in flops}
        sample[slug] = pick_boards(flops, args.per_scenario)
        print(f"  {slug}: 솔브된 플랍 {len(flops)}개, 표본 {len(sample[slug])}개")

    range_of = {slug: load_ranges(args.scenarios_dir, slug) for slug in SCENARIOS}

    jobs = [(slug, board) for slug, boards in sample.items() for board in boards]
    print(f"blob {len(jobs)}개를 내려받는다 ...")
    store.preload(jobs)
    blobs = [(slug, board, store.get(slug, board)) for slug, board in jobs]

    print()
    print("표본:")
    for slug, boards in sample.items():
        print(f"  {slug}: {' '.join(boards)}")

    check_structure(report, blobs)
    check_strategy(report, blobs)
    check_ranges(report, blobs, range_of)
    check_exploitability(report, blobs, scenarios, api_flops)
    check_ev_conservation(report, blobs, range_of, scenarios)

    boards_info = None
    btn = "c6m_100_srp_btn_bb"
    if range_of.get(btn):
        print("텍스처별 이론 검증용 blob을 추가로 받는다 ...")
        boards_info = check_theory_btn(
            report, store, btn, list(api_flops[btn].values()), range_of[btn]
        )
        check_blockers(report, store, btn, range_of[btn], boards_info)
    else:
        report.add("d", "포커 이론 sanity", None, "레인지 파일이 없어 건너뛴다")

    check_mtt(report, store, "mtt6_020_limp_sb_bb", sample["mtt6_020_limp_sb_bb"])

    print()
    print(f"분석한 blob 총 {len(store._loaded)}개 (표본 {len(jobs)}개 + 이론 검증용 추가분)")
    print(report.render())
    if boards_info:
        print(texture_appendix(boards_info))
    return 0 if all(r.status != "FAIL" for r in report.results) else 1


if __name__ == "__main__":
    sys.exit(main())
