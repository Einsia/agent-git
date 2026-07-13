#!/usr/bin/env bash
# demo 公共库。只做一件事：把仓库搭到起始状态，然后闪开。
#
# 这里没有任何「演示」逻辑。命令由你自己敲 —— 敲什么写在每个 demo 的 README 里。

set -uo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
SEED="$DEMO_DIR/seed"
DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
BIN_DIR="$DEMO_HOME/bin"

B=$'\033[1m'; DIM=$'\033[2m'; G=$'\033[32m'; Y=$'\033[33m'; N=$'\033[0m'

# 找到（必要时编译）agit，并在 $DEMO_HOME/bin 下放一个干净的软链。
_ensure_agit() {
  local agit="$ROOT/target/release/agit"
  [[ -x "$agit" ]] || agit="$ROOT/target/debug/agit"
  if [[ ! -x "$agit" ]]; then
    echo "没找到 agit 二进制，先编译……" >&2
    "$ROOT/build.sh" --release >&2 || exit 1
    agit="$ROOT/target/release/agit"
  fi
  mkdir -p "$BIN_DIR"
  ln -sf "$agit" "$BIN_DIR/agit"
  ln -sf "$DEMO_DIR/state.sh" "$BIN_DIR/agit-state"
  AGIT="$BIN_DIR/agit"
}

gitcfg() { git config user.name "$1"; git config user.email "$1@payments.io"; git config commit.gpgsign false; }

# seed_repo <名字> [--with-agit]
# 建一个干净的假支付服务仓库。--with-agit 会替你跑 agit init 并提交。
seed_repo() {
  _ensure_agit
  local name="$1"; shift
  REPO="$DEMO_HOME/$name"
  rm -rf "$REPO"; mkdir -p "$REPO"; cd "$REPO"
  git init -q -b main .
  gitcfg alice
  cp -r "$SEED/." "$REPO/"
  mv env.seed .env
  printf '.env\nnode_modules/\n' > .gitignore
  git add -A && git commit -qm "支付服务：初始代码"
  if [[ "${1:-}" == "--with-agit" ]]; then
    "$AGIT" init >/dev/null
    git add -A && git commit -qm "agit init" >/dev/null
  fi
}

# 建一个裸远端
bare_origin() {
  ORIGIN="$DEMO_HOME/$1.git"
  rm -rf "$ORIGIN"
  git init -q --bare -b main "$ORIGIN"
}

# 打印「接下来你自己做什么」
handoff() {
  echo
  echo "${B}仓库准备好了。${N}"
  echo
  echo "${DIM}把 agit 加到 PATH，进仓库：${N}"
  echo "${G}  export PATH=\"$BIN_DIR:\$PATH\"${N}"
  echo "${G}  cd $REPO${N}"
  echo
  echo "${DIM}然后照着 README 一条一条敲：${N}"
  echo "${G}  \$EDITOR $1/README.md${N}"
  echo
  echo "${DIM}任何时候想知道「现在是什么样」：${N}"
  echo "${G}  $DEMO_DIR/state.sh${N}"
  echo
}
