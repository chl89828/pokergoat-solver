# 솔루션 blob 포맷 v0

설계서 `docs/solver/gto-solver-plan.md` §5.2를 실제 바이트 배치까지 확정한 문서다.
user-web의 파서(워커)는 이 문서만 보고 구현할 수 있어야 한다.

- 바이트 순서는 전부 little-endian.
- 카드 인코딩은 POKER GOAT 규칙이다. `card = rank * 4 + suit`, rank는 2가 0이고 A가 12,
  suit은 0=s, 1=h, 2=d, 3=c. postflop-solver 내부 규칙(suit 0=c 1=d 2=h 3=s)은 CLI가
  변환해서 넣으므로 파서는 신경 쓸 필요가 없다.
- 금액과 EV의 단위는 bb다. 엔진 내부의 정수 칩(1bb = 100칩)은 CLI가 나눠서 기록한다.
- 파일은 brotli 레벨 9로 미리 압축해 `*.bin.br`로 저장한다. R2에서 `Content-Encoding: br`로
  서빙하면 브라우저가 알아서 풀기 때문에 파서는 압축 해제 코드를 가질 필요가 없다.
  로컬 디버깅용으로 `--no-compress`를 주면 `*.bin`이 그대로 나온다.

## 1. 파일 배치

한 솔브의 출력 디렉토리는 이렇게 생겼다.

```
manifest.json
flop.bin.br                 시작 스트리트가 플랍일 때. 플랍 스트리트 노드 전부
turn/{card}.bin.br          플랍에서 시작할 때만. 대표 턴 카드마다 하나
river/{turn}/{river}.bin.br storeRiver가 true일 때만
turn.bin.br                 시작 스트리트가 턴일 때 (이 경우 turn/ 디렉토리는 없다)
river.bin.br                시작 스트리트가 리버일 때
```

`{card}`는 "Kh", "2c" 같은 두 글자 표기다. 스트리트 경계는 찬스 노드다. 찬스 노드 자체는
자기 스트리트 파일에 남고, 자식은 다음 스트리트 파일 안의 노드 번호를 가리킨다.

## 2. 헤더

| 오프셋 | 타입 | 이름 | 설명 |
|---|---|---|---|
| 0 | u8[4] | magic | `"GTOB"` (0x47 0x54 0x4F 0x42) |
| 4 | u8 | version | 현재 0 |
| 5 | u32 | scenarioId | 시나리오 id |
| 9 | u16 | templateId | 사이징 템플릿 id |
| 11 | u16 | solverVersion | 포맷과 엔진 버전 |
| 13 | u8 | boardLen | 3, 4, 5 중 하나 |
| 14 | u8[boardLen] | board | 보드 카드 |
| 14+boardLen | u8 | street | 0=플랍, 1=턴, 2=리버. 이 파일이 담은 스트리트 |
| 15+boardLen | f32 | startingPot | 시작 팟 (bb) |
| 19+boardLen | f32 | effectiveStack | 유효 스택 (bb) |
| 23+boardLen | f32 | rakePercent | 레이크 비율 (%) |
| 27+boardLen | f32 | rakeCap | 레이크 상한 (bb) |
| 31+boardLen | f32 | exploitabilityPct | 솔브 결과 익스플로이터빌리티 (% pot) |
| 35+boardLen | u32 | iterations | 실제 반복 횟수 |

헤더 길이는 `39 + boardLen` 바이트다 (플랍이면 42, 리버면 44). 턴 파일의 board는 플랍 3장에 그 턴 카드를 붙인 4장,
리버 파일은 5장이라서 파일 하나만 봐도 어느 보드인지 알 수 있다.

## 3. 핸드 목록

헤더 바로 뒤에 플레이어별 핸드 목록이 한 번 나온다. blob 안의 모든 전략과 EV 배열이 이
순서를 그대로 쓴다.

```
u16 n0, u16 combos0[n0]      OOP
u16 n1, u16 combos1[n1]      IP
```

콤보 인덱스는 카드 두 장을 0..1325로 접은 값이다. 두 카드를 `lo < hi`로 정렬하고
`index = hi * (hi - 1) / 2 + lo`. 되돌릴 때는 `hi * (hi - 1) / 2 <= index`를 만족하는 가장 큰
`hi`를 찾고 `lo = index - hi * (hi - 1) / 2`.

