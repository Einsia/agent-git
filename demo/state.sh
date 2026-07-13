#!/usr/bin/env bash
# 在任意 agit 仓库里跑，回答「现在是什么样」。
#
# agit 一共只碰四个地方，全在下面。没有隐藏数据库，没有 ~/.agit。

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

B=$'\033[1m'; DIM=$'\033[2m'; N=$'\033[0m'

echo "${DIM}仓库: $PWD${N}"
echo
echo "${B}1. ctx/ —— 一个 claim 一个文件，文件路径就是 subject${N}"
if [[ -d ctx ]]; then
  n=$(find ctx -name '*.md' 2>/dev/null | wc -l)
  find ctx -name '*.md' 2>/dev/null | sort | sed 's|^ctx/||; s|\.md$||' | sed 's/^/     /'
  echo "${DIM}     共 $n 条，都是被 git 跟踪的普通文件${N}"
else
  echo "     （还没有 ctx/。跑 agit init）"
fi

echo
echo "${B}2. .gitattributes —— 进仓库，跟着 clone 走${N}"
grep -n 'merge=agit' .gitattributes 2>/dev/null | sed 's/^/     /' || echo "     （无）"

echo
echo "${B}3. .git/config —— 不进仓库，clone 之后必须重跑 agit init${N}"
git config --get-regexp '^merge\.agit\.' 2>/dev/null | sed 's/^/     /' || echo "     （无）"

echo
echo "${B}4. .git/hooks/ —— 不进仓库${N}"
found=0
for h in pre-commit pre-push; do
  if [[ -f .git/hooks/$h ]]; then echo "     $h → $(tail -1 .git/hooks/$h)"; found=1; fi
done
[[ $found -eq 0 ]] && echo "     （无）"

echo
echo "${B}工作区状态${N}"
git status --short -- ctx 2>/dev/null | sed 's/^/     /' || true
git status --short -- ctx 2>/dev/null | grep -q . || echo "${DIM}     （ctx/ 干净）${N}"

echo
echo "${DIM}没有别的地方。全部是 git 对象。${N}"
