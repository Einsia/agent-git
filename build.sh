#!/usr/bin/env bash
# 用「能用的」cargo 编译，而不是 PATH 里碰巧排在前面的那个。
#
# 背景：Ubuntu 22.04 的 apt 里是 cargo 1.75，它读不了本仓库 Cargo.lock 的 v4 格式，
# 也编不了依赖树里用 edition2024 的 crate。而 rustup 装在 ~/.cargo/bin，
# 除非你 source 过 ~/.cargo/env，否则它不在 PATH 上。
#
# 这个脚本按优先级挑，不需要你改任何 dotfile。
#
#   ./build.sh              debug
#   ./build.sh --release    release
#   ./build.sh test         跑测试

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

pick_cargo() {
  local candidates=(
    "${CARGO:-}"                 # 显式覆盖
    "$HOME/.cargo/bin/cargo"     # rustup
    "$(command -v cargo 2>/dev/null || true)"
  )
  for c in "${candidates[@]}"; do
    [[ -n "$c" && -x "$c" ]] || continue
    # 判据不是版本号，是「它能不能解析这个仓库的 manifest + lockfile」。
    # 不加 --locked：允许它按需更新 Cargo.lock（加依赖后 lock 会滞后）。
    if "$c" metadata --format-version 1 >/dev/null 2>&1; then
      echo "$c"; return 0
    fi
  done
  return 1
}

if ! CARGO_BIN="$(pick_cargo)"; then
  cat >&2 <<'EOF'
找不到能编译本仓库的 cargo。

  Cargo.lock 是 v4 格式，需要 cargo >= 1.78。
  Ubuntu 22.04 的 apt 版是 1.75，不行。

装一个：
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path

然后重新跑 ./build.sh —— 它会自己找到 ~/.cargo/bin/cargo，不需要你改 PATH。
EOF
  exit 1
fi

echo "cargo: $CARGO_BIN ($("$CARGO_BIN" --version | awk '{print $2}'))" >&2

case "${1:-}" in
  ui)   # 重建 Hub 前端（hub-ui/dist 被 agit-hub 用 include_str! 嵌进二进制）。
        shift
        cd hub-ui
        [[ -d node_modules ]] || npm install
        exec npm run build "$@" ;;
  test) shift; exec "$CARGO_BIN" test "$@" ;;
  *)    exec "$CARGO_BIN" build "$@" ;;
esac