핸드 목록은 보드와 겹치는 콤보도 포함한다. 겹치는 핸드는 도달 가중치가 0이라 화면에서
빼야 하는데, 그 판단은 뷰어가 보드 카드와 콤보를 비교해서 한다.

## 4. 노드

```
u32 nodeCount
node[0], node[1], ... node[nodeCount - 1]
```

노드 하나의 배치는 다음과 같다.

| 타입 | 이름 | 설명 |
|---|---|---|
| u32 | id | 파일 안에서의 자기 인덱스. 검증용 중복 정보 |
| u8 | kind | 0=플레이어, 1=찬스, 2=터미널 |
| u8 | player | 0=OOP, 1=IP, 255=해당 없음 (찬스와 터미널) |
| u8 | nActions | 액션 수. 찬스와 터미널은 0 |
| u8 | flags | 비트 1=pruned, 2=hasEv, 4=locked |
| (u8, f32) × nActions | actions | 액션 종류와 금액 |
| u32 × nChildren | children | 자식 노드 인덱스 |
| f32 × 2 | invest | [OOP, IP] 누적 투자액 (bb) |
| u64 | strategyOffset | 전략 배열의 파일 절대 오프셋. 없으면 `u64::MAX` |
| u64 | evOffset | EV 배열의 파일 절대 오프셋. 없으면 `u64::MAX` |
| f32 | evScale | EV 복원 배율 |

`nChildren`은 저장하지 않고 `kind`로 정한다. 플레이어 노드는 `nActions`, 찬스 노드는 52,
터미널은 0이다. 노드 레코드 길이는 `36 + 5 * nActions + 4 * nChildren` 바이트라서
앞에서부터 순차로 읽으면 각 노드의 시작 위치를 그때그때 알 수 있다. 랜덤 접근이 필요하면
파서가 한 번 훑으면서 인덱스 배열을 만들어 두면 된다.

액션 종류는 0=폴드, 1=체크, 2=콜, 3=벳, 4=레이즈, 5=올인이다. 금액은 벳, 레이즈, 올인만
의미가 있고 그 스트리트에서 그 플레이어가 넣는 누적 금액이다. 콜 금액은 따로 적지 않는다.
`invest[상대] - invest[나]`로 계산해라.

### 자식 슬롯의 특수값

- `0xFFFFFFFF` (u32::MAX): 그 카드는 나올 수 없다. 보드에 이미 있거나 딜이 불가능하다.
- `0xFFFFFFFE`: 카드는 나올 수 있지만 이 배포에 서브트리를 담지 않았다. `storeRiver`가
  false인 리버 찬스 노드, 그리고 pruned 노드의 자식이 여기 해당한다.

찬스 노드의 children은 항상 52칸이고 인덱스가 곧 카드 id다. 값은 다음 스트리트 파일 안의
노드 인덱스이며, 어느 파일인지는 뷰어가 카드로 정한다. 플랍 파일의 찬스 노드라면
`turn/{card}.bin.br`, 턴 파일의 찬스 노드라면 `river/{turn}/{card}.bin.br`이다.

### 수트 동형과 턴 파일

엔진은 수트 동형인 턴 카드를 하나로 묶어 계산한다. CLI도 대표 카드에 대해서만 파일을 쓰고,
접힌 카드는 `manifest.json`의 `turnIsomorphism`에 `{"Kd": "Kc"}` 형태로 남긴다.
뷰어가 Kd 턴을 열려면 `turn/Kc.bin.br`을 읽고 두 수트를 맞바꿔서 해석한다. 바꿀 짝은
접힌 카드의 수트와 대표 카드의 수트다. 보드 표시와 콤보 인덱스 양쪽에 같은 치환을 적용해야
한다. 찬스 노드의 children 값은 접힌 카드 칸에도 대표 카드와 같은 노드 인덱스가 들어 있으니
파일만 바꿔 열면 된다.

`manifest.json`의 `turnCards`는 실제로 파일이 있는 대표 카드 목록이다. 동형이 하나도 없는
보드(무지개 플랍이 흔하다)에서는 49장이 전부 대표라 `turnIsomorphism`이 빈 객체다.

### pruned 노드

액션 플레이어의 레인지 도달 확률이 임계값 미만이면 본문 없이 헤더만 남긴다.
도달 확률은 그 노드에서의 해당 플레이어 가중치 합을 루트에서의 가중치 합으로 나눈 값이다.
pruned 노드는 `flags & 1`이 켜져 있고, 전략과 EV 오프셋이 `u64::MAX`이며, 자식이 전부
`0xFFFFFFFE`다. 액션 목록은 그대로 있으니 화면에는 균등 전략과 "저도달" 배지로 표시해라.

