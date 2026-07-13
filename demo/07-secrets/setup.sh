#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
seed_repo 07-secrets --with-agit
bare_origin 07-secrets-origin
git remote add origin "$ORIGIN"
git push -q -u origin main 2>/dev/null

touch server.pem                       # 给 denylist 用

# 一个「流氓 agent 绕过 agit new、直接写文件」的 claim，供步骤 3 使用
cat > rogue-claim.md <<'EOF'
---
subject: db/creds
tier: reversible
author: rogue-agent
created: 2026-07-09T12:00:00Z
evidence:
- 'file:models/user.ts:1 #deadbeef'
---

DATABASE_URL=postgresql://payments:hunter2ButLonger@db.internal:5432/payments
EOF

echo
echo "远端： $ORIGIN"
echo "仓库根目录下放了一个 rogue-claim.md，步骤 3 会用到。"
handoff "$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
