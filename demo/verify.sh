#!/usr/bin/env bash
# 把每个 demo 的 README 里 ```console 块中以 `$ ` 开头的命令抠出来，真的跑一遍，
# 然后断言 expect.txt 里列的每条模式都出现在输出里。
#
# 目的不是「演示」，是**防止 README 腐烂**：README 写的命令必须真的能跑，
# 承诺的输出必须真的出现。CI 跑这个。
#
#   ./demo/verify.sh            全部
#   ./demo/verify.sh 04-merge   单个

set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export DEMO_HOME="${DEMO_HOME:-/tmp/agit-demo}"
BIN_DIR="$DEMO_HOME/bin"

G=$'\033[32m'; R=$'\033[31m'; DIM=$'\033[2m'; N=$'\033[0m'

# 从 README 里抽出 ```console 块内以 "$ " 开头的行
extract_cmds() {
  awk '
    /^```console$/ { inblk=1; next }
    /^```/         { inblk=0; next }
    inblk && /^\$ / { print substr($0, 3) }
  ' "$1"
}

run_one() {
  local dir="$1" name; name="$(basename "$dir")"
  local log="/tmp/agit-verify-$name.log"

  [[ -x "$dir/setup.sh" ]] || { echo "  ${R}✗${N} $name: 没有 setup.sh"; return 1; }
  "$dir/setup.sh" >"$log" 2>&1 || { echo "  ${R}✗${N} $name: setup 失败 ($log)"; return 1; }

  local repo; repo="$(grep -oE 'cd /tmp/agit-demo/[A-Za-z0-9._-]+' "$log" | tail -1 | awk '{print $2}')"
  [[ -d "$repo" ]] || { echo "  ${R}✗${N} $name: setup 没告诉我仓库在哪"; return 1; }

  local cmds; cmds="$(extract_cmds "$dir/README.md")"
  [[ -n "$cmds" ]] || { echo "  ${R}✗${N} $name: README 里没有可执行的 console 块"; return 1; }

  # 逐条跑。README 里的命令允许失败（很多就是要演示失败），我们只看输出。
  (
    export PATH="$BIN_DIR:$PATH"
    cd "$repo" || exit 1
    while IFS= read -r c; do
      echo "### \$ $c"
      eval "$c" 2>&1
    done <<<"$cmds"
  ) >>"$log" 2>&1

  local fail=0 n=0
  while IFS= read -r pat; do
    [[ -z "$pat" || "$pat" == \#* ]] && continue
    n=$((n+1))
    grep -qE -- "$pat" "$log" || { echo "  ${R}✗${N} $name: README 承诺但没出现 → ${pat}"; fail=1; }
  done < "$dir/expect.txt"

  if [[ $fail -eq 0 ]]; then
    echo "  ${G}✓${N} $name  $(wc -l <<<"$cmds") 条命令，$n 条断言   ${DIM}$log${N}"
  fi
  return $fail
}

targets=()
if [[ $# -gt 0 ]]; then targets=("$HERE/$1"); else targets=("$HERE"/0*/); fi

rc=0
for d in "${targets[@]}"; do run_one "${d%/}" || rc=1; done
echo
[[ $rc -eq 0 ]] && echo "全部 demo 的 README 都是真的。" || echo "有 README 和实现对不上。"
exit $rc
