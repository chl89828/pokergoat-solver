#!/usr/bin/env python3
"""솔루션 blob v0 독립 리더.

docs/blob-format.md 스펙과 src/blob.rs 작성기를 그대로 옮긴 파서다.
러스트 코드를 전혀 쓰지 않고 CDN에 올라간 파일만으로 내용을 확인하는 것이 목적이다.

사용법:
    read_blob.py <url-or-path> [--node N] [--line 0.1.2] [--top 15]

URL이면 Accept-Encoding: identity로 받아 저장된 바이트 그대로 가져온 뒤 brotli로 푼다.
"""

from __future__ import annotations

import argparse
import os
import struct
import sys
from dataclasses import dataclass, field
from typing import Iterable, Sequence

MAGIC = b"GTOB"
FORMAT_VERSION = 0

KIND_PLAYER = 0
KIND_CHANCE = 1
KIND_TERMINAL = 2

FLAG_PRUNED = 1
FLAG_HAS_EV = 2
FLAG_LOCKED = 4

PLAYER_NONE = 255

ACTION_FOLD = 0
ACTION_CHECK = 1
ACTION_CALL = 2
ACTION_BET = 3
ACTION_RAISE = 4
ACTION_ALLIN = 5

ACTION_NAMES = {
    ACTION_FOLD: "fold",
    ACTION_CHECK: "check",
    ACTION_CALL: "call",
    ACTION_BET: "bet",
    ACTION_RAISE: "raise",
    ACTION_ALLIN: "allin",
}

CHILD_IMPOSSIBLE = 0xFFFFFFFF
CHILD_NOT_STORED = 0xFFFFFFFE
OFFSET_NONE = 0xFFFFFFFFFFFFFFFF

CHANCE_CHILDREN = 52
NUM_COMBOS = 1326

RANK_CHARS = "23456789TJQKA"
SUIT_CHARS = "shdc"
STREET_NAMES = {0: "flop", 1: "turn", 2: "river"}


# ---------------------------------------------------------------- 카드 유틸


def card_to_str(card: int) -> str:
    return f"{RANK_CHARS[card >> 2]}{SUIT_CHARS[card & 3]}"


def card_from_str(text: str) -> int:
    rank = RANK_CHARS.index(text[0].upper())
    suit = SUIT_CHARS.index(text[1].lower())
    return rank * 4 + suit


def board_to_str(board: Sequence[int]) -> str:
    return "".join(card_to_str(c) for c in board)


def board_from_str(text: str) -> list[int]:
    text = text.strip()
    return [card_from_str(text[i : i + 2]) for i in range(0, len(text), 2)]


def combo_from_index(index: int) -> tuple[int, int]:
    """콤보 인덱스 -> (lo, hi) 카드 두 장."""
    hi = 1
    while hi * (hi + 1) // 2 <= index:
        hi += 1
    lo = index - hi * (hi - 1) // 2
    return lo, hi


def combo_index(card1: int, card2: int) -> int:
    lo, hi = (card1, card2) if card1 < card2 else (card2, card1)
    return hi * (hi - 1) // 2 + lo


def combo_to_str(index: int) -> str:
    lo, hi = combo_from_index(index)
    return f"{card_to_str(hi)}{card_to_str(lo)}"


def combo_class(index: int) -> str:
    """콤보를 AKs / AKo / AA 같은 핸드 클래스로."""
    lo, hi = combo_from_index(index)
    r1, r2 = hi >> 2, lo >> 2
    if r1 == r2:
        return f"{RANK_CHARS[r1]}{RANK_CHARS[r2]}"
    high, low = (r1, r2) if r1 > r2 else (r2, r1)
    suited = "s" if (hi & 3) == (lo & 3) else "o"
    return f"{RANK_CHARS[high]}{RANK_CHARS[low]}{suited}"


# ---------------------------------------------------------------- 레인지 파서


def _rank_index(ch: str) -> int:
    return RANK_CHARS.index(ch.upper())


