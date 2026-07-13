# Demo 01 — `agit init` 到底往我的仓库里塞了什么

## 这个 demo 回答什么问题

「agit 把 context 存在哪？它动了我仓库里的哪些东西？哪些会跟着 clone 走？」

## 准备

```sh
./demo/01-init/setup.sh
```

它建一个**还没有 agit** 的普通 git 仓库（一个假的支付服务），然后告诉你：

```sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/01-init
```

下面每一条你自己敲。

---

## 步骤 1 — 先看看现在什么都没有

```console
$ agit-state
```

预期：

```text
1. ctx/ —— 一个 claim 一个文件，文件路径就是 subject
     （还没有 ctx/。跑 agit init）

2. .gitattributes —— 进仓库，跟着 clone 走
     （无）

3. .git/config —— 不进仓库，clone 之后必须重跑 agit init
     （无）

4. .git/hooks/ —— 不进仓库
     （无）
```

`agit-state` 是个小脚本，任何时候在任何 agit 仓库里跑，都告诉你「现在是什么样」。

---

## 步骤 2 — 初始化

```console
$ agit init
```

预期：

```text
  写入 .gitattributes: ctx/** merge=agit
  安装 merge driver: /path/to/agent-git/target/release/agit
  安装 hook: pre-commit
  安装 hook: pre-push

agit 已就绪。
```

> 注意它记的是**二进制的真实路径**，不是你 PATH 里那个符号链接。
> `agit init` 用 `std::env::current_exe()`，它会解析符号链接。
> 好处是把 `bin/agit` 这个链接删掉之后 merge driver 依然能用。

**它一共动了四个地方，一个不多。** 自己数：

```console
$ agit-state
```

```text
1. ctx/                共 0 条
2. .gitattributes      1:ctx/** merge=agit
3. .git/config         merge.agit.name   agit claim merge (evidence-aware)
                       merge.agit.driver <agit 绝对路径> merge-file %O %A %B %P
4. .git/hooks/         pre-commit → exec "<agit 绝对路径>" scan --staged
                       pre-push   → exec "<agit 绝对路径>" scan
```

没有隐藏数据库，没有 `~/.agit`，没有额外的对象存储。**全部是 git 自己的东西。**

---

## 步骤 3 — 哪些进仓库？

```console
$ git status --short
```

预期：只有 `.gitattributes` 和 `ctx/.gitkeep` 是新增的未跟踪文件。

```console
$ cat .gitattributes
```

```text
ctx/** merge=agit
```

这一行告诉 git：**`ctx/` 下面的文件不要按行合并，交给一个叫 `agit` 的驱动。**

```console
$ git config --get-regexp '^merge\.agit\.'
```

```text
merge.agit.name agit claim merge (evidence-aware)
merge.agit.driver <agit 绝对路径> merge-file %O %A %B %P
```

`%O` 是共同祖先，`%A` 是我方（也是输出文件），`%B` 是对方，`%P` 是路径。

```console
$ cat .git/hooks/pre-push
```

```text
#!/bin/sh
# installed by agit
exec "<agit 绝对路径>" scan
```

---

## 步骤 4 — 关键的坑：clone 之后必须重跑 `agit init`

`.gitattributes` 是被 git 跟踪的**普通文件**，会跟着 clone 走。

但 `.git/config` 和 `.git/hooks/` **永远不进仓库**。这是 git 有意的安全设计——
否则任何人 clone 一个仓库，就等于执行仓库作者写的任意命令。

后果很实在：新 clone 的仓库里 `.gitattributes` 说「用 agit 合并 `ctx/**`」，
但 git 找不到这个驱动，于是**退化成按行合并**——你会看到一堆原始 frontmatter 的冲突，
而且没有任何证据裁决。

我们模拟一下。把驱动配置删掉：

```console
$ git config --unset merge.agit.driver
$ agit verify
```

预期：

```text
warning: 本仓库尚未安装 agit 的 merge driver。
         `.gitattributes` 会跟着 clone 走，但驱动配置不会（git 的安全设计）。
         跑一次 `agit init` 修复。
```

**agit 的每条命令启动时都做这个检查。** 修复：

```console
$ agit init
```

---

## 步骤 5 — `init` 是幂等的

再跑一次，`.gitattributes` 里那行不会被写第二遍：

```console
$ agit init
$ grep -c 'merge=agit' .gitattributes
```

预期输出是 `1`。

---

## 收尾

```console
$ git add -A
$ git commit -qm 'agit init'
$ git log --oneline
```

## 存储模型总表

| 位置 | 内容 | 进仓库 | 跟着 clone |
|---|---|:--:|:--:|
| `ctx/<subject>.md` | 一条 claim = 一个文件，**路径即 subject** | ✓ | ✓ |
| `.gitattributes` | `ctx/** merge=agit` | ✓ | ✓ |
| `.git/config` | `merge.agit.driver` | ✗ | ✗ |
| `.git/hooks/pre-commit` | `exec agit scan --staged` | ✗ | ✗ |
| `.git/hooks/pre-push` | `exec agit scan` | ✗ | ✗ |

## 接着看

[Demo 02](../02-claim/) —— 一条 claim 长什么样，为什么「没有出处的结论不入库」。
