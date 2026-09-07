#!/bin/bash
# 솔버 러너 launchd 등록. 상시 실행이다 (RunAtLoad + KeepAlive).
#
# 템플릿 com.pokergoat.solver-runner.plist의 경로 자리를 현재 머신 값으로
# 치환해 ~/Library/LaunchAgents/에 쓴다. 경로 하드코딩이 없어서 레포를 다른
# 위치로 옮겨도 다시 돌리면 그만이다 (마케팅 install.sh와 같은 방식).
#
# 설치:   ./install-launchd.sh [python 경로]
# 제거:   ./uninstall-launchd.sh
# 상태:   launchctl list | grep pokergoat
# 로그:   runner/logs/solver-runner.{out,err}.log
set -euo pipefail

RUNNER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOLVER_DIR="$(dirname "$RUNNER_DIR")"
ROOT_DIR="$(dirname "$SOLVER_DIR")"
LABEL="com.pokergoat.solver-runner"
LA_DIR="$HOME/Library/LaunchAgents"
PLIST="$LA_DIR/$LABEL.plist"
TEMPLATE="$RUNNER_DIR/$LABEL.plist"

# 파이썬은 인자 > 메타 레포의 공용 venv > PATH의 python3 순으로 고른다.
PYTHON="${1:-}"
if [ -z "$PYTHON" ]; then
  if [ -x "$ROOT_DIR/.venv/bin/python" ]; then
    PYTHON="$ROOT_DIR/.venv/bin/python"
  else
    PYTHON="$(command -v python3 || true)"
  fi
fi

if [ -z "$PYTHON" ] || [ ! -x "$PYTHON" ]; then
  echo "ERROR: 실행 가능한 python을 찾지 못했다. 경로를 인자로 넘겨라" >&2
  exit 1
fi

if ! "$PYTHON" -c "import boto3, requests" 2>/dev/null; then
  echo "ERROR: $PYTHON 에 boto3와 requests가 없다" >&2
  exit 1
fi

if [ ! -f "$RUNNER_DIR/.env" ]; then
  echo "ERROR: runner/.env 없음. cp .env.example .env && chmod 600 .env 후 값 입력" >&2
  exit 1
fi

if [ ! -x "$RUNNER_DIR/bin/pokergoat-solver" ]; then
  echo "ERROR: runner/bin/pokergoat-solver 없음. ./install-bin.sh 먼저 실행" >&2
  exit 1
fi

mkdir -p "$LA_DIR" "$RUNNER_DIR/logs"
chmod +x "$RUNNER_DIR/runner.py"

# 경로에 & < > 가 들어갈 일은 없지만 sed 구분자만 |로 피한다.
sed -e "s|__RUNNER_DIR__|$RUNNER_DIR|g" \
    -e "s|__PYTHON__|$PYTHON|g" \
    "$TEMPLATE" > "$PLIST"

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "  ✓ $LABEL 등록"
echo "    python  : $PYTHON"
echo "    러너    : $RUNNER_DIR/runner.py"
echo "    로그    : $RUNNER_DIR/logs/solver-runner.{out,err}.log"
echo
echo "상태: launchctl list | grep pokergoat"
echo "점검: $PYTHON $RUNNER_DIR/runner.py --check"
echo "⚠️ Mac이 켜져 있어야 돈다. caffeinate -i가 유휴 슬립만 막으므로"
echo "   뚜껑을 닫거나 직접 잠재우면 그동안 잡을 처리하지 못한다."