def parse_range(text: str) -> dict[int, float]:
    """부록 A 레인지 표기를 콤보 인덱스 -> 가중치 사전으로.

    뒤 토큰이 앞 토큰을 덮어쓴다 (src/range.rs와 같은 규칙).
    지원 형태: AA, AKs, AKo, AK, AhKh, 22+, A2s+, T9s-65s, 그리고 뒤에 :0.5 가중치.
    """
    weights: dict[int, float] = {}
    for raw_line in text.splitlines():
        line = raw_line.split("#", 1)[0]
        for token in line.replace(",", " ").split():
            _apply_token(token, weights)
    return {k: v for k, v in weights.items() if v > 0.0}


def _apply_token(token: str, weights: dict[int, float]) -> None:
    weight = 1.0
    if ":" in token:
        token, _, tail = token.partition(":")
        weight = float(tail)
    token = token.strip()
    if not token:
        return
    if "-" in token:
        left, _, right = token.partition("-")
        for combo in _expand_dash(left, right):
            weights[combo] = weight
        return
    plus = token.endswith("+")
    if plus:
        token = token[:-1]
    for combo in _expand_plus(token) if plus else _expand_simple(token):
        weights[combo] = weight


def _combos_pair(rank: int) -> Iterable[int]:
    cards = [rank * 4 + s for s in range(4)]
    for i in range(4):
        for j in range(i + 1, 4):
            yield combo_index(cards[i], cards[j])


def _combos_suited(r1: int, r2: int) -> Iterable[int]:
    for suit in range(4):
        yield combo_index(r1 * 4 + suit, r2 * 4 + suit)


def _combos_offsuit(r1: int, r2: int) -> Iterable[int]:
    for s1 in range(4):
        for s2 in range(4):
            if s1 != s2:
                yield combo_index(r1 * 4 + s1, r2 * 4 + s2)


def _expand_simple(token: str) -> Iterable[int]:
    if len(token) == 4 and token[1].lower() in SUIT_CHARS and token[3].lower() in SUIT_CHARS:
        yield combo_index(card_from_str(token[:2]), card_from_str(token[2:]))
        return
    r1 = _rank_index(token[0])
    r2 = _rank_index(token[1])
    suffix = token[2:].lower()
    if r1 == r2:
        yield from _combos_pair(r1)
        return
    if suffix == "s":
        yield from _combos_suited(r1, r2)
    elif suffix == "o":
        yield from _combos_offsuit(r1, r2)
    else:
        yield from _combos_suited(r1, r2)
        yield from _combos_offsuit(r1, r2)


def _expand_plus(token: str) -> Iterable[int]:
    r1 = _rank_index(token[0])
    r2 = _rank_index(token[1])
    suffix = token[2:].lower()
    if r1 == r2:
        for rank in range(r1, 13):
            yield from _combos_pair(rank)
        return
    high, low = max(r1, r2), min(r1, r2)
    for kicker in range(low, high):
        yield from _expand_simple(f"{RANK_CHARS[high]}{RANK_CHARS[kicker]}{suffix}")


def _expand_dash(left: str, right: str) -> Iterable[int]:
    lr1, lr2 = _rank_index(left[0]), _rank_index(left[1])
    rr1, rr2 = _rank_index(right[0]), _rank_index(right[1])
    suffix = left[2:].lower()
    if lr1 == lr2:
        lo, hi = min(lr1, rr1), max(lr1, rr1)
        for rank in range(lo, hi + 1):
            yield from _combos_pair(rank)
        return
    high = max(lr1, lr2)
    gap = abs(lr1 - lr2)
    start = min(lr2, rr2)
    end = max(lr2, rr2)
    for low in range(start, end + 1):
        top = low + gap
        if top > 12:
            continue
        yield from _expand_simple(f"{RANK_CHARS[top]}{RANK_CHARS[low]}{suffix}")
    del high


def load_range_file(path: str) -> dict[int, float]:
    with open(path, "r", encoding="utf-8") as handle:
        return parse_range(handle.read())


# ---------------------------------------------------------------- 파일 읽기


