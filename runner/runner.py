#!/usr/bin/env python3
"""POKER GOAT 솔버 로컬 러너 (설계서 6.3, 9.1).

Django API의 잡 큐를 폴링해서 하나씩 가져오고, pokergoat-solver CLI로 풀고,
결과 blob을 Cloudflare R2에 올린 뒤 완료를 보고한다. 상태를 들고 있지 않아서
러너를 늘리려면 바이너리를 깔고 폴링을 시작하면 된다.

흐름 하나는 이렇다.

    claim -> estimate(메모리 상한 검사) -> solve -> R2 업로드(병렬)
          -> complete -> work 디렉토리 삭제

의존성은 표준 라이브러리와 boto3, requests뿐이다. Railway CLI는 쓰지 않는다.
API 토큰과 R2 키만 있으면 돌기 때문에 마케팅 러너에서 겪은 CLI 로그인 만료
문제가 생기지 않는다.

사용:
    ./runner.py            상시 루프 (launchd가 이 형태로 띄운다)
    ./runner.py --once     잡 하나만 처리하고 종료
    ./runner.py --check    바이너리와 R2 접근만 점검 (claim 안 함)
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

RUNNER_DIR = Path(__file__).resolve().parent

# API 계약 (apps/solver/serializers.py). 러너가 보내는 값이 이 형태를 벗어나면
# 400이 떨어지므로 클라이언트에서 미리 맞춘다.
RUNNER_ID_PATTERN = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
MAX_MANIFEST_CHARS = 16384
MAX_ERROR_CHARS = 2000
# API가 재시도 없이 실패 처리하는 사유 (apps/solver/services.py NO_RETRY_REASONS와 맞춘다)
NO_RETRY_REASONS = ("memory",)
# exploitability는 DecimalField(max_digits=8, decimal_places=4, min_value=0).
EXPLOITABILITY_DECIMALS = 4
EXPLOITABILITY_MAX = 9999.9999

# CLI의 `--memory auto` 기준. 비압축 크기가 이보다 크면 엔진이 압축 저장으로
# 내려가므로 러너의 상한 검사도 같은 규칙을 따른다 (src/main.rs MemoryMode).
MEMORY_AUTO_THRESHOLD_BYTES = 8 * 1024**3

HEARTBEAT_INTERVAL_SEC = 60
# solve는 --time-limit에 걸려도 그 뒤에 blob을 쓰는 시간이 더 든다. 프로세스를
# 강제 종료하는 시각은 시간 상한보다 이만큼 뒤로 둔다.
SOLVE_EXPORT_GRACE_SEC = 900
ESTIMATE_TIMEOUT_SEC = 900
VALIDATE_TIMEOUT_SEC = 600
HTTP_TIMEOUT_SEC = 60

CACHE_CONTROL = "public, max-age=31536000, immutable"

# out/ 업로드 병렬도 기본값. SOLVER_UPLOAD_WORKERS로 덮어쓴다.
DEFAULT_UPLOAD_WORKERS = 16
# 파일 하나가 재시도 후에도 실패로 굳기까지 시도하는 횟수.
UPLOAD_RETRY_ATTEMPTS = 3
# 재시도 사이 대기(초). attempt 1 실패 후 0.5s, attempt 2 실패 후 1.0s.
UPLOAD_RETRY_BACKOFF_BASE_SEC = 0.5
# 이 개수마다 진행 로그를 한 줄 찍는다.
UPLOAD_LOG_EVERY = 500
MANIFEST_FILENAME = "manifest.json"

# manifest는 API에서 16KB 상한이 걸려 있는데 CLI manifest의 files와 턴 동형
# 매핑은 그 혼자로 수십 KB다. 잡 기록에 필요한 건 요약값이라 큰 배열을 뺀다.
MANIFEST_BULK_KEYS = ("files", "turnIsomorphism", "turnCards", "boardCards")
# 위를 빼고도 상한을 넘으면 이 키만 남긴다.
MANIFEST_CORE_KEYS = (
    "formatVersion",
    "solverVersion",
    "engine",
    "cliVersion",
    "scenario",
    "template",
    "board",
    "street",
    "iterations",
    "exploitability",
    "exploitabilityPctPot",
    "elapsedSec",
    "memoryBytes",
    "nodeCount",
    "prunedNodes",
    "bytesRaw",
    "bytesStored",
    "compression",
)

CONTENT_TYPES = {
    ".json": "application/json",
    ".bin": "application/octet-stream",
    ".br": "application/octet-stream",
}

STOP = threading.Event()


class RunnerStopped(Exception):
    """SIGTERM/SIGINT으로 중단됐다. 잡고 있던 잡은 fail로 돌려준다."""


class JobFailure(Exception):
    """이 잡은 실패했다. message가 API의 error 필드로 간다.

    reason이 "memory"면 API가 재시도 없이 바로 failed로 둔다(설계서 §6.2).
    """

    def __init__(self, message, reason=None):
        super().__init__(message)
        self.reason = reason


def log(message):
    """타임스탬프 로그. 토큰과 R2 키는 절대 여기로 흘리지 않는다."""
    stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{stamp}] {message}", flush=True)


# ── 설정 ────────────────────────────────────────────────────────────────


def load_env_file(path):
    """KEY=VALUE 형식의 .env를 읽는다. 셸을 태우지 않는다.

    주석과 빈 줄은 건너뛰고, 값 양끝의 따옴표 한 겹만 벗긴다. 이미 프로세스
    환경에 있는 키는 덮어쓰지 않는다. `SOLVER_ONCE=1 ./runner.py`처럼 그때만
    다르게 주고 싶은 경우가 있어서다.
    """
    path = Path(path)
    if not path.is_file():
        return {}

    loaded = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        if not key:
            continue
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
            value = value[1:-1]
        loaded[key] = value
    return loaded


def apply_env_file(path):
    """.env 값을 os.environ에 채운다 (기존 값 우선)."""
    for key, value in load_env_file(path).items():
        os.environ.setdefault(key, value)


def _int_env(name, default):
    raw = (os.environ.get(name) or "").strip()
    if not raw:
        return default
    try:
        return int(raw)
    except ValueError:
        log(f"경고: {name}={raw!r}를 정수로 읽지 못해 기본값 {default}를 쓴다")
        return default


def _float_env(name, default):
    raw = (os.environ.get(name) or "").strip()
    if not raw:
        return default
    try:
        return float(raw)
    except ValueError:
        log(f"경고: {name}={raw!r}를 숫자로 읽지 못해 기본값 {default}를 쓴다")
        return default


def _bool_env(name, default=False):
    raw = (os.environ.get(name) or "").strip().lower()
    if not raw:
        return default
    return raw in ("1", "true", "yes", "on")


def sanitize_runner_id(value):
    """API의 runner_id 패턴에 맞춘다. 호스트명에 흔한 공백과 한글을 -로 바꾼다."""
    cleaned = re.sub(r"[^A-Za-z0-9._-]", "-", (value or "").strip())[:64]
    return cleaned or "mac-runner"


def parse_solve_args(raw):
    """`SOLVER_SOLVE_ARGS`를 argv 조각으로 쪼갠다.

    셸을 태우지 않고 shlex로만 나누므로 따옴표는 먹지만 변수 전개나 글롭은
    일어나지 않는다. 따옴표가 안 맞으면 ValueError를 올려서 러너가 시작할 때
    바로 걸리게 한다 (조용히 무시하면 옵션이 빠진 줄 모르고 돈다).
    """
    text = (raw or "").strip()
    if not text:
        return []
    try:
        return shlex.split(text)
    except ValueError as exc:
        raise ValueError(f"SOLVER_SOLVE_ARGS를 읽지 못했다: {exc}") from exc


def default_threads():
    """물리 코어 수. 애플 실리콘은 하이퍼스레딩이 없어 논리 코어와 같다."""
    return os.cpu_count() or 4


class RunnerConfig:
    """러너 설정 한 덩어리. 값은 전부 .env나 프로세스 환경에서 온다."""

    def __init__(self, env=None):
        env = os.environ if env is None else env
        self.api_base = (env.get("SOLVER_API_BASE") or "https://api.pokergoat.xyz").rstrip("/")
        self.token = env.get("SOLVER_RUNNER_TOKEN") or ""
        self.runner_id = sanitize_runner_id(
            env.get("SOLVER_RUNNER_ID") or socket.gethostname()
        )
        self.solver_bin = Path(
            env.get("SOLVER_BIN") or (RUNNER_DIR / "bin" / "pokergoat-solver")
        ).expanduser()
        self.threads = _int_env("SOLVER_THREADS", default_threads())
        self.max_memory_gb = _float_env("SOLVER_MAX_MEMORY_GB", 24.0)
        self.job_time_limit_sec = _int_env("SOLVER_JOB_TIME_LIMIT_SEC", 3600)
        self.work_dir = Path(
            env.get("SOLVER_WORK_DIR") or (RUNNER_DIR / "work")
        ).expanduser()
        self.poll_interval_sec = _int_env("SOLVER_POLL_INTERVAL_SEC", 30)
        # solve 서브커맨드 뒤에 그대로 붙는 추가 인자 (예: "--river-ev off")
        self.solve_args = parse_solve_args(env.get("SOLVER_SOLVE_ARGS"))
        self.once = _bool_env("SOLVER_ONCE", False)
        # out/ 업로드에 쓸 스레드 수. R2 라운드트립이 병목이라 코어 수보다 크게 잡는다.
        self.upload_workers = _int_env("SOLVER_UPLOAD_WORKERS", DEFAULT_UPLOAD_WORKERS)
        self.r2_account_id = env.get("R2_ACCOUNT_ID") or ""
        self.r2_access_key_id = env.get("R2_ACCESS_KEY_ID") or ""
        self.r2_secret_access_key = env.get("R2_SECRET_ACCESS_KEY") or ""
        self.r2_bucket = env.get("R2_BUCKET") or ""

    @property
    def max_memory_bytes(self):
        return int(self.max_memory_gb * 1024**3)

    @property
    def r2_endpoint(self):
        return f"https://{self.r2_account_id}.r2.cloudflarestorage.com"

    def missing_keys(self):
        """비어 있으면 러너가 돌 수 없는 키 목록."""
        required = {
            "SOLVER_RUNNER_TOKEN": self.token,
            "R2_ACCOUNT_ID": self.r2_account_id,
            "R2_ACCESS_KEY_ID": self.r2_access_key_id,
            "R2_SECRET_ACCESS_KEY": self.r2_secret_access_key,
            "R2_BUCKET": self.r2_bucket,
        }
        return [key for key, value in required.items() if not value]

    def summary_lines(self):
        """사람이 읽는 설정 요약. 비밀값은 유무만 보여준다."""
        return [
            f"API          {self.api_base}",
            f"러너 id      {self.runner_id}",
            f"토큰         {'설정됨' if self.token else '없음'}",
            f"바이너리     {self.solver_bin}",
            f"스레드       {self.threads}",
            f"메모리 상한  {self.max_memory_gb:g}GB",
            f"시간 상한    {self.job_time_limit_sec}s",
            f"작업 디렉토리 {self.work_dir}",
            f"폴링 간격    {self.poll_interval_sec}s",
            f"추가 인자    {' '.join(self.solve_args) if self.solve_args else '없음'}",
            f"업로드 워커  {self.upload_workers}",
            f"R2 버킷      {self.r2_bucket or '없음'}",
            f"R2 계정      {'설정됨' if self.r2_account_id else '없음'}",
            f"R2 키        {'설정됨' if self.r2_access_key_id else '없음'}",
        ]


# ── API 클라이언트 ───────────────────────────────────────────────────────


class ApiError(Exception):
    pass


class ApiClient:
    """러너 전용 엔드포인트 4개. 인증은 X-Solver-Runner-Token 헤더 하나다."""

    def __init__(self, base_url, token, runner_id, session=None):
        self.base_url = base_url.rstrip("/")
        self.runner_id = runner_id
        self._token = token
        if session is None:
            import requests

            session = requests.Session()
        self.session = session
        self.session.headers.update(
            {
                "X-Solver-Runner-Token": token,
                "Content-Type": "application/json",
                "Accept": "application/json",
                "User-Agent": f"pokergoat-solver-runner/{runner_id}",
            }
        )

    def _url(self, path):
        return f"{self.base_url}/api/v1/solver/runner/{path.lstrip('/')}"

    def _post(self, path, payload):
        url = self._url(path)
        response = self.session.post(url, json=payload, timeout=HTTP_TIMEOUT_SEC)
        status = response.status_code
        if status >= 400:
            # 본문에 토큰이 실릴 일은 없지만 길이는 잘라서 남긴다.
            body = (response.text or "")[:500]
            raise ApiError(f"{status} {path} {body}")
        return response

    def claim(self):
        """대기 잡 하나. 없으면 None (API가 204를 준다)."""
        response = self._post("jobs/claim/", {"runner_id": self.runner_id})
        if response.status_code == 204:
            return None
        return response.json()

    def heartbeat(self, job_id):
        self._post(f"jobs/{job_id}/heartbeat/", {"runner_id": self.runner_id})

    def complete(self, job_id, manifest, blob_prefix, size_bytes, node_count, exploitability):
        payload = {
            "runner_id": self.runner_id,
            "manifest": manifest,
            "blob_prefix": blob_prefix,
            "size_bytes": int(size_bytes),
            "node_count": int(node_count),
            "exploitability": exploitability,
        }
        self._post(f"jobs/{job_id}/complete/", payload)

    def fail(self, job_id, error, reason=None):
        payload = {
            "runner_id": self.runner_id,
            "error": (error or "")[-MAX_ERROR_CHARS:],
        }
        if reason:
            payload["reason"] = reason
            payload["retry"] = reason not in NO_RETRY_REASONS
        self._post(f"jobs/{job_id}/fail/", payload)


class Heartbeat:
    """solve가 도는 동안 60초마다 진행 신호를 보내는 스레드."""

    def __init__(self, api, job_id, interval=HEARTBEAT_INTERVAL_SEC):
        self.api = api
        self.job_id = job_id
        self.interval = interval
        self._done = threading.Event()
        self._thread = None

    def _loop(self):
        while not self._done.wait(self.interval):
            try:
                self.api.heartbeat(self.job_id)
            except Exception as exc:  # 하트비트 실패로 솔브를 죽이지 않는다
                log(f"하트비트 실패 (잡 {self.job_id}): {exc}")

    def __enter__(self):
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc_info):
        self._done.set()
        if self._thread is not None:
            self._thread.join(timeout=5)
        return False


# ── R2 업로드 ───────────────────────────────────────────────────────────


def build_r2_client(config):
    """R2용 S3 호환 클라이언트. 테스트는 이 함수를 갈아 끼운다."""
    import boto3
    from botocore.config import Config as BotoConfig

    options = {
        "signature_version": "s3v4",
        "retries": {"max_attempts": 5, "mode": "standard"},
        # 워커 스레드 수만큼 동시 연결을 열 수 있어야 풀에서 대기하지 않는다.
        # boto3 클라이언트는 스레드 세이프해서 워커 전체가 이 클라이언트
        # 하나를 공유한다 (호출부는 Runner.r2 프로퍼티 참고).
        "max_pool_connections": max(config.upload_workers, 10),
        # R2는 boto3 1.36부터 기본으로 붙는 flexible checksum을 다 받아주지
        # 않는다. 필요할 때만 계산하도록 낮춘다.
        "request_checksum_calculation": "when_required",
        "response_checksum_validation": "when_supported",
    }
    try:
        boto_config = BotoConfig(**options)
    except TypeError:
        # 체크섬 옵션이 없는 옛 botocore.
        for key in ("request_checksum_calculation", "response_checksum_validation"):
            options.pop(key, None)
        boto_config = BotoConfig(**options)

    return boto3.client(
        "s3",
        endpoint_url=config.r2_endpoint,
        aws_access_key_id=config.r2_access_key_id,
        aws_secret_access_key=config.r2_secret_access_key,
        region_name="auto",
        config=boto_config,
    )


def content_type_for(name):
    """확장자로 Content-Type을 고른다. manifest.json만 JSON이고 나머지는 바이너리."""
    suffix = Path(name).suffix.lower()
    return CONTENT_TYPES.get(suffix, "application/octet-stream")


def upload_args_for(name):
    """S3 put_object의 메타데이터. blob 경로는 immutable이라 캐시를 최대로 준다."""
    args = {
        "ContentType": content_type_for(name),
        "CacheControl": CACHE_CONTROL,
    }
    if name.lower().endswith(".br"):
        # 미리 brotli로 압축해 저장한 파일이라 브라우저가 알아서 푼다.
        args["ContentEncoding"] = "br"
    return args


def iter_upload_files(out_dir):
    """out/ 아래 파일을 (상대경로, 절대경로)로 정렬해 돌려준다."""
    out_dir = Path(out_dir)
    files = [path for path in out_dir.rglob("*") if path.is_file()]
    files.sort()
    return [(str(path.relative_to(out_dir)), path) for path in files]


def build_key(blob_prefix, relative_path):
    """{blob_prefix}{relative_path}. 윈도 구분자는 쓰지 않는다."""
    prefix = blob_prefix if blob_prefix.endswith("/") else blob_prefix + "/"
    return prefix + str(relative_path).replace(os.sep, "/")


class UploadError(Exception):
    """파일 하나가 재시도 후에도 업로드에 실패했다. key와 원인을 들고 있다."""

    def __init__(self, key, cause):
        super().__init__(f"{key}: {cause}")
        self.key = key
        self.cause = cause


def _put_object_with_retry(client, bucket, key, path, args, attempts=UPLOAD_RETRY_ATTEMPTS):
    """put_object를 최대 attempts번 시도한다.

    botocore 예외(네트워크 끊김, 429, 5xx 등)만 재시도 대상이다. 그 외
    예외(로컬 파일 IO 오류 등)는 재시도해도 소용이 없으니 바로 올려보낸다.
    """
    from botocore.exceptions import BotoCoreError, ClientError

    last_exc = None
    for attempt in range(1, attempts + 1):
        try:
            with open(path, "rb") as handle:
                return client.put_object(Bucket=bucket, Key=key, Body=handle, **args)
        except (BotoCoreError, ClientError) as exc:
            last_exc = exc
            if attempt < attempts:
                time.sleep(UPLOAD_RETRY_BACKOFF_BASE_SEC * (2 ** (attempt - 1)))
    raise UploadError(key, last_exc)


def upload_directory(client, bucket, out_dir, blob_prefix, workers=DEFAULT_UPLOAD_WORKERS):
    """out/의 모든 파일을 R2에 병렬로 올리고 (키, 바이트) 목록을 돌려준다.

    blob(.bin.br 등)을 ThreadPoolExecutor로 동시에 올린 뒤, 전부 성공해야만
    manifest.json을 마지막으로 하나 더 올린다. 도중에 잡이 죽거나 파일 하나가
    끝내 실패해도 manifest 없는 반쪽 솔루션이 CDN에 노출되지 않는다.

    검증은 별도 HEAD를 치지 않는다. S3 호환 PUT은 원자적이라 put_object가
    예외 없이 돌아오면 R2가 그 객체를 통째로 받았다는 뜻이고, 재시도도
    botocore가 붙잡지 못한 실패만 여기서 다시 돈다. 그래서 성공 응답 자체가
    이미 존재+크기 검증이다 (2400개 파일마다 왕복을 하나씩 더 태우던 HEAD를
    없애는 이유). 로컬에서 이미 알고 있는 바이트 수를 그대로 반환값에 쓴다.
    """
    out_dir = Path(out_dir)
    entries = iter_upload_files(out_dir)
    manifest_entry = None
    blob_entries = []
    for relative, path in entries:
        if relative == MANIFEST_FILENAME:
            manifest_entry = (relative, path)
        else:
            blob_entries.append((relative, path))

    uploaded = []
    failures = []
    started = time.monotonic()
    done = 0

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, workers)) as pool:
        future_to_entry = {
            pool.submit(
                _put_object_with_retry,
                client,
                bucket,
                build_key(blob_prefix, relative),
                path,
                upload_args_for(relative),
            ): (relative, path)
            for relative, path in blob_entries
        }
        for future in concurrent.futures.as_completed(future_to_entry):
            relative, path = future_to_entry[future]
            key = build_key(blob_prefix, relative)
            try:
                future.result()
            except UploadError as exc:
                failures.append(exc.key)
                continue
            uploaded.append((key, path.stat().st_size))
            done += 1
            if done % UPLOAD_LOG_EVERY == 0:
                log(f"업로드 진행 {done}/{len(blob_entries)}개 -> {blob_prefix}")

    if failures:
        shown = ", ".join(failures[:5])
        more = f" 외 {len(failures) - 5}개" if len(failures) > 5 else ""
        raise JobFailure(
            f"업로드 실패 (재시도 {UPLOAD_RETRY_ATTEMPTS}회 소진, "
            f"{len(failures)}개 파일): {shown}{more}"
        )

    if manifest_entry is not None:
        relative, path = manifest_entry
        key = build_key(blob_prefix, relative)
        _put_object_with_retry(client, bucket, key, path, upload_args_for(relative))
        uploaded.append((key, path.stat().st_size))

    elapsed = max(time.monotonic() - started, 1e-9)
    total_bytes = sum(size for _, size in uploaded)
    mb_per_sec = (total_bytes / 1024**2) / elapsed
    log(
        f"업로드 완료 {len(uploaded)}개 파일 {total_bytes / 1024**2:.1f}MB "
        f"{elapsed:.1f}s ({mb_per_sec:.1f}MB/s) -> {blob_prefix}"
    )
    return uploaded


# ── CLI 실행 ────────────────────────────────────────────────────────────


def run_cli(argv, timeout, cwd=None):
    """pokergoat-solver 호출. (returncode, stdout, stderr)."""
    proc = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=str(cwd) if cwd else None,
        check=False,
    )
    return proc.returncode, proc.stdout, proc.stderr


def run_estimate(solver_bin, config_path):
    """estimate JSON. 실패하면 JobFailure."""
    argv = [str(solver_bin), "estimate", "--config", str(config_path)]
    try:
        code, stdout, stderr = run_cli(argv, ESTIMATE_TIMEOUT_SEC)
    except subprocess.TimeoutExpired as exc:
        raise JobFailure(f"estimate 시간 초과 ({ESTIMATE_TIMEOUT_SEC}s)") from exc
    except OSError as exc:
        raise JobFailure(f"솔버 바이너리를 실행하지 못했다: {exc}") from exc
    if code != 0:
        raise JobFailure(f"estimate 실패 (코드 {code}): {tail(stderr)}")
    try:
        return json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise JobFailure(f"estimate 출력을 JSON으로 읽지 못했다: {exc}") from exc


def estimated_memory_bytes(estimate):
    """실제로 할당될 바이트. CLI의 `--memory auto`와 같은 기준으로 고른다.

    auto는 비압축 크기가 8GiB를 넘을 때만 압축 저장(i16 + 스케일)으로 내려간다.
    러너는 auto를 그대로 쓰므로 상한 검사도 같은 규칙이어야 한다.
    """
    memory = (estimate or {}).get("memoryBytes") or {}
    uncompressed = _number(memory.get("uncompressed"))
    compressed = _number(memory.get("compressed"))
    if uncompressed is None:
        return None
    if uncompressed > MEMORY_AUTO_THRESHOLD_BYTES and compressed is not None:
        return int(compressed)
    return int(uncompressed)


def check_memory_limit(estimate, max_bytes):
    """상한을 넘으면 JobFailure. 넘지 않으면 사용 예정 바이트를 돌려준다."""
    needed = estimated_memory_bytes(estimate)
    if needed is None:
        raise JobFailure("estimate에 memoryBytes.uncompressed가 없다")
    if needed > max_bytes:
        raise JobFailure(
            "메모리 상한 초과: 예상 "
            f"{needed / 1024**3:.2f}GB > 상한 {max_bytes / 1024**3:.2f}GB. "
            "트리를 줄이거나 SOLVER_MAX_MEMORY_GB를 올려야 한다",
            reason="memory",
        )
    return needed


def tail(text, limit=MAX_ERROR_CHARS):
    return (text or "").strip()[-limit:]


def read_tail_file(path, limit=MAX_ERROR_CHARS):
    try:
        data = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    return tail(data, limit)


def run_solve(
    solver_bin, config_path, out_dir, threads, time_limit, stderr_path, extra_args=None
):
    """solve 서브프로세스. STOP이 서면 죽이고 RunnerStopped를 올린다.

    `extra_args`(SOLVER_SOLVE_ARGS)는 맨 뒤에 붙는다. 뒤에 오는 값이 이기는
    clap 규칙이라 러너가 세운 기본 인자를 덮어쓸 수도 있다.
    """
    argv = [
        str(solver_bin),
        "solve",
        "--config",
        str(config_path),
        "--out",
        str(out_dir),
        "--threads",
        str(threads),
        "--time-limit",
        str(time_limit),
    ]
    argv.extend(extra_args or [])
    extra_note = f" extra={' '.join(extra_args)}" if extra_args else ""
    log(f"solve 실행: threads={threads} time-limit={time_limit}s out={out_dir}{extra_note}")

    hard_deadline = time.monotonic() + time_limit + SOLVE_EXPORT_GRACE_SEC
    stdout_path = Path(stderr_path).parent / "solve.out.log"
    with open(stdout_path, "w", encoding="utf-8") as out_handle, open(
        stderr_path, "w", encoding="utf-8"
    ) as err_handle:
        try:
            proc = subprocess.Popen(argv, stdout=out_handle, stderr=err_handle)
        except OSError as exc:
            raise JobFailure(f"솔버 바이너리를 실행하지 못했다: {exc}") from exc

        while True:
            try:
                code = proc.wait(timeout=1)
                break
            except subprocess.TimeoutExpired:
                pass
            if STOP.is_set():
                terminate(proc)
                raise RunnerStopped()
            if time.monotonic() > hard_deadline:
                terminate(proc)
                raise JobFailure(
                    f"solve가 시간 상한 {time_limit}s + 마감 여유 "
                    f"{SOLVE_EXPORT_GRACE_SEC}s를 넘겨 강제 종료했다"
                )

    if code != 0:
        raise JobFailure(
            f"solve 실패 (코드 {code}): {read_tail_file(stderr_path)}"
        )


def terminate(proc):
    """SIGTERM 먼저, 안 죽으면 SIGKILL."""
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


# ── manifest 매핑 ───────────────────────────────────────────────────────


def _number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def trim_manifest(manifest):
    """API의 16KB 상한에 맞춘 manifest.

    CLI manifest의 files와 턴 동형 매핑은 그 둘만으로도 수십 KB다. 잡 기록에
    필요한 건 요약값이라 큰 배열을 빼고, 설계서 4.5 이름(snake_case) 별칭을
    더한다. API가 스팟 요약으로 옮길 때 그 이름으로 찾기 때문이다.
    """
    trimmed = {
        key: value
        for key, value in (manifest or {}).items()
        if key not in MANIFEST_BULK_KEYS
    }

    aliases = {
        "elapsed": trimmed.get("elapsedSec"),
        "node_count": trimmed.get("nodeCount"),
        "memory_peak": trimmed.get("memoryBytes"),
        "solver_version": trimmed.get("solverVersion"),
        "template_id": trimmed.get("template") or trimmed.get("templateId"),
    }
    for key, value in aliases.items():
        if value is not None and key not in trimmed:
            trimmed[key] = value

    if len(json.dumps(trimmed)) <= MAX_MANIFEST_CHARS:
        return trimmed

    core = {key: trimmed[key] for key in MANIFEST_CORE_KEYS if key in trimmed}
    core.update({key: value for key, value in aliases.items() if value is not None})
    return core


def exploitability_field(manifest):
    """complete의 exploitability (% pot). DecimalField 범위를 벗어나면 None."""
    value = _number((manifest or {}).get("exploitabilityPctPot"))
    if value is None or value < 0 or value > EXPLOITABILITY_MAX:
        return None
    return round(float(value), EXPLOITABILITY_DECIMALS)


def completion_payload(manifest, uploaded_bytes):
    """manifest에서 complete 본문의 숫자 필드를 뽑는다."""
    manifest = manifest or {}
    stored = _number(manifest.get("bytesStored"))
    nodes = _number(manifest.get("nodeCount"))
    return {
        "manifest": trim_manifest(manifest),
        "size_bytes": int(stored) if stored is not None else int(uploaded_bytes),
        "node_count": int(nodes) if nodes is not None else 0,
        "exploitability": exploitability_field(manifest),
    }


def job_config_document(job):
    """work/{id}/job.json에 쓸 SolverConfig.

    API의 잡 페이로드는 config와 메타(scenario, template_id)를 따로 준다.
    CLI는 blob 헤더와 manifest에 쓰려고 config 안에서 scenario와 template을
    읽으므로, config에 없을 때만 사람이 읽는 식별자를 채워 넣는다.
    """
    config = dict(job.get("config") or {})
    scenario = job.get("scenario")
    if scenario and not config.get("scenario"):
        config["scenario"] = scenario
    template = job.get("template_id")
    if template and not config.get("template"):
        config["template"] = template
    return config


# ── 잡 처리 ─────────────────────────────────────────────────────────────


class Runner:
    def __init__(self, config, api=None, r2=None):
        self.config = config
        self.api = api or ApiClient(config.api_base, config.token, config.runner_id)
        self._r2 = r2

    @property
    def r2(self):
        if self._r2 is None:
            self._r2 = build_r2_client(self.config)
        return self._r2

    def job_dir(self, job_id):
        return self.config.work_dir / str(job_id)

    def handle_job(self, job):
        """잡 하나. 성공하면 complete, 실패하면 fail을 보내고 True/False."""
        job_id = job["id"]
        blob_prefix = job.get("blob_prefix") or ""
        label = job.get("scenario") or f"spot {job.get('spot_id')}"
        log(
            f"잡 {job_id} 시작: kind={job.get('kind')} {label} "
            f"{job.get('flop_iso') or ''} 시도 {job.get('attempts')}"
        )

        work = self.job_dir(job_id)
        out_dir = work / "out"
        try:
            if not blob_prefix:
                raise JobFailure("잡에 blob_prefix가 없다")

            shutil.rmtree(work, ignore_errors=True)
            out_dir.mkdir(parents=True, exist_ok=True)
            config_path = work / "job.json"
            config_path.write_text(
                json.dumps(job_config_document(job), ensure_ascii=False, indent=2),
                encoding="utf-8",
            )

            estimate = run_estimate(self.config.solver_bin, config_path)
            needed = check_memory_limit(estimate, self.config.max_memory_bytes)
            log(
                f"잡 {job_id} estimate: 노드 {estimate.get('nodeCount')} "
                f"메모리 {needed / 1024**3:.2f}GB"
            )

            with Heartbeat(self.api, job_id):
                run_solve(
                    self.config.solver_bin,
                    config_path,
                    out_dir,
                    self.config.threads,
                    self.config.job_time_limit_sec,
                    work / "solve.err.log",
                    extra_args=self.config.solve_args,
                )

            manifest_path = out_dir / "manifest.json"
            if not manifest_path.is_file():
                raise JobFailure("solve가 manifest.json을 남기지 않았다")
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

            uploaded = upload_directory(
                self.r2,
                self.config.r2_bucket,
                out_dir,
                blob_prefix,
                workers=self.config.upload_workers,
            )
            if not uploaded:
                raise JobFailure("업로드할 파일이 없다")
            total_bytes = sum(size for _, size in uploaded)

            payload = completion_payload(manifest, total_bytes)
            self.api.complete(
                job_id,
                manifest=payload["manifest"],
                blob_prefix=blob_prefix,
                size_bytes=payload["size_bytes"],
                node_count=payload["node_count"],
                exploitability=payload["exploitability"],
            )
            log(
                f"잡 {job_id} 완료: 노드 {payload['node_count']} "
                f"익스플로이터빌리티 {payload['exploitability']}% pot"
            )
            shutil.rmtree(work, ignore_errors=True)
            return True

        except RunnerStopped:
            self.report_failure(job_id, "runner stopped")
            raise
        except JobFailure as exc:
            self.report_failure(job_id, str(exc), reason=exc.reason)
            return False
        except Exception as exc:  # 예상 못 한 오류도 잡을 놓아주고 다음으로 간다
            detail = read_tail_file(work / "solve.err.log")
            message = f"{type(exc).__name__}: {exc}"
            if detail:
                message = f"{message}\n{detail}"
            self.report_failure(job_id, message)
            return False

    def report_failure(self, job_id, message, reason=None):
        log(f"잡 {job_id} 실패: {tail(message, 500)}")
        try:
            if reason:
                self.api.fail(job_id, message, reason=reason)
            else:
                self.api.fail(job_id, message)
        except Exception as exc:
            log(f"잡 {job_id} 실패 보고를 못 보냈다: {exc}")

    def run_forever(self, once=False):
        """claim 루프. 204면 폴링 간격만큼 쉬고 다시 두드린다."""
        self.config.work_dir.mkdir(parents=True, exist_ok=True)
        idle_logged = False

        while not STOP.is_set():
            try:
                job = self.api.claim()
            except Exception as exc:
                log(f"claim 실패: {exc}")
                if once:
                    return 1
                STOP.wait(self.config.poll_interval_sec)
                continue

            if job is None:
                if not idle_logged:
                    log(f"대기 중인 잡 없음. {self.config.poll_interval_sec}s마다 폴링한다")
                    idle_logged = True
                if once:
                    return 0
                STOP.wait(self.config.poll_interval_sec)
                continue

            idle_logged = False
            try:
                self.handle_job(job)
            except RunnerStopped:
                log("중단 신호를 받아 종료한다")
                return 0
            if once:
                return 0

        return 0


# ── 점검 모드 ───────────────────────────────────────────────────────────


def run_check(config):
    """바이너리와 R2 접근만 확인한다. claim은 하지 않는다."""
    print("설정")
    for line in config.summary_lines():
        print(f"  {line}")

    ok = True

    missing = config.missing_keys()
    if missing:
        print(f"\n[FAIL] 필수 env 없음: {', '.join(missing)}")
        ok = False

    print("\n바이너리")
    if not config.solver_bin.is_file():
        print(f"  [FAIL] 없음: {config.solver_bin} (install-bin.sh로 설치)")
        ok = False
    else:
        try:
            code, stdout, stderr = run_cli(
                [str(config.solver_bin), "validate"], VALIDATE_TIMEOUT_SEC
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            print(f"  [FAIL] validate 실행 실패: {exc}")
            ok = False
        else:
            if code == 0:
                print(f"  [OK] validate 통과 {tail(stdout, 200)}")
            else:
                print(f"  [FAIL] validate 실패 (코드 {code}): {tail(stderr, 300)}")
                ok = False

    print("\nR2")
    if missing:
        print("  [SKIP] R2 키가 없어 건너뛴다")
    else:
        try:
            client = build_r2_client(config)
            client.head_bucket(Bucket=config.r2_bucket)
            listing = client.list_objects_v2(Bucket=config.r2_bucket, MaxKeys=1)
            count = listing.get("KeyCount", 0)
            print(f"  [OK] 버킷 {config.r2_bucket} 접근 가능 (객체 표본 {count}개)")
        except Exception as exc:
            print(f"  [FAIL] 버킷 접근 실패: {exc}")
            ok = False

    print("\n" + ("점검 통과" if ok else "점검 실패"))
    return 0 if ok else 1


# ── 진입점 ──────────────────────────────────────────────────────────────


def install_signal_handlers():
    def handler(signum, _frame):
        log(f"신호 {signum} 수신. 현재 잡을 정리하고 종료한다")
        STOP.set()

    signal.signal(signal.SIGTERM, handler)
    signal.signal(signal.SIGINT, handler)


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="POKER GOAT 솔버 로컬 러너",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="바이너리와 R2 접근만 점검하고 종료 (잡을 claim하지 않는다)",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="잡 하나만 처리하고 종료 (SOLVER_ONCE=1과 같다)",
    )
    parser.add_argument(
        "--env",
        default=str(RUNNER_DIR / ".env"),
        help="설정 파일 경로 (기본 runner/.env)",
    )
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    apply_env_file(args.env)
    try:
        config = RunnerConfig()
    except ValueError as exc:
        log(str(exc))
        return 1

    if args.check:
        return run_check(config)

    missing = config.missing_keys()
    if missing:
        log(f"필수 env가 없다: {', '.join(missing)}. runner/.env.example 참고")
        return 1
    if not config.solver_bin.is_file():
        log(f"솔버 바이너리가 없다: {config.solver_bin}. install-bin.sh로 설치")
        return 1

    install_signal_handlers()
    log(f"러너 시작 (id={config.runner_id}, API={config.api_base})")
    runner = Runner(config)
    return runner.run_forever(once=args.once or config.once)


if __name__ == "__main__":
    sys.exit(main())
