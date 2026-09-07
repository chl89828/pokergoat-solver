"""러너 단위 테스트.

네트워크와 R2는 전부 목이다. 실행:

    /Users/dong/projects/pokergoat/.venv/bin/python -m pytest runner -q
"""

import json
import sys
from pathlib import Path
from unittest import mock

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import runner as R  # noqa: E402


# ── 픽스처 ──────────────────────────────────────────────────────────────


MANIFEST = {
    "formatVersion": 0,
    "solverVersion": 1,
    "engine": "postflop-solver@9d1509f",
    "cliVersion": "0.1.0",
    "scenario": "c6m_100_srp_btn_bb",
    "template": "t_simple_v1",
    "templateId": 0,
    "board": "As7d2c",
    "boardCards": [48, 22, 3],
    "street": "flop",
    "iterations": 110,
    "exploitability": 1.4841442108154297,
    "exploitabilityPctPot": 0.26984440196644177,
    "elapsedSec": 15.562253431,
    "memoryBytes": 902000000,
    "nodeCount": 1332,
    "prunedNodes": 4,
    "bytesRaw": 1512309,
    "bytesStored": 812947,
    "compression": "brotli-9",
    "turnCards": ["2s", "2h"],
    "turnIsomorphism": {"2d": "2s"},
    "files": [{"path": "flop.bin.br", "bytesRaw": 11390, "bytesStored": 6833, "nodes": 9}],
    "warnings": [],
}

JOB = {
    "id": 42,
    "kind": "library",
    "priority": 0,
    "attempts": 0,
    "solver_version": 1,
    "blob_prefix": "solutions/c6m_100_srp_btn_bb/As7d2c/",
    "scenario": "c6m_100_srp_btn_bb",
    "flop_iso": "As7d2c",
    "spot_id": None,
    "template_id": "t_simple_v1",
    "template_version": 1,
    "config": {"board": "As7d2c", "pot": 5.5, "iterations": 1000},
}


@pytest.fixture(autouse=True)
def _clear_stop():
    R.STOP.clear()
    yield
    R.STOP.clear()


@pytest.fixture
def config(tmp_path):
    env = {
        "SOLVER_API_BASE": "https://api.example.test/",
        "SOLVER_RUNNER_TOKEN": "secret-token",
        "SOLVER_RUNNER_ID": "test-mac",
        "SOLVER_BIN": str(tmp_path / "pokergoat-solver"),
        "SOLVER_WORK_DIR": str(tmp_path / "work"),
        "R2_ACCOUNT_ID": "acct",
        "R2_ACCESS_KEY_ID": "akid",
        "R2_SECRET_ACCESS_KEY": "secret",
        "R2_BUCKET": "pokergoat-solver",
    }
    with mock.patch.dict(R.os.environ, env, clear=False):
        yield R.RunnerConfig()


class FakeResponse:
    def __init__(self, status_code=200, payload=None, text=""):
        self.status_code = status_code
        self._payload = payload
        self.text = text

    def json(self):
        return self._payload


class FakeSession:
    """requests.Session 대역. post 호출을 기록하고 정해둔 응답을 준다."""

    def __init__(self, responses=None):
        self.headers = {}
        self.calls = []
        self.responses = list(responses or [])

    def post(self, url, json=None, timeout=None):
        self.calls.append({"url": url, "json": json, "timeout": timeout})
        if self.responses:
            return self.responses.pop(0)
        return FakeResponse(200, {})


def make_api(responses=None):
    session = FakeSession(responses)
    api = R.ApiClient("https://api.example.test", "secret-token", "test-mac", session)
    return api, session


# ── .env 로딩 ───────────────────────────────────────────────────────────


def test_load_env_file_parses_and_skips_comments(tmp_path):
    path = tmp_path / ".env"
    path.write_text(
        "\n".join(
            [
                "# 주석",
                "",
                'SOLVER_API_BASE="https://a.test"',
                "SOLVER_THREADS=8",
                "EMPTY=",
                "not a pair",
            ]
        ),
        encoding="utf-8",
    )
    assert R.load_env_file(path) == {
        "SOLVER_API_BASE": "https://a.test",
        "SOLVER_THREADS": "8",
        "EMPTY": "",
    }