def fetch_bytes(source: str, timeout: float = 60.0) -> bytes:
    """URL이나 로컬 경로에서 blob 원본 바이트를 가져온다 (brotli면 푼다)."""
    if source.startswith("http://") or source.startswith("https://"):
        raw = _http_get(source, timeout)
    else:
        with open(source, "rb") as handle:
            raw = handle.read()
    return maybe_decompress(raw)


def _http_get(url: str, timeout: float) -> bytes:
    import urllib.request

    request = urllib.request.Request(
        url,
        headers={
            # 저장된 바이트를 그대로 받는다. R2는 Content-Encoding: br로 서빙한다.
            "Accept-Encoding": "identity",
            "User-Agent": "pokergoat-blob-reader/1",
        },
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.read()


def maybe_decompress(raw: bytes) -> bytes:
    if raw[:4] == MAGIC:
        return raw
    try:
        import brotli  # type: ignore
    except ImportError as exc:  # pragma: no cover
        raise SystemExit(
            "brotli 모듈이 필요하다: pip install brotli"
        ) from exc
    return brotli.decompress(raw)


# ---------------------------------------------------------------- blob 구조


@dataclass
class Header:
    scenario_id: int
    template_id: int
    solver_version: int
    board: list[int]
    street: int
    starting_pot: float
    effective_stack: float
    rake_percent: float
    rake_cap: float
    exploitability_pct: float
    iterations: int


@dataclass
class Node:
    index: int
    id: int
    kind: int
    player: int
    flags: int
    actions: list[tuple[int, float]]
    children: list[int]
    invest: tuple[float, float]
    strategy_offset: int
    ev_offset: int
    ev_scale: float
    parent: int = -1
    parent_action: int = -1

    @property
    def n_actions(self) -> int:
        return len(self.actions)

    @property
    def pruned(self) -> bool:
        return bool(self.flags & FLAG_PRUNED)

    @property
    def has_ev(self) -> bool:
        return bool(self.flags & FLAG_HAS_EV)

    def action_label(self, index: int) -> str:
        kind, amount = self.actions[index]
        name = ACTION_NAMES.get(kind, f"?{kind}")
        if kind in (ACTION_BET, ACTION_RAISE, ACTION_ALLIN):
            return f"{name} {amount:.2f}"
        return name


class Blob:
    def __init__(self, data: bytes, source: str = "") -> None:
        self.data = data
        self.source = source
        self.hands: list[list[int]] = [[], []]
        self.nodes: list[Node] = []
        self._parse()

    # -- 파싱 ------------------------------------------------------------
    def _parse(self) -> None:
        data = self.data
        if data[:4] != MAGIC:
            raise ValueError("magic이 GTOB가 아니다")
        version = data[4]
        if version != FORMAT_VERSION:
            raise ValueError(f"지원하지 않는 blob 버전: {version}")
        pos = 5
        scenario_id, template_id, solver_version = struct.unpack_from("<IHH", data, pos)
        pos += 8
        board_len = data[pos]
        pos += 1
        if not 3 <= board_len <= 5:
            raise ValueError(f"보드 길이가 이상하다: {board_len}")
        board = list(data[pos : pos + board_len])
        pos += board_len
        street = data[pos]
        pos += 1
        pot, stack, rake_pct, rake_cap, expl = struct.unpack_from("<5f", data, pos)
        pos += 20
        (iterations,) = struct.unpack_from("<I", data, pos)
        pos += 4
        self.header = Header(
            scenario_id=scenario_id,
            template_id=template_id,
            solver_version=solver_version,
            board=board,
            street=street,
            starting_pot=pot,
            effective_stack=stack,
            rake_percent=rake_pct,
            rake_cap=rake_cap,
            exploitability_pct=expl,
            iterations=iterations,
        )

        for player in range(2):
            (count,) = struct.unpack_from("<H", data, pos)
            pos += 2
            self.hands[player] = list(struct.unpack_from(f"<{count}H", data, pos))
            pos += count * 2

        (node_count,) = struct.unpack_from("<I", data, pos)
        pos += 4
        for index in range(node_count):
            node_id, kind, player, n_actions, flags = struct.unpack_from("<IBBBB", data, pos)
            pos += 8
            actions = []
            for _ in range(n_actions):
                a_kind = data[pos]
                (amount,) = struct.unpack_from("<f", data, pos + 1)
                actions.append((a_kind, amount))
                pos += 5
            n_children = expected_children(kind, n_actions)
            children = list(struct.unpack_from(f"<{n_children}I", data, pos)) if n_children else []
            pos += 4 * n_children
            invest = struct.unpack_from("<2f", data, pos)
            pos += 8
            strategy_offset, ev_offset = struct.unpack_from("<2Q", data, pos)
            pos += 16
            (ev_scale,) = struct.unpack_from("<f", data, pos)
            pos += 4
            self.nodes.append(
                Node(
                    index=index,
                    id=node_id,
                    kind=kind,
                    player=player,
                    flags=flags,
                    actions=actions,
                    children=children,
                    invest=(invest[0], invest[1]),
                    strategy_offset=strategy_offset,
                    ev_offset=ev_offset,
                    ev_scale=ev_scale,
                )
            )
        self._nodes_end = pos
        self._link_parents()

    def _link_parents(self) -> None:
        for node in self.nodes:
            if node.kind != KIND_PLAYER:
                continue
            for action_index, child in enumerate(node.children):
                if child >= len(self.nodes):
                    continue
                self.nodes[child].parent = node.index
                self.nodes[child].parent_action = action_index

    # -- 접근자 ----------------------------------------------------------
    def n_hands(self, player: int) -> int:
        return len(self.hands[player]) if player < 2 else 0

    def board_str(self) -> str:
        return board_to_str(self.header.board)

    def strategy(self, node: Node) -> list[list[float]]:
        """[action][hand] 확률. pruned면 균등 분포."""
        hands = self.n_hands(node.player)
        n_actions = node.n_actions
        if hands == 0 or n_actions == 0:
            return []
        if n_actions == 1:
            return [[1.0] * hands]
        if node.strategy_offset == OFFSET_NONE:
            return [[1.0 / n_actions] * hands for _ in range(n_actions)]
        base = node.strategy_offset
        out: list[list[float]] = []
        rest = [255] * hands
        for action in range(n_actions - 1):
            start = base + action * hands
            row_bytes = self.data[start : start + hands]
            row = []
            for hand, value in enumerate(row_bytes):
                rest[hand] -= value
                row.append(value / 255.0)
            out.append(row)
        out.append([max(v, 0) / 255.0 for v in rest])
        return out

    def raw_strategy_sums(self, node: Node) -> list[int]:
        """저장된 액션 바이트의 핸드별 합 (255를 넘으면 마지막 액션이 음수라는 뜻)."""
        hands = self.n_hands(node.player)
        n_actions = node.n_actions
        if hands == 0 or n_actions <= 1 or node.strategy_offset == OFFSET_NONE:
            return []
        base = node.strategy_offset
        sums = [0] * hands
        for action in range(n_actions - 1):
            start = base + action * hands
            for hand, value in enumerate(self.data[start : start + hands]):
                sums[hand] += value
        return sums

    def ev(self, node: Node) -> list[list[float]] | None:
        """[action][hand] 절대 EV (bb). 없으면 None."""
        if not node.has_ev or node.ev_offset == OFFSET_NONE:
            return None
        hands = self.n_hands(node.player)
        n_actions = node.n_actions
        if hands == 0 or n_actions == 0:
            return None
        out = []
        for action in range(n_actions):
            start = node.ev_offset + (action * hands) * 2
            values = struct.unpack_from(f"<{hands}h", self.data, start)
            out.append([v * node.ev_scale for v in values])
        return out

    def local_root(self, index: int) -> int:
        """파일 안에서 부모가 없는 조상. 턴/리버 파일은 서브트리가 여러 개다."""
        cursor = index
        while self.nodes[cursor].parent >= 0:
            cursor = self.nodes[cursor].parent
        return cursor

    def path_to(self, index: int) -> list[tuple[int, int]]:
        """서브트리 루트에서 노드까지의 (부모 인덱스, 액션 인덱스) 목록."""
        steps: list[tuple[int, int]] = []
        cursor = index
        while self.nodes[cursor].parent >= 0:
            node = self.nodes[cursor]
            steps.append((node.parent, node.parent_action))
            cursor = node.parent
        steps.reverse()
        return steps

    def node_by_line(self, line: str) -> Node:
        node = self.nodes[0]
        if not line:
            return node
        for part in line.split("."):
            index = int(part)
            if node.kind == KIND_CHANCE:
                raise ValueError("찬스 노드다. 다음 스트리트는 다른 파일에 있다")
            if index >= len(node.children):
                raise ValueError(f"액션 인덱스 {index}가 범위를 넘는다")
            child = node.children[index]
            if child in (CHILD_IMPOSSIBLE, CHILD_NOT_STORED):
                raise ValueError(f"자식이 저장돼 있지 않다 (0x{child:X})")
            node = self.nodes[child]
        return node

    # -- 가중치 ----------------------------------------------------------
    def base_weights(self, player: int, range_weights: dict[int, float] | None) -> list[float]:
        """루트 도달 가중치. 보드와 겹치는 콤보는 0."""
        board = set(self.header.board)
        out = []
        for combo in self.hands[player]:
            lo, hi = combo_from_index(combo)
            if lo in board or hi in board:
                out.append(0.0)
            elif range_weights is None:
                out.append(1.0)
            else:
                out.append(float(range_weights.get(combo, 0.0)))
        return out

    def reach_weights(
        self,
        index: int,
        ranges: tuple[dict[int, float] | None, dict[int, float] | None] = (None, None),
        base: list[list[float]] | None = None,
    ) -> list[list[float]]:
        """노드에서의 [OOP, IP] 핸드별 도달 가중치.

        `base`를 주면 서브트리 루트의 가중치로 그것을 쓴다 (앞 스트리트에서 이어받은 값).
        """
        if base is not None:
            weights = [list(base[0]), list(base[1])]
        else:
            weights = [self.base_weights(0, ranges[0]), self.base_weights(1, ranges[1])]
        for parent_index, action_index in self.path_to(index):
            parent = self.nodes[parent_index]
            if parent.kind != KIND_PLAYER:
                continue
            strategy = self.strategy(parent)
            row = strategy[action_index]
            player = parent.player
            weights[player] = [w * p for w, p in zip(weights[player], row)]
        return weights

    def normalized_weights(self, weights: list[list[float]], player: int) -> list[float]:
        """상대 레인지의 카드 제거 효과를 반영한 가중치 (엔진의 normalized weights)."""
        opp = 1 - player
        opp_weights = weights[opp]
        total = sum(opp_weights)
        per_card = [0.0] * 52
        pair_weight: dict[int, float] = {}
        for combo, weight in zip(self.hands[opp], opp_weights):
            if weight <= 0.0:
                continue
            lo, hi = combo_from_index(combo)
            per_card[lo] += weight
            per_card[hi] += weight
            pair_weight[combo] = weight
        out = []
        for combo, weight in zip(self.hands[player], weights[player]):
            if weight <= 0.0:
                out.append(0.0)
                continue
            lo, hi = combo_from_index(combo)
            overlap = per_card[lo] + per_card[hi] - pair_weight.get(combo, 0.0)
            out.append(weight * max(total - overlap, 0.0))
        return out


def parent_source(source: str) -> tuple[str, int] | None:
    """턴/리버 파일이면 (앞 스트리트 파일, 이 파일을 여는 카드 id)."""
    sep = "/" if "/" in source else os.sep
    parts = source.split(sep)
    name = parts[-1]
    if not name.endswith(".bin") and not name.endswith(".bin.br"):
        return None
    stem = name.split(".")[0]
    if len(parts) >= 3 and parts[-2] == "turn":
        head = sep.join(parts[:-2])
        return f"{head}{sep}flop.bin.br", card_from_str(stem)
    if len(parts) >= 4 and parts[-3] == "river":
        head = sep.join(parts[:-3])
        return f"{head}{sep}turn{sep}{parts[-2]}.bin.br", card_from_str(stem)
    return None


def entry_weights(
    blob: "Blob",
    subtree_root: int,
    ranges: tuple[dict[int, float] | None, dict[int, float] | None],
    depth: int = 0,
) -> list[list[float]] | None:
    """턴/리버 서브트리 루트의 도달 가중치를 앞 스트리트 파일에서 가져온다.

    턴 파일은 플랍의 베팅 라인마다 서브트리를 하나씩 담고 있어서, 파일만 보면
    그 라인이 이미 걸러 놓은 레인지를 알 수 없다. 앞 스트리트 파일의 찬스 노드까지
    거슬러 올라가 실제 도달 가중치를 계산한다.
    """
    if depth > 4:
        return None
    found = parent_source(blob.source)
    if found is None:
        return None
    parent_url, card = found
    try:
        parent = Blob(fetch_bytes(parent_url), parent_url)
    except Exception:
        return None
    if parent.hands != blob.hands:
        return None
    target = None
    for node in parent.nodes:
        if node.kind == KIND_CHANCE and node.children[card] == subtree_root:
            target = node
            break
    if target is None:
        return None
    base = entry_weights(parent, parent.local_root(target.index), ranges, depth + 1)
    weights = parent.reach_weights(target.index, ranges, base=base)
    for player in range(2):
        for hand, combo in enumerate(blob.hands[player]):
            lo, hi = combo_from_index(combo)
            if lo == card or hi == card:
                weights[player][hand] = 0.0
    return weights


def expected_children(kind: int, n_actions: int) -> int:
    if kind == KIND_CHANCE:
        return CHANCE_CHILDREN
    if kind == KIND_TERMINAL:
        return 0
    return n_actions


# ---------------------------------------------------------------- 집계


def action_frequencies(
    blob: Blob, node: Node, weights: list[float]
) -> tuple[list[float], float]:
    """가중치 기준 액션별 빈도와 가중치 합."""
    strategy = blob.strategy(node)
    total = sum(weights)
    if total <= 0.0 or not strategy:
        return [0.0] * node.n_actions, 0.0
    freqs = []
    for row in strategy:
        freqs.append(sum(w * p for w, p in zip(weights, row)) / total)
    return freqs, total


def range_ev(blob: Blob, node: Node, weights: list[float]) -> float | None:
    ev = blob.ev(node)
    if ev is None:
        return None
    strategy = blob.strategy(node)
    total = sum(weights)
    if total <= 0.0:
        return None
    acc = 0.0
    for hand, weight in enumerate(weights):
        if weight <= 0.0:
            continue
        value = 0.0
        for action in range(node.n_actions):
            value += strategy[action][hand] * ev[action][hand]
        acc += weight * value
    return acc / total


# ---------------------------------------------------------------- 출력


def print_header(blob: Blob) -> None:
    h = blob.header
    print(f"소스        {blob.source}")
    print(f"보드        {blob.board_str()}  ({STREET_NAMES.get(h.street, h.street)})")
    print(f"시나리오 id {h.scenario_id}  템플릿 {h.template_id}  솔버 {h.solver_version}")
    print(f"팟 / 스택   {h.starting_pot:.2f}bb / {h.effective_stack:.2f}bb")
    print(f"레이크      {h.rake_percent:.2f}%  캡 {h.rake_cap:.2f}bb")
    print(f"익스플로잇  {h.exploitability_pct:.4f}% pot   반복 {h.iterations}")
    print(f"핸드        OOP {len(blob.hands[0])}  IP {len(blob.hands[1])}")
    pruned = sum(1 for n in blob.nodes if n.pruned)
    kinds = {0: 0, 1: 0, 2: 0}
    for node in blob.nodes:
        kinds[node.kind] = kinds.get(node.kind, 0) + 1
    print(
        f"노드        {len(blob.nodes)}개 "
        f"(플레이어 {kinds.get(0, 0)} / 찬스 {kinds.get(1, 0)} / 터미널 {kinds.get(2, 0)}, "
        f"pruned {pruned})"
    )


def describe_node(
    blob: Blob,
    node: Node,
    ranges: tuple[dict[int, float] | None, dict[int, float] | None],
    top: int,
    normalized: bool,
    base: list[list[float]] | None = None,
) -> None:
    print()
    print(f"=== 노드 {node.index} ===")
    steps = blob.path_to(node.index)
    subtree_root = blob.local_root(node.index)
    prefix = "" if subtree_root == 0 else f"[서브트리 루트 {subtree_root}] "
    if steps:
        labels = [blob.nodes[p].action_label(a) for p, a in steps]
        print(f"라인        {prefix}{' -> '.join(labels)}")
    else:
        print(f"라인        {prefix}(서브트리 루트)")
    kind_name = {KIND_PLAYER: "player", KIND_CHANCE: "chance", KIND_TERMINAL: "terminal"}
    print(f"종류        {kind_name.get(node.kind, node.kind)}", end="")
    if node.kind == KIND_PLAYER:
        print(f"  액션 플레이어 {'OOP' if node.player == 0 else 'IP'}", end="")
    print(f"  flags 0x{node.flags:02X}{' (pruned)' if node.pruned else ''}")
    pot = blob.header.starting_pot + node.invest[0] + node.invest[1]
    print(f"투자        OOP {node.invest[0]:.2f}  IP {node.invest[1]:.2f}   현재 팟 {pot:.2f}bb")

    if node.kind == KIND_CHANCE:
        stored = sum(1 for c in node.children if c < CHILD_NOT_STORED)
        impossible = sum(1 for c in node.children if c == CHILD_IMPOSSIBLE)
        not_stored = sum(1 for c in node.children if c == CHILD_NOT_STORED)
        print(f"찬스 자식   저장 {stored} / 불가 {impossible} / 미저장 {not_stored} (총 52)")
        return
    if node.kind == KIND_TERMINAL:
        return

    weights_pair = blob.reach_weights(node.index, ranges, base=base)
    plain = weights_pair[node.player]
    weights = plain
    if normalized:
        raw = blob.normalized_weights(weights_pair, node.player)
        scale = sum(plain) / sum(raw) if sum(raw) > 0 else 0.0
        weights = [w * scale for w in raw]
    freqs, total = action_frequencies(blob, node, weights)
    root_total = sum(
        (base[node.player] if base else blob.base_weights(node.player, ranges[node.player]))
    )
    reach = sum(plain) / root_total if root_total > 0 else 0.0
    preflop_total = sum(blob.base_weights(node.player, ranges[node.player]))
    extra = ""
    if base is not None and preflop_total > 0:
        extra = f", 프리플랍 {preflop_total:.2f} 대비 {sum(plain) / preflop_total * 100:.2f}%"
    print(
        f"도달        가중치 합 {sum(plain):.2f} / 서브트리 루트 {root_total:.2f} "
        f"= {reach * 100:.2f}%{extra}"
        + ("   (집계는 상대 카드 제거 반영)" if normalized else "")
    )
    ev_value = range_ev(blob, node, weights)
    if ev_value is None:
        print("레인지 EV   없음 (EV 미저장 노드)")
    else:
        print(f"레인지 EV   {ev_value:.4f}bb")

    print()
    print("  #  액션            빈도      자식     액션 EV")
    ev_table = blob.ev(node)
    strategy = blob.strategy(node)
    for i in range(node.n_actions):
        child = node.children[i]
        if child == CHILD_IMPOSSIBLE:
            child_text = "impossible"
        elif child == CHILD_NOT_STORED:
            child_text = "not-stored"
        else:
            child_text = str(child)
        if ev_table is None:
            ev_text = "-"
        else:
            picked = sum(
                w * strategy[i][h] for h, w in enumerate(weights) if w > 0.0
            )
            if picked > 0:
                value = sum(
                    w * strategy[i][h] * ev_table[i][h]
                    for h, w in enumerate(weights)
                    if w > 0.0
                )
                ev_text = f"{value / picked:8.3f}"
            else:
                ev_text = "       -"
        print(
            f"  {i}  {node.action_label(i):<14}  {freqs[i] * 100:6.2f}%  "
            f"{child_text:>10}  {ev_text}"
        )

    if top <= 0:
        return
    print()
    print(f"  상위 {top}핸드 (가중치 순)")
    order = sorted(
        range(len(weights)), key=lambda h: weights[h], reverse=True
    )[:top]
    header_actions = "  ".join(f"{node.action_label(i):>12}" for i in range(node.n_actions))
    print(f"  {'핸드':<8} {'가중치':>7}   {header_actions}")
    for hand in order:
        if weights[hand] <= 0.0:
            continue
        combo = blob.hands[node.player][hand]
        splits = "  ".join(
            f"{strategy[i][hand] * 100:11.1f}%" for i in range(node.n_actions)
        )
        print(f"  {combo_to_str(combo):<8} {weights[hand]:7.3f}   {splits}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="솔루션 blob v0 리더")
    parser.add_argument("source", help="URL 또는 파일 경로 (.bin / .bin.br)")
    parser.add_argument("--node", type=int, default=None, help="노드 인덱스")
    parser.add_argument("--line", default=None, help="루트에서의 액션 경로 (예: 0.1.2)")
    parser.add_argument("--top", type=int, default=15, help="상위 핸드 출력 개수")
    parser.add_argument("--range-oop", default=None, help="OOP 레인지 파일 (가중치 반영)")
    parser.add_argument("--range-ip", default=None, help="IP 레인지 파일")
    parser.add_argument(
        "--normalized",
        action="store_true",
        help="상대 카드 제거를 반영한 가중치로 집계",
    )
    parser.add_argument("--tree", type=int, default=0, help="지정 깊이까지 트리 요약 출력")
    parser.add_argument(
        "--no-chain",
        action="store_true",
        help="턴/리버 파일에서 앞 스트리트 파일을 따라가 도달 가중치를 잇지 않는다",
    )
    args = parser.parse_args(argv)

    data = fetch_bytes(args.source)
    blob = Blob(data, source=args.source)
    print_header(blob)

    ranges = (
        load_range_file(args.range_oop) if args.range_oop else None,
        load_range_file(args.range_ip) if args.range_ip else None,
    )

    if args.tree:
        print()
        print_tree(blob, ranges, args.tree)

    if args.line is not None:
        node = blob.node_by_line(args.line)
    elif args.node is not None:
        node = blob.nodes[args.node]
    else:
        node = blob.nodes[0]

    base = None
    subtree_root = blob.local_root(node.index)
    if not args.no_chain and blob.header.street > 0:
        base = entry_weights(blob, subtree_root, ranges)
        if base is None:
            print()
            print(
                "주의: 앞 스트리트 파일을 잇지 못했다. 빈도는 프리플랍 레인지 기준이라 "
                "이전 스트리트의 액션 필터가 빠져 있다."
            )
        else:
            print()
            print(
                f"앞 스트리트 연결   서브트리 루트 {subtree_root}까지의 도달 가중치를 "
                "앞 스트리트 파일에서 이어받았다"
            )
    describe_node(blob, node, ranges, args.top, args.normalized, base=base)
    return 0


def print_tree(
    blob: Blob,
    ranges: tuple[dict[int, float] | None, dict[int, float] | None],
    depth: int,
    root_index: int = 0,
    base: list[list[float]] | None = None,
) -> None:
    print("트리 요약 (액션 빈도)")

    def walk(index: int, level: int, prefix: str) -> None:
        node = blob.nodes[index]
        if node.kind != KIND_PLAYER or level > depth:
            return
        weights_pair = blob.reach_weights(index, ranges, base=base)
        freqs, _ = action_frequencies(blob, node, weights_pair[node.player])
        for i in range(node.n_actions):
            label = f"{prefix}{'OOP' if node.player == 0 else 'IP'} {node.action_label(i)}"
            print(f"  {'  ' * level}{label}: {freqs[i] * 100:.1f}%")
            child = node.children[i]
            if child < CHILD_NOT_STORED and level < depth:
                walk(child, level + 1, prefix)

    walk(root_index, 0, "")


if __name__ == "__main__":
    sys.exit(main())