임계값 기본은 1e-5이고 `manifest.json`의 `pruneEpsilon`에 적힌다. 리버 노드만 따로
더 세게 자를 수 있는데, 그때 쓴 값은 `riverPruneReach`에 남는다. 잘린 노드의 서브트리는
아예 파일에 들어가지 않으므로 리버 임계값을 올리면 노드 수와 파일 크기가 같이 줄어든다.
`riverPruneReach`가 `pruneEpsilon`과 같으면 스트리트별 차이가 없다는 뜻이다.

## 5. 전략

노드 순서대로 이어 붙인 u8 배열이다. 노드 하나의 길이는
`(nActions - 1) * hands[player]` 바이트이고, 액션 우선 배치다. 즉 `i`번째 액션과 `j`번째
핸드의 값은 `strategy[i * hands + j]`에 있다.

마지막 액션은 저장하지 않는다. 앞의 값을 다 더해 255에서 빼면 나온다.

```js
const hands = handsOf[node.player];
const base = Number(node.strategyOffset);
function probability(action, hand) {
  if (node.strategyOffset === MAX_U64) return 1 / node.nActions;   // pruned
  if (action < node.nActions - 1) return bytes[base + action * hands + hand] / 255;
  let rest = 255;
  for (let a = 0; a < node.nActions - 1; a += 1) rest -= bytes[base + a * hands + hand];
  return Math.max(rest, 0) / 255;
}
```

액션이 하나뿐인 노드는 전략을 저장하지 않는다. 그 액션의 확률은 1이다.
양자화 오차는 액션당 1/255 이내이고, 핸드별 합은 정확히 1이 된다.

## 6. EV

`flags & 2`가 켜진 노드만 EV를 가진다. 배열은 i16이고 길이는 `nActions * hands[player]`,
배치는 전략과 같은 액션 우선이다.

플랍과 턴의 플레이어 노드는 항상 EV를 가진다. 리버 플레이어 노드는 EV가 없을 수도 있다.
리버 노드의 EV는 배포 용량의 대부분을 차지해서(노드마다 2바이트 × 액션 수 × 핸드 수)
빼고 내보내는 선택지가 있기 때문이다. 그렇게 나온 리버 노드는 `flags & 2`가 꺼져 있고
`evOffset`이 `u64::MAX`, `evScale`이 0이다. 전략은 그대로 있으므로 리버에서도 액션 빈도는
읽을 수 있고, EV 숫자만 화면에서 빼면 된다. 어느 쪽으로 나왔는지는 `manifest.json`의
`riverEv`(true/false)로 판단해라. 파서는 이 값을 보지 않고도 `flags & 2`만 확인하면 된다.

```js
const evBb = readInt16LE(base + (action * hands + hand) * 2) * node.evScale;
```

값은 그 액션을 골랐을 때 그 핸드의 절대 EV(bb)이며 팟 지분이 이미 반영돼 있다.
`evScale`은 노드마다 다르고 `max(|EV|) / 32767`이다. 복원 오차는 스케일 한 칸 이내다.
EV가 전부 0인 노드는 스케일이 1.0이다.

## 7. 파싱 순서 요약

1. 헤더를 읽어 보드, 스트리트, 팟, 스택을 잡는다.
2. 핸드 목록 두 개를 읽어 콤보 인덱스 배열을 만든다. 이후 모든 배열이 이 순서다.
3. `nodeCount`를 읽고 노드 레코드를 순차로 읽으면서 각 노드의 오프셋을 기억한다.
4. 화면에 필요한 노드만 `strategyOffset`, `evOffset`으로 바로 찾아간다. 전략과 EV 구간은
   파일 끝에 몰려 있으므로 노드 레코드 구간만 먼저 파싱해 두면 트리 탐색이 가볍다.
5. 다음 스트리트로 넘어갈 때는 찬스 노드의 children에서 노드 인덱스를 꺼내고, 카드로
   파일 경로를 만들어 새 blob을 연다.

## 8. 호환성

경로는 immutable이다. 포맷이나 엔진이 바뀌면 헤더의 `version`이나 `solverVersion`을 올리고
새 경로에 쓴다. 파서는 `magic`과 `version`을 먼저 확인하고 모르는 버전이면 거부해라.
