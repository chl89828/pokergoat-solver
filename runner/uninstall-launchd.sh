#!/bin/bash
# 솔버 러너 launchd 등록 해제. 로그와 .env는 지우지 않는다.
set -euo pipefail

LABEL="com.pokergoat.solver-runner"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

launchctl unload "$PLIST" 2>/dev/null || true
rm -f "$PLIST"

echo "제거: $LABEL"
echo "돌던 잡이 있었다면 하트비트가 끊긴 뒤 API가 큐로 되돌린다 (기본 10분)."
