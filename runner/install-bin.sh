#!/bin/bash
# 솔버 CLI 맥용(arm64) 바이너리 설치. GitHub 릴리스에서 받아 runner/bin/에 푼다.
#
# 릴리스 자산은 .github/workflows/release.yml이 태그 푸시로 만든다
# (pokergoat-solver-aarch64-apple-darwin.tar.gz + .sha256).
#
# 사용:
#   ./install-bin.sh            최신 릴리스
#   ./install-bin.sh v0.1.0     특정 태그
#
# gh CLI가 있으면 gh release download를 쓰고, 없으면 공개 릴리스 URL로 curl한다.
set -euo pipefail

RUNNER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$RUNNER_DIR/bin"
REPO="chl89828/pokergoat-solver"
TARGET="aarch64-apple-darwin"
ASSET="pokergoat-solver-$TARGET.tar.gz"
TAG="${1:-}"

arch="$(uname -m)"
if [ "$(uname -s)" != "Darwin" ] || [ "$arch" != "arm64" ]; then
  echo "경고: 이 스크립트는 애플 실리콘 맥용($TARGET) 자산을 받는다. 현재 $(uname -s)/$arch" >&2
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

download_with_gh() {
  local args=(release download)
  [ -n "$TAG" ] && args+=("$TAG")
  args+=(--repo "$REPO" --pattern "$ASSET" --pattern "$ASSET.sha256" --dir "$TMP_DIR" --clobber)
  gh "${args[@]}"
}

download_with_curl() {
  local base
  if [ -n "$TAG" ]; then
    base="https://github.com/$REPO/releases/download/$TAG"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  curl -fSL --retry 3 -o "$TMP_DIR/$ASSET" "$base/$ASSET"
  # 체크섬 파일은 없을 수도 있으니 실패해도 넘어간다.
  curl -fsSL --retry 2 -o "$TMP_DIR/$ASSET.sha256" "$base/$ASSET.sha256" || true
}

echo "릴리스 자산 내려받는 중: $REPO ${TAG:-latest} / $ASSET"
if command -v gh >/dev/null 2>&1; then
  download_with_gh
else
  echo "gh가 없어 curl로 공개 릴리스 URL을 쓴다 (비공개 레포면 gh를 설치해 로그인해야 한다)"
  download_with_curl
fi

if [ ! -f "$TMP_DIR/$ASSET" ]; then
  echo "ERROR: $ASSET 을 받지 못했다" >&2
  exit 1
fi

if [ -s "$TMP_DIR/$ASSET.sha256" ]; then
  echo "체크섬 확인"
  # 릴리스의 sha256 파일은 자산과 같은 디렉토리 기준 상대 경로로 적혀 있다.
  (cd "$TMP_DIR" && shasum -a 256 -c "$ASSET.sha256")
else
  echo "체크섬 파일이 없어 건너뛴다"
fi

echo "압축 해제"
tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"

SRC="$TMP_DIR/pokergoat-solver-$TARGET/pokergoat-solver"
if [ ! -f "$SRC" ]; then
  # 패키징 구조가 바뀐 경우를 대비해 한 번 찾아본다.
  SRC="$(find "$TMP_DIR" -type f -name pokergoat-solver -perm -u+x | head -1)"
fi
if [ -z "$SRC" ] || [ ! -f "$SRC" ]; then
  echo "ERROR: 압축 안에서 pokergoat-solver 실행 파일을 찾지 못했다" >&2
  exit 1
fi

mkdir -p "$BIN_DIR"
install -m 755 "$SRC" "$BIN_DIR/pokergoat-solver"

# 다운로드한 바이너리는 격리 속성이 붙어 Gatekeeper가 막는다.
xattr -d com.apple.quarantine "$BIN_DIR/pokergoat-solver" 2>/dev/null || true

echo "설치: $BIN_DIR/pokergoat-solver"
"$BIN_DIR/pokergoat-solver" --version || true

echo "토이 게임 자가 점검 (validate)"
"$BIN_DIR/pokergoat-solver" validate

echo "완료. 다음: ./runner.py --check"
