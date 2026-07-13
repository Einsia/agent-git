#!/usr/bin/env bash
# 新模型 demo 的舞台:Alice / Bob 两个开发者的代码仓库,各带一条(伪造的)Claude 会话,
# 外加一个团队远端。不替你跑 sync/push/reconcile —— 那些是你要演的。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

ALICE="$DEMO_HOME/showcase-alice"
BOB="$DEMO_HOME/showcase-bob"
ORIGIN="$DEMO_HOME/showcase-origin.git"

_ensure_agit
B=$'\033[1m'; G=$'\033[32m'; DIM=$'\033[2m'; N=$'\033[0m'

# 造一条 Claude 风格的会话 dump:$1=项目路径 $2=会话名 $3=用户话 $4=agent结论
fake_session() {
  local slug; slug=$(echo "$1" | sed 's|/|-|g')
  local c="$HOME/.claude/projects/$slug"
  mkdir -p "$c/$2/tool-results"
  cat > "$c/$2.jsonl" <<EOF
{"type":"mode","sessionId":"$2"}
{"type":"user","cwd":"$1","gitBranch":"main","uuid":"u1","message":{"role":"user","content":"$3"}}
{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"$4"}]}}
EOF
  echo "工具输出示例" > "$c/$2/tool-results/toolu_x.txt"
}

seed_code() {
  rm -rf "$1"; mkdir -p "$1"; cd "$1"
  git init -q -b main .; git config user.name "$2"; git config user.email "$2@x"; git config commit.gpgsign false
  printf 'export interface User {\n  id: number;\n  user_id: string;\n}\n' > models.ts
  git add -A && git commit -qm '初始代码'
}

rm -rf "$ORIGIN"; git init -q --bare -b main "$ORIGIN"

seed_code "$ALICE" alice
fake_session "$ALICE" alice-sess "用户标识字段用什么？" "查清了:全库统一用 user_id 这个字段名,别的地方也都是它。"

seed_code "$BOB" bob
fake_session "$BOB" bob-sess "用户标识字段用什么？" "我决定改用 uid,更简洁。"

cat <<EOF

${B}舞台就绪。${N}

  ${G}export PATH="$BIN_DIR:\$PATH"${N}

  团队远端    ${G}$ORIGIN${N}
  Alice 仓库  ${G}$ALICE${N}   （有一条会话:主张 user_id）
  Bob 仓库    ${G}$BOB${N}   （有一条会话:主张 uid —— 和 Alice 冲突）

${DIM}照着讲稿一幕一幕敲:${N}
  ${G}\$EDITOR demo/showcase/讲稿.md${N}

${DIM}先进 Alice 的仓库:${N}
  ${G}cd $ALICE${N}
EOF