def test_apply_env_file_does_not_override_process_env(tmp_path):
    path = tmp_path / ".env"
    path.write_text("SOLVER_ONCE=0\nSOLVER_POLL_INTERVAL_SEC=99\n", encoding="utf-8")
    with mock.patch.dict(R.os.environ, {"SOLVER_ONCE": "1"}, clear=False):
        R.apply_env_file(path)
        assert R.os.environ["SOLVER_ONCE"] == "1"
        assert R.os.environ["SOLVER_POLL_INTERVAL_SEC"] == "99"


def test_sanitize_runner_id_matches_api_pattern():
    assert R.sanitize_runner_id("동이의 MacBook Pro") == "----MacBook-Pro"
    assert R.RUNNER_ID_PATTERN.match(R.sanitize_runner_id("동이의 MacBook Pro"))
    assert R.sanitize_runner_id("") == "mac-runner"
    assert len(R.sanitize_runner_id("x" * 200)) == 64


def test_config_summary_never_leaks_secrets(config):
    text = "\n".join(config.summary_lines())
    assert "secret-token" not in text
    assert "akid" not in text
    assert "secret" not in text.replace("R2 키        설정됨", "")


# ── claim 루프 ──────────────────────────────────────────────────────────


def test_claim_returns_none_on_204():
    api, session = make_api([FakeResponse(204)])
    assert api.claim() is None
    assert session.calls[0]["url"].endswith("/api/v1/solver/runner/jobs/claim/")
    assert session.calls[0]["json"] == {"runner_id": "test-mac"}


def test_claim_returns_payload_on_200():
    api, _ = make_api([FakeResponse(200, JOB)])
    assert api.claim()["id"] == 42


def test_claim_raises_on_error_status():
    api, _ = make_api([FakeResponse(401, None, "unauthorized")])
    with pytest.raises(R.ApiError):
        api.claim()


def test_run_forever_sleeps_when_queue_empty(config):
    api = mock.Mock()
    api.claim.return_value = None
    runner = R.Runner(config, api=api, r2=mock.Mock())

    waits = []

    def fake_wait(seconds):
        waits.append(seconds)
        R.STOP.set()
        return True

    with mock.patch.object(R.STOP, "wait", side_effect=fake_wait):
        assert runner.run_forever() == 0

    assert waits == [config.poll_interval_sec]
    api.claim.assert_called_once_with()


def test_run_forever_once_returns_after_single_job(config):
    api = mock.Mock()
    api.claim.return_value = dict(JOB)
    runner = R.Runner(config, api=api, r2=mock.Mock())
    with mock.patch.object(runner, "handle_job", return_value=True) as handle:
        assert runner.run_forever(once=True) == 0
    handle.assert_called_once()
    assert api.claim.call_count == 1


def test_run_forever_survives_claim_errors(config):
    api = mock.Mock()
    api.claim.side_effect = R.ApiError("500 boom")
    runner = R.Runner(config, api=api, r2=mock.Mock())
    assert runner.run_forever(once=True) == 1


# ── manifest 매핑 ───────────────────────────────────────────────────────


def test_trim_manifest_drops_bulk_keys_and_adds_aliases():
    trimmed = R.trim_manifest(MANIFEST)
    for key in R.MANIFEST_BULK_KEYS:
        assert key not in trimmed
    assert trimmed["elapsed"] == MANIFEST["elapsedSec"]
    assert trimmed["node_count"] == MANIFEST["nodeCount"]
    assert trimmed["memory_peak"] == MANIFEST["memoryBytes"]
    assert trimmed["solver_version"] == MANIFEST["solverVersion"]
    assert trimmed["template_id"] == "t_simple_v1"
    assert len(json.dumps(trimmed)) <= R.MAX_MANIFEST_CHARS


def test_trim_manifest_falls_back_to_core_keys_when_still_too_big():
    fat = dict(MANIFEST)
    fat["warnings"] = ["경고" * 200] * 40
    trimmed = R.trim_manifest(fat)
    assert len(json.dumps(trimmed)) <= R.MAX_MANIFEST_CHARS
    assert "warnings" not in trimmed
    assert trimmed["nodeCount"] == 1332
    assert trimmed["node_count"] == 1332


def test_completion_payload_maps_manifest_fields():
    payload = R.completion_payload(MANIFEST, uploaded_bytes=999)
    assert payload["size_bytes"] == 812947
    assert payload["node_count"] == 1332
    assert payload["exploitability"] == 0.2698
    assert payload["manifest"]["board"] == "As7d2c"


