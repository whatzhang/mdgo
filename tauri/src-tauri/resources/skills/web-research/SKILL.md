---
id: web-research
scope: system
name: 网页调研
description: 当用户要求调研/研究某个主题、收集网页资料、对比多个来源、深度调查外部信息（非知识库内容）时触发。适合需要联网获取最新信息、跨多来源综合的场景。
priority: 55
tools: [webfetch, deep_research, parallel_research, read_subagent_result]
triggers: [调研, 研究, 网页调研, 网上查, 搜索一下, 最新消息, 新闻, 对比资料, 调查, 情报, 联网搜索, 外部资料, web research, research]
enabled: true
version: 1
created_at: 1760000000000
updated_at: 1760000000000
---

# 网页调研

## 核心思想
- **先规划再抓取**：明确调研目标与子问题，避免无目标地抓取大量网页浪费预算。
- **优先并行**：多个独立子问题用 `parallel_research` 并行调研；单一大主题用 `deep_research` 深度调研；少量定向网页用 `webfetch` 直接抓取。
- **只信抓取结果**：所有事实必须来自 `webfetch`/子代理返回内容，不凭记忆编造数据、日期或来源。

## 工作流
1. **澄清目标**：确认主题、期望的深度（概要/详细）、时间范围（如"近一年"）；用户未指定时合理推断并说明假设。
2. **选择策略**：
   - 主题宽泛/需多角度 → `parallel_research`（2-5 个独立子代理并行）
   - 主题深/需精读 → `deep_research`（单只只读子代理深度调研）
   - 明确 1-3 个具体 URL → `webfetch` 逐个抓取
3. **汇总**：子代理返回有界摘要，需要完整内容时用 `read_subagent_result` 分页读取。
4. **输出**：结论先行，标注来源 URL；信息不足/互相矛盾时如实说明，不强行下结论。

## 约束
- `webfetch` 仅支持 http/https，有 SSRF 防护；响应上限 200KB、文本上限 50000 字符。
- 子代理是隔离只读上下文，不共享当前会话技能激活状态。
- 调研结果若用户要求保存，用 `write` 落盘为笔记（可结合 note-writing 卡片规范）。
- 涉及知识库内部内容时优先用 `kb_search`，本技能只负责外部信息。
