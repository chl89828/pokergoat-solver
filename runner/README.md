# 솔버 로컬 러너

사장님 맥에서 상시 도는 배치 워커다. Django API의 잡 큐를 폴링해서 하나씩
가져오고, `pokergoat-solver` CLI로 풀고, 결과 blob을 Cloudflare R2에 올린 뒤
완료를 보고한다. 설계는 메타 레포 `docs/solver/gto-solver-plan.md` 6.2, 6.3, 7,
9.1절이다.

러너는 상태를 들고 있지 않다. 서버를 늘리고 싶으면 바이너리를 깔고 폴링을
시작하면 그만이다. Railway CLI는 쓰지 않는다. API 토큰과 R2 키만 있으면 돌기
때문에 마케팅 러너에서 며칠씩 멈추게 만든 CLI 로그인 만료 문제가 없다.

## 한 잡의 흐름

```
claim ──204──> 폴링 간격만큼 대기
  │
  └─200─> work/{job_id}/job.json 쓰기
          → estimate (메모리 상한 검사, 넘으면 여기서 fail)
          → solve (60초마다 heartbeat)
          → out/ 전부 R2 업로드 → 키마다 HEAD로 검증
          → complete (manifest, 크기, 노드 수, 익스플로이터빌리티)
          → work 디렉토리 삭제
```

중간에 무엇이 깨지든 `fail`을 보내고 다음 잡으로 넘어간다. 재시도 횟수는 API가
센다. 3회까지는 큐로 되돌아가고 그 다음에 failed로 굳는다.

## 설치

```bash
cd pokergoat-solver/runner

./install-bin.sh                       # 맥 arm64 바이너리 + validate 자가 점검
cp .env.example .env && chmod 600 .env # 값 입력 (아래 표)
../../.venv/bin/python runner.py --check

./install-launchd.sh                   # 상시 실행 등록
```

파이썬은 메타 레포의 공용 venv(`pokergoat/.venv`)를 쓴다. boto3와 requests가
이미 들어 있고, `install-launchd.sh`가 알아서 그 경로를 찾는다. 다른 파이썬을
쓰려면 경로를 인자로 넘기면 된다.

- 제거: `./uninstall-launchd.sh`
- 상태: `launchctl list | grep pokergoat`
- 수동 실행: `launchctl start com.pokergoat.solver-runner`
- 로그: `logs/solver-runner.out.log`, `logs/solver-runner.err.log`

특정 태그의 바이너리를 받으려면 `./install-bin.sh v0.1.0`처럼 태그를 준다. `gh`가
있으면 `gh release download`를 쓰고, 없으면 공개 릴리스 URL로 curl한다. 레포가
비공개면 `gh` 로그인이 필요하다.

## 환경 변수 (runner/.env)

| 키 | 기본값 | 설명 |
|---|---|---|
| `SOLVER_API_BASE` | `https://api.pokergoat.xyz` | 잡 큐를 가진 API 루트 |
| `SOLVER_RUNNER_TOKEN` | (필수) | Django의 `SOLVER_RUNNER_TOKEN`과 같은 값 |
| `SOLVER_RUNNER_ID` | 호스트명 | 누가 잡았는지 추적용. `[A-Za-z0-9._-]` 64자 이내 |
| `SOLVER_BIN` | `runner/bin/pokergoat-solver` | CLI 경로 |
| `SOLVER_THREADS` | 코어 수 | rayon 스레드. 애플 실리콘은 물리 코어와 논리 코어가 같다 |
| `SOLVER_MAX_MEMORY_GB` | 24 | `estimate`가 이 값을 넘으면 솔브하지 않고 실패로 보고 |
| `SOLVER_JOB_TIME_LIMIT_SEC` | 3600 | CLI의 `--time-limit`으로 그대로 전달 |
| `SOLVER_WORK_DIR` | `runner/work` | 잡별 작업 디렉토리. 완료하면 지운다 |
| `SOLVER_POLL_INTERVAL_SEC` | 30 | 큐가 비었을 때 쉬는 시간 |
| `SOLVER_ONCE` | 없음 | 1이면 잡 하나만 처리하고 종료 |
| `R2_ACCOUNT_ID` | (필수) | 엔드포인트 `https://{id}.r2.cloudflarestorage.com` |
| `R2_ACCESS_KEY_ID` | (필수) | R2 API 토큰 |
| `R2_SECRET_ACCESS_KEY` | (필수) | R2 API 토큰 |
| `R2_BUCKET` | `pokergoat-solver` | Django의 `SOLVER_R2_BUCKET`과 같아야 한다 |

`.env`는 셸을 태우지 않고 `KEY=VALUE`로만 읽는다. 이미 프로세스 환경에 있는
키는 덮어쓰지 않으므로 `SOLVER_ONCE=1 ./runner.py` 같은 일회성 지정이 그대로
먹는다. 토큰과 R2 키는 로그에 절대 찍히지 않는다.

메모리 상한은 CLI의 `--memory auto`와 같은 규칙으로 본다. 비압축 크기가 8GiB를
넘으면 엔진이 압축 저장으로 내려가므로 러너도 그때는 압축 크기와 비교한다.

## Django 쪽 설정

러너만 갖춰서는 돌지 않는다. API에 아래 세 값이 있어야 한다
(`config/settings/base.py`, Railway의 pokergoat-api 서비스 Variables).