def test_completion_payload_falls_back_to_uploaded_bytes():
    manifest = {key: value for key, value in MANIFEST.items() if key != "bytesStored"}
    payload = R.completion_payload(manifest, uploaded_bytes=777)
    assert payload["size_bytes"] == 777


@pytest.mark.parametrize(
    "value,expected",
    [
        (0.26984440196644177, 0.2698),
        (0, 0.0),
        (-1, None),
        (10_000, None),
        ("nope", None),
        (None, None),
    ],
)
def test_exploitability_field_respects_decimal_bounds(value, expected):
    assert R.exploitability_field({"exploitabilityPctPot": value}) == expected


def test_job_config_document_fills_readable_identifiers():
    config = R.job_config_document(JOB)
    assert config["scenario"] == "c6m_100_srp_btn_bb"
    assert config["template"] == "t_simple_v1"
    assert config["pot"] == 5.5


def test_job_config_document_keeps_existing_values():
    job = dict(JOB, config={"scenario": "custom", "template": "own"})
    config = R.job_config_document(job)
    assert config["scenario"] == "custom"
    assert config["template"] == "own"


# ── 업로드 키와 메타데이터 ────────────────────────────────────────────────


def test_build_key_joins_prefix_and_relative_path():
    assert (
        R.build_key("solutions/scen/As7d2c/", "turn/2s.bin.br")
        == "solutions/scen/As7d2c/turn/2s.bin.br"
    )
    # 접미 슬래시가 없어도 붙여 준다.
    assert R.build_key("solutions/custom/7", "manifest.json") == (
        "solutions/custom/7/manifest.json"
    )


def test_upload_args_for_br_and_json():
    br = R.upload_args_for("turn/2s.bin.br")
    assert br["ContentType"] == "application/octet-stream"
    assert br["ContentEncoding"] == "br"
    assert br["CacheControl"] == "public, max-age=31536000, immutable"

    manifest = R.upload_args_for("manifest.json")
    assert manifest["ContentType"] == "application/json"
    assert "ContentEncoding" not in manifest

    raw = R.upload_args_for("flop.bin")
    assert raw["ContentType"] == "application/octet-stream"
    assert "ContentEncoding" not in raw


def _write_out_dir(tmp_path):
    out = tmp_path / "out"
    (out / "turn").mkdir(parents=True)
    (out / "manifest.json").write_text(json.dumps(MANIFEST), encoding="utf-8")
    (out / "flop.bin.br").write_bytes(b"flop-blob")
    (out / "turn" / "2s.bin.br").write_bytes(b"turn-blob")
    return out


def test_upload_directory_builds_keys_and_metadata(tmp_path):
    out = _write_out_dir(tmp_path)
    client = mock.Mock()
    uploaded = R.upload_directory(client, "bucket", out, JOB["blob_prefix"])

    keys = sorted(key for key, _ in uploaded)
    assert keys == [
        "solutions/c6m_100_srp_btn_bb/As7d2c/flop.bin.br",
        "solutions/c6m_100_srp_btn_bb/As7d2c/manifest.json",
        "solutions/c6m_100_srp_btn_bb/As7d2c/turn/2s.bin.br",
    ]
    by_key = {call.kwargs["Key"]: call.kwargs for call in client.put_object.call_args_list}
    turn = by_key["solutions/c6m_100_srp_btn_bb/As7d2c/turn/2s.bin.br"]
    assert turn["ContentEncoding"] == "br"
    assert turn["Bucket"] == "bucket"
    assert "ContentEncoding" not in by_key[
        "solutions/c6m_100_srp_btn_bb/As7d2c/manifest.json"
    ]


def test_verify_uploads_flags_size_mismatch():
    client = mock.Mock()
    client.head_object.return_value = {"ContentLength": 5}
    with pytest.raises(R.JobFailure, match="크기 불일치"):
        R.verify_uploads(client, "bucket", [("k", 9)])


def test_verify_uploads_flags_missing_object():
    client = mock.Mock()
    client.head_object.side_effect = RuntimeError("404")
    with pytest.raises(R.JobFailure, match="업로드 검증 실패"):
        R.verify_uploads(client, "bucket", [("k", 9)])


