# pokergoat-solver

POKER GOAT의 GTO 솔버 래퍼 CLI다. 잡 설정 JSON을 받아 [postflop-solver] 엔진을 돌리고,
뷰어가 바로 읽는 solution blob과 manifest를 떨군다. 설계는 메타 레포의
`docs/solver/gto-solver-plan.md` §4, §5, §12를 따른다.

[postflop-solver]: https://github.com/b-inary/postflop-solver

## 라이선스와 격리

엔진인 postflop-solver가 AGPL-3.0이다. 이 엔진을 링크한 프로그램은 AGPL 파생물이 되고
소스 공개 의무가 생긴다. 그래서 엔진을 쓰는 코드는 전부 이 레포에 모아 두고 AGPL-3.0-or-later로
공개한다. 내용은 설정을 읽고 엔진을 돌리고 blob을 쓰는 코드라 사업적으로 잃을 것이 없다.

경계는 이렇게 잡았다.

- 이 레포만 AGPL이다. `pokergoat-api`와 `pokergoat-user-web`은 blob 파일과 HTTP API로만
  이 프로그램과 통신하고, 프로그램의 출력 데이터는 라이선스 대상이 아니라서 비공개를 유지한다.
- postflop-solver의 소스를 다른 레포로 복사하지 않는다. 의존성은 커밋 해시로 고정한 git
  dependency 하나뿐이다 (`Cargo.toml`의 `rev`).
- 나중에 브라우저 리버 재솔브를 wasm으로 만들면 그 wasm도 이 레포에서 빌드해 공개한다.

## 빌드

로컬 맥에는 Rust를 설치하지 않는다. 컴파일과 테스트는 Colima docker의 `rust` 이미지로 하고,
배포 바이너리는 GitHub Actions가 만든다.

```bash
colima start

docker run --rm -v "$PWD":/work \
  -v pokergoat-cargo-registry:/usr/local/cargo/registry \
  -v pokergoat-cargo-git:/usr/local/cargo/git \
  -v pokergoat-target:/work/target \
  -w /work rust:1-bookworm cargo build --release

docker run --rm -v "$PWD":/work \
  -v pokergoat-cargo-registry:/usr/local/cargo/registry \
  -v pokergoat-cargo-git:/usr/local/cargo/git \
  -v pokergoat-target:/work/target \
  -w /work rust:1-bookworm cargo test --release
```

볼륨 세 개는 레지스트리, git 체크아웃, 빌드 산출물 캐시다. 이걸 붙여야 두 번째 빌드부터
몇 초에 끝난다. 이렇게 만든 바이너리는 Linux용이라 docker 안에서만 돈다. 맥용 바이너리는
릴리스 워크플로가 만든다.

## 서브커맨드

```
pokergoat-solver estimate --config job.json
pokergoat-solver solve --config job.json --out DIR [--threads N] [--time-limit SEC] [--no-compress]
pokergoat-solver aggregate --scenario-dir DIR --out DIR
pokergoat-solver validate [--iterations N]
```

### estimate

트리 노드 수와 메모리 사용량을 JSON으로 출력한다. 러너가 잡을 claim하기 전에 상한을 검사하는
용도다. 메모리는 엔진이 계산한 값이라 실제 할당량과 같다.

```json
{
  "board": "As7d2c",
  "street": "flop",
  "nodeCount": 3970233,
  "nodeBreakdown": { "player": 1514659, "chance": 3984, "terminal": 2451590, "actionTree": 101193 },
  "memoryBytes": { "uncompressed": 13606573780, "compressed": 6937094466 },
  "handsPerPlayer": [431, 429],
  "turnRepresentatives": 49,
  "warnings": ["maxRaisesPerStreet=2는 ..."]
}
```

### solve

솔브하고 `--out` 아래에 `manifest.json`과 blob을 쓴다. 파일 배치는 `docs/blob-format.md` §1.
`--time-limit`을 주면 그 초를 넘길 때 그때까지의 평균 전략으로 마감하고 manifest에 경고를 남긴다.
`--threads`는 rayon 스레드 수이고 기본값은 코어 수다. `--no-compress`는 brotli를 건너뛰고
`.bin`을 그대로 쓴다. 디버깅과 픽스처 생성용이다.

