---
name: 缺陷
about: 记录可复现的缺陷，必须带证据路径与修复验收门
title: "[Bug] "
labels: ["bug"]
assignees: ''
---

## 现象

<!-- 一句话说清“什么坏了 + 影响谁”，不写猜测的根因。 -->

## 复现步骤

1.
2.
3.

<!-- 尽量给最小命令，例如：
     bash scripts/ci/run.sh cargo xtask test --layer l2
     cargo xtask e2e --case E-04 -->

## 期望 vs 实际

| | 内容 |
|---|---|
| 期望 | |
| 实际 | |

<!-- 有报错就贴原文（含退出码），不要改写措辞。 -->

## 环境

| 项 | 值 |
|---|---|
| 宿主 | Windows（无 cargo/rustup，无 make） |
| Rust 侧 | 容器 `aya-dev`，`/workspace/probe` |
| 控制面 | `control-center/`，JDK 21（宿主只有 17） |
| 其他 | 分支 / commit： |

## 影响面

<!-- 哪个契约/层受影响：L0–L3、CSV 列序、ABI、proto、部署；是否阻塞 demo 或 gate。 -->

## 是否回归

<!-- 是 / 否 / 未知。是：指出引入的 PR 号或 commit；未知：写出 bisect 计划。 -->

## 证据

<!-- 日志、CSV、JUnit、截图路径。仓库内路径写相对路径；产物在 `verification/` 下（已 gitignore）；外层只剩 `out/`。 -->

- 日志：
- 产物：

## 修复验收门

<!-- 修完后用什么命令证明修好；格式同 task.md，命令 + 期望输出。 -->

```bash
# 期望：
```

- [ ] 回归用例已加到对应层（L0–L3）
- [ ] `CHANGELOG.md` 的 `Fixed` 已加一条