# ── 메모리 가드 ─────────────────────────────────────────────────────────


def test_check_memory_limit_passes_under_limit():
    estimate = {"memoryBytes": {"uncompressed": 2 * 1024**3}}
    assert R.check_memory_limit(estimate, 24 * 1024**3) == 2 * 1024**3


def test_check_memory_limit_fails_over_limit():
    estimate = {"memoryBytes": {"uncompressed": 30 * 1024**3}}
    with pytest.raises(R.JobFailure, match="메모리 상한 초과"):
        R.check_memory_limit(estimate, 24 * 1024**3)


def test_check_memory_limit_uses_compressed_size_above_auto_threshold():
    # 비압축 20GiB지만 CLI가 auto로 압축 저장을 고르므로 10GiB 기준으로 본다.
    estimate = {
        "memoryBytes": {"uncompressed": 20 * 1024**3, "compressed": 10 * 1024**3}
    }
    assert R.check_memory_limit(estimate, 24 * 1024**3) == 10 * 1024**3
    with pytest.raises(R.JobFailure, match="메모리 상한 초과"):
        R.check_memory_limit(estimate, 8 * 1024**3)


def test_check_memory_limit_fails_when_field_missing():
    with pytest.raises(R.JobFailure, match="memoryBytes"):
        R.check_memory_limit({"nodeCount": 1}, 24 * 1024**3)


def test_handle_job_memory_guard_posts_fail_without_solving(config):
    api = mock.Mock()
    runner = R.Runner(config, api=api, r2=mock.Mock())
    estimate = {"nodeCount": 3970233, "memoryBytes": {"uncompressed": 30 * 1024**3}}

    with mock.patch.object(R, "run_estimate", return_value=estimate), mock.patch.object(
        R, "run_solve"
    ) as solve:
        assert runner.handle_job(dict(JOB)) is False

    solve.assert_not_called()
    api.complete.assert_not_called()
    api.fail.assert_called_once()
    args, kwargs = api.fail.call_args
    assert args[0] == 42
    assert "메모리 상한 초과" in args[1]


# ── 성공 경로와 실패 경로 ────────────────────────────────────────────────


def test_handle_job_success_uploads_and_completes(config, tmp_path):
    api = mock.Mock()
    r2 = mock.Mock()
    r2.head_object.side_effect = lambda Bucket, Key: {
        "ContentLength": _sizes[Key]
    }
    runner = R.Runner(config, api=api, r2=r2)

    estimate = {"nodeCount": 1332, "memoryBytes": {"uncompressed": 1024**3}}
    _sizes = {}

    def fake_solve(solver_bin, config_path, out_dir, threads, time_limit, stderr_path):
        out = Path(out_dir)
        (out / "turn").mkdir(parents=True, exist_ok=True)
        (out / "manifest.json").write_text(json.dumps(MANIFEST), encoding="utf-8")
        (out / "flop.bin.br").write_bytes(b"flop-blob")
        (out / "turn" / "2s.bin.br").write_bytes(b"turn-blob")
        for path in out.rglob("*"):
            if path.is_file():
                key = R.build_key(JOB["blob_prefix"], path.relative_to(out))
                _sizes[key] = path.stat().st_size

    with mock.patch.object(R, "run_estimate", return_value=estimate), mock.patch.object(
        R, "run_solve", side_effect=fake_solve
    ):
        assert runner.handle_job(dict(JOB)) is True

    api.fail.assert_not_called()
    api.complete.assert_called_once()
    args, kwargs = api.complete.call_args
    assert args[0] == 42
    assert kwargs["blob_prefix"] == JOB["blob_prefix"]
    assert kwargs["size_bytes"] == 812947
    assert kwargs["node_count"] == 1332
    assert kwargs["exploitability"] == 0.2698
    assert kwargs["manifest"]["scenario"] == "c6m_100_srp_btn_bb"
    assert "files" not in kwargs["manifest"]

    assert r2.put_object.call_count == 3
    # work 디렉토리는 성공 후 지운다.
    assert not runner.job_dir(42).exists()