manifest에는 익스플로이터빌리티, 반복 횟수, 경과 시간, 노드 수, 메모리, 파일별 크기,
솔버 버전, 템플릿 id, 턴 대표 카드 목록과 동형 매핑이 들어간다.

### validate

설계서 §12.2 토이 게임을 내장 실행한다. 보드 2c3d4h8s9c, OOP는 65(넛)와 JT(에어), IP는
QQ(블러프캐처)뿐이고 팟사이즈 벳 하나만 허용한다. 이론값은 IP 콜 빈도 50%, 벳 중 블러프 비율
1/3이다. 허용 오차 ±2%p, 익스플로이터빌리티 0.1% pot 이하를 모두 만족해야 종료 코드 0이다.
CI와 러너 기동 시 자가 점검으로 돌린다.

```json
{ "ipCallFrequency": 0.4999, "bluffShareOfBets": 0.3341, "exploitabilityPctPot": 0.0476, "pass": true }
```

### aggregate

§6.4 애그리게이트 리포트 자리다. 아직 "not implemented"만 출력한다.

## 잡 설정 JSON

`examples/srp_btn_bb_flop.json`이 100bb 6맥스 BTN 오픈 대 BB 콜 싱글레이즈드 팟이다.

```json
{
  "scenario": "c6m_100_srp_btn_bb",
  "scenarioId": 1,
  "template": "t_simple_v1",
  "templateId": 1,
  "board": "As7d2c",
  "ranges": ["JJ-22,AJs-A2s,K2s+,...", "22+,A2s+,K5s+,..."],
  "pot": 5.5,
  "effectiveStack": 97.5,
  "sizing": {
    "flop":  { "oop": { "bet": [33, 75],       "raise": [50] }, "ip": { "bet": [33, 75],       "raise": [50] } },
    "turn":  { "oop": { "bet": [50, 100],      "raise": [50] }, "ip": { "bet": [50, 100],      "raise": [50] } },
    "river": { "oop": { "bet": [50, 100, 150], "raise": [50] }, "ip": { "bet": [50, 100, 150], "raise": [50] } }
  },
  "donk": { "turn": [50], "river": [50] },
  "maxRaisesPerStreet": 2,
  "allInThreshold": 0.67,
  "addAllInThreshold": 1.5,
  "mergingThreshold": 0.1,
  "iterations": 1000,
  "targetExploitability": 0.3,
  "rake": { "percent": 5, "capBb": 3 },
  "storeRiver": false
}
```

필드 설명이다.

- `board`는 "As7d2c" 표기나 카드 id 배열 `[48, 22, 3]` 둘 다 받는다. 3장이면 플랍부터,
  4장이면 턴부터, 5장이면 리버만 푼다. 카드 id는 `rank * 4 + suit`, suit은 0=s 1=h 2=d 3=c다.
- `ranges`는 [OOP, IP] 순서다. 문법은 아래 레인지 절.
- `pot`과 `effectiveStack`은 bb다. 엔진은 정수 칩만 받으므로 CLI가 100배해서 넘긴다.
  0.01bb 미만은 반올림된다.
- 사이즈는 전부 % pot이다. `raise`가 비어 있으면 그 스트리트에 레이즈가 없다.
- `allInThreshold`는 엔진의 `force_allin_threshold`다. 콜 이후 SPR이 이 값 이하면 벳을
  올인으로 승격한다. `addAllInThreshold`는 `add_allin_threshold`이고 최대 벳이 팟의 이 배수
  이하일 때 올인 액션을 추가한다.
- `mergingThreshold`는 가까운 벳 사이즈를 합치는 PioSOLVER식 임계값이다.
- `rake`는 `{percent, capBb}`. 없으면 레이크 0이다.
- `donk`는 OOP 돈벳 사이즈다. 플랍 돈벳은 개념상 없다.
- `storeRiver`가 true면 리버 blob까지 쓴다. 파일 수와 용량이 수십 배로 뛴다.
- `targetExploitability`는 % pot이고, 이 값에 도달하면 반복을 멈춘다.

### maxRaisesPerStreet은 무시한다

