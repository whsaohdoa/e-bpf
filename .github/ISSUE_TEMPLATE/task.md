---
name: 任务
about: 进入 W 计划的功能或工程任务，必须带可执行的验收门
title: "[Task] "
labels: ["task"]
assignees: ''
---

## 目标

<!-- 一句话结果，≤2 行。不写过程、不写“研究一下”。 -->

## 验收门

<!-- 命令 + 期望输出，格式同 control-center/docs/plan-control-plane-impl.md:91。
     至少 1 条，能复制粘贴执行；允许写“勾选式”验收（如 checkbox），但必须可判定。 -->

```bash
# 例：cargo xtask test --layer l2
# 期望：test result: ok. N passed; 0 failed
```

- [ ] 验收门 1：
- [ ] 验收门 2：

## 依赖

<!-- 前置 issue / 分支 / 环境（aya-dev 容器、JDK 21、k3d）。无则写“无”。 -->

## 规模

<!-- S ≤0.5 人日 / M ≤2 人日 / L >2 人日（L 必须拆成多个 issue 或明确拆分子任务）。 -->

- 规模：M（约 1 人日）

## 关联计划条目

<!-- 仓库内文件:行，必须能点开定位。 -->

- `docs/Train0-Plan-6x1.md:行号`

## 关联 PR

<!-- 开 PR 后回填，例如 #12；未开时写“待开”。 -->