def test_handle_job_solve_failure_posts_fail(config):
    api = mock.Mock()
    runner = R.Runner(config, api=api, r2=mock.Mock())
    estimate = {"memoryBytes": {"uncompressed": 1024**3}}

    with mock.patch.object(R, "run_estimate", return_value=estimate), mock.patch.object(
        R, "run_solve", side_effect=R.JobFailure("solve 실패 (코드 101): 트리가 너무 크다")
    ):
        assert runner.handle_job(dict(JOB)) is False

    api.complete.assert_not_called()
    api.fail.assert_called_once()
    assert "트리가 너무 크다" in api.fail.call_args[0][1]


def test_handle_job_upload_failure_posts_fail(config):
    api = mock.Mock()
    r2 = mock.Mock()
    r2.put_object.side_effect = RuntimeError("R2 연결 끊김")
    runner = R.Runner(config, api=api, r2=r2)
    estimate = {"memoryBytes": {"uncompressed": 1024**3}}

    def fake_solve(solver_bin, config_path, out_dir, *args):
        out = Path(out_dir)
        out.mkdir(parents=True, exist_ok=True)
        (out / "manifest.json").write_text(json.dumps(MANIFEST), encoding="utf-8")
        (out / "flop.bin.br").write_bytes(b"blob")

    with mock.patch.object(R, "run_estimate", return_value=estimate), mock.patch.object(
        R, "run_solve", side_effect=fake_solve
    ):
        assert runner.handle_job(dict(JOB)) is False

    api.complete.assert_not_called()
    assert "R2 연결 끊김" in api.fail.call_args[0][1]


def test_handle_job_stop_signal_fails_with_runner_stopped(config):
    api = mock.Mock()
    runner = R.Runner(config, api=api, r2=mock.Mock())
    estimate = {"memoryBytes": {"uncompressed": 1024**3}}

    with mock.patch.object(R, "run_estimate", return_value=estimate), mock.patch.object(
        R, "run_solve", side_effect=R.RunnerStopped()
    ):
        with pytest.raises(R.RunnerStopped):
            runner.handle_job(dict(JOB))

    api.fail.assert_called_once_with(42, "runner stopped")


def test_report_failure_swallows_api_error(config):
    api = mock.Mock()
    api.fail.side_effect = R.ApiError("503")
    runner = R.Runner(config, api=api, r2=mock.Mock())
    runner.report_failure(42, "무슨 일이 있었다")  # 예외가 새면 실패


def test_api_fail_truncates_error_to_contract_limit():
    api, session = make_api([FakeResponse(200, {})])
    api.fail(42, "x" * 5000)
    assert len(session.calls[0]["json"]["error"]) == R.MAX_ERROR_CHARS


def test_api_sets_token_header_once():
    api, session = make_api()
    assert session.headers["X-Solver-Runner-Token"] == "secret-token"
    assert api.runner_id == "test-mac"


def test_heartbeat_thread_posts_while_running():
    api = mock.Mock()
    with R.Heartbeat(api, 42, interval=0.01):
        deadline = R.time.monotonic() + 2
        while api.heartbeat.call_count < 2 and R.time.monotonic() < deadline:
            R.time.sleep(0.01)
    assert api.heartbeat.call_count >= 2
    api.heartbeat.assert_called_with(42)


def test_heartbeat_survives_api_errors():
    api = mock.Mock()
    api.heartbeat.side_effect = R.ApiError("409")
    with R.Heartbeat(api, 42, interval=0.01):
        deadline = R.time.monotonic() + 1
        while api.heartbeat.call_count < 1 and R.time.monotonic() < deadline:
            R.time.sleep(0.01)
    assert api.heartbeat.call_count >= 1


def test_memory_failure_is_reported_without_retry(monkeypatch):
    """메모리 초과는 reason=memory, retry=False로 보고해 API가 재시도하지 않게 한다."""
    calls = []

    class FakeApi:
        runner_id = "mac"

        def fail(self, job_id, error, reason=None):
            calls.append((job_id, error, reason))

    api = R.ApiClient.__new__(R.ApiClient)
    api.runner_id = "mac"
    api._post = lambda path, payload: calls.append((path, payload))
    api.fail(7, "메모리 상한 초과", reason="memory")
    path, payload = calls[-1]
    assert path == "jobs/7/fail/"
    assert payload["reason"] == "memory"
    assert payload["retry"] is False

    api.fail(8, "기타 오류")
    _, payload = calls[-1]
    assert "reason" not in payload and "retry" not in payload