postflop-solver에는 스트리트별 레이즈 횟수 상한 옵션이 없다. `Additive` 사이즈에만 raise cap이
있고 퍼센트 사이즈에는 걸리지 않는다. 그래서 이 필드는 읽되 적용하지 않고 경고를 남긴다.
경고는 stderr와 manifest의 `warnings`에 같이 들어간다. 실제 레이즈 깊이는 스택, 사이즈 목록,
`addAllInThreshold`가 결정한다. 트리를 얕게 하고 싶으면 레이즈 사이즈를 줄이거나 빼라.

## 레인지 문법

설계서 부록 A 그대로다. 쉼표나 공백으로 토큰을 나누고 `:w`로 가중치를 준다.

```
AA, KK+, AKs, AQo, A2s+, T9s-65s, AhKh, KQo:0.5
```

- `AK`처럼 수딧 표시가 없으면 수딧과 오프수트를 둘 다 넣는다.
- 대시 범위는 내림차순이어야 한다. `AJs-A2s`는 되고 `A2s-AJs`는 에러다.
- 같은 콤보가 여러 번 나오면 뒤 토큰이 앞을 덮어쓴다. 엔진 파서는 반대 방향이라 래퍼가
  토큰 순서를 뒤집어서 넘긴다.
- 대소문자는 가리지 않는다. `ahkh`도 `AhKh`로 읽는다.

## blob 포맷

`docs/blob-format.md`에 바이트 배치까지 적어 뒀다. user-web 파서는 그 문서만 보고 짤 수 있다.
요점은 이렇다.

- 파일은 스트리트 단위로 쪼갠다. `flop.bin.br`, `turn/{card}.bin.br`,
  `river/{turn}/{river}.bin.br`.
- 전략은 u8 양자화이고 마지막 액션은 저장하지 않는다 (255에서 나머지를 뺀다).
- EV는 노드별 스케일을 곱하는 i16이다.
- 수트 동형인 턴 카드는 대표 카드 파일 하나만 쓰고 매핑을 manifest에 남긴다.
- 도달 확률 1e-5 미만 노드는 헤더만 남기고 본문을 생략한다.

## 실측 (Colima docker, aarch64 8코어 16GB)

`examples/srp_btn_bb_flop_quick.json`은 사이즈를 스트리트당 하나로 줄인 스모크 테스트용
설정이다. 전체 파이프라인을 몇 초 만에 확인할 수 있다.

| 항목 | 값 |
|---|---|
| 노드 수 (estimate, 리버 포함) | 248,292 |
| 메모리 (비압축) | 0.84GB |
| 핸드 | OOP 431 / IP 429 |
| 반복 | 110 (목표 0.3% pot 도달) |
| 솔브 시간 | 12.7초 |
| 저장 노드 (플랍 + 턴) | 1,332 |
| 출력 | 50개 파일, 압축 전 1.44MB, 압축 후 0.78MB |

`examples/srp_btn_bb_flop.json`(t_simple_v1 전체 템플릿)은 같은 보드에서 노드 397만 개,
비압축 메모리 13.6GB다. 이 정도는 러너 맥에서 돌리는 크기이고 docker 16GB에서는 빠듯하다.

## 테스트

`cargo test --release`가 도는 것들이다.

- 카드 인코딩 왕복, 콤보 인덱스 1326개 전수, 보드 파싱
- 잡 JSON 파싱과 검증 (기본값, 미지 필드 거부, 범위 검사)
- 레인지 변환 (`22+`, `A2s+`, `T9s-65s`, `AK`, `AhKh`, `:0.5`, 덮어쓰기 방향)
- 손으로 센 소형 트리의 액션, 금액, 노드 수
- blob 쓰기와 읽기 왕복, 전략 오차 1/255 이내, EV 오차 스케일 이내
- 솔브부터 파일 쓰기까지 파이프라인 (플랍 시작, 턴 시작 + 리버 저장, 모노톤 플랍 동형)
- §12.2 토이 게임

## 릴리스

태그 `v*`를 푸시하면 `.github/workflows/release.yml`이 macos-14(arm64)와
ubuntu-22.04(x86_64)에서 테스트와 빌드를 하고, 토이 게임 자가 점검을 돌린 뒤
`pokergoat-solver-{target}.tar.gz`와 sha256을 GitHub 릴리스에 올린다.
러너는 그 릴리스에서 맥 바이너리를 내려받아 쓴다.