| env | 용도 |
|---|---|
| `SOLVER_RUNNER_TOKEN` | 러너 엔드포인트 인증. 비어 있으면 `/runner/*`가 503을 준다 |
| `SOLVER_CDN_BASE_URL` | 라이브러리 blob의 공개 CDN 루트. 기본 `https://cdn.pokergoat.xyz` |
| `SOLVER_R2_BUCKET` | 커스텀 솔브 blob의 presigned URL 대상 버킷 |

토큰은 긴 랜덤 문자열로 만들어 Railway와 `runner/.env` 양쪽에 같은 값을 넣는다.

```bash
python -c "import secrets; print(secrets.token_urlsafe(48))"
```

버킷 이름은 러너의 `R2_BUCKET`과 Django의 `SOLVER_R2_BUCKET`이 같아야 한다.
다르면 러너는 잘 올리는데 커스텀 솔브 다운로드 링크가 빈 곳을 가리킨다.

## R2 버킷과 커스텀 도메인

라이브러리 blob은 공개다. 버킷에 `cdn.pokergoat.xyz`를 커스텀 도메인으로 붙이고
Django의 `SOLVER_CDN_BASE_URL`을 같은 주소로 맞춘다. 그러면 프론트가 API를 거치지
않고 `https://cdn.pokergoat.xyz/solutions/{scenario}/{flop}/flop.bin.br`을 바로
읽는다.

업로드 메타데이터는 러너가 붙인다.

- `.bin.br` 파일: `Content-Type: application/octet-stream`, `Content-Encoding: br`
- `manifest.json`: `Content-Type: application/json`
- 전부: `Cache-Control: public, max-age=31536000, immutable`

blob은 brotli로 미리 압축해서 저장한다. `Content-Encoding: br`이 붙어 있으므로
브라우저가 받는 즉시 알아서 푼다. 파서에 압축 해제 코드가 필요 없는 이유다.
경로는 immutable이라 같은 키를 덮어쓰지 않는다. 포맷이나 결과가 바뀌면
`solver_version`을 올려 새 경로에 쓴다.

커스텀 솔브 blob(`solutions/custom/{spot_id}/`)은 같은 버킷에 들어가지만 공개
CDN으로 노출하지 않는다. Django가 10분짜리 presigned URL을 내준다.

## 잡 하나만 돌려보기

```bash
cd pokergoat-solver/runner
SOLVER_ONCE=1 ../../.venv/bin/python runner.py
```

큐가 비어 있으면 아무것도 하지 않고 바로 끝난다. 잡이 있으면 하나만 처리하고
종료한다. launchd 잡이 떠 있는 상태로 같이 돌리면 둘이 서로 다른 잡을 가져간다.
API의 claim이 `select_for_update(skip_locked=True)`라서 같은 행을 두 러너가 잡는
일은 없다.

로컬 API로 시험하려면 `SOLVER_API_BASE=http://localhost:8010`을 주고, Django dev
서버의 `SOLVER_RUNNER_TOKEN`을 `.env`와 맞춘다.

## 문제 해결

**503이 계속 온다.** Django의 `SOLVER_RUNNER_TOKEN`이 비어 있다. 러너 토큰이
설정되지 않으면 API가 러너 엔드포인트 자체를 열지 않는다.

**401이 온다.** 토큰 값이 서로 다르다. `.env`의 값에 따옴표나 공백이 섞여
들어갔는지 본다.

**409가 온다.** heartbeat나 complete를 보낼 때 그 잡이 이미 다른 러너에게
넘어갔거나 끝난 상태다. 하트비트가 10분 넘게 끊기면 API가 잡을 큐로 되돌리므로,
맥이 자다 깬 뒤에 이 상황이 자주 생긴다. 그 잡은 다른 사이클에서 다시 풀린다.

**메모리 상한 초과로 실패한다.** `estimate` 결과가 `SOLVER_MAX_MEMORY_GB`를
넘었다. 램에 여유가 있으면 그 값을 올리고, 아니면 시나리오의 사이징 템플릿을
줄인다. 참고로 `t_simple_v1` 전체 템플릿은 플랍 한 장에 비압축 13.6GB다.

**아무 잡도 안 온다.** 큐가 비었다. API 쪽에서 `solver_seed_scenarios`로
시나리오를 적재하고 잡을 만들었는지 확인한다.

**밤사이 아무 일도 안 일어났다.** 맥이 잤다. launchd는 맥을 깨우지 않는다.
plist가 `caffeinate -i`로 감싸므로 유휴 슬립은 막지만, 뚜껑을 닫거나 직접
잠재우면 그동안은 멈춘다. 배치를 길게 돌릴 때는 전원을 꽂고 뚜껑을 열어 둔다.

**바이너리가 안 뜬다.** Gatekeeper가 격리 속성을 걸었을 수 있다.
`install-bin.sh`가 `xattr -d com.apple.quarantine`을 시도하지만 막히면 시스템
설정의 보안 항목에서 한 번 허용해 준다.

**업로드 검증에서 크기가 안 맞는다.** R2에 올라간 객체의 `ContentLength`가 로컬
파일과 다르다는 뜻이다. 같은 키를 다른 러너가 동시에 쓰고 있는지 본다. blob
경로는 잡마다 다르므로 정상 운영에서는 겹치지 않는다.

## 테스트

```bash
cd pokergoat-solver
/Users/dong/projects/pokergoat/.venv/bin/python -m pytest runner -q
```

네트워크와 R2는 전부 목이라 자격증명 없이 돈다. claim의 204 처리, manifest 매핑,
업로드 키 조립, 메모리 가드, 실패 경로의 `fail` 호출을 덮는다.
