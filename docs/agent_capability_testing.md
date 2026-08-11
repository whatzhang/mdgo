# Agent 能力验证方案(以 G:\ai_测试 知识库为例)

> 归档日期:2026-08 · 配套设计文档:`docs/agent_capability_archive.md`
> 适用范围:取消传播、子代理、优化修复、Planner、Trace 五组功能的手动验收。
> 前置:应用已构建启动;知识库 `G:\ai_测试` 已建立索引(`.mdgo\` 下含 bm25/lance,166 文件,含 11 个 .md 及 .mm/.opml/.py);LLM 已配置。

---

## 0. 准备

1. 打开 `G:\ai_测试` 为知识库目录(索引已存在;若提示 schema 版本不符会自动重建)。
2. 日志文件(dev 默认 DEBUG):`%APPDATA%\com.mdgo\logs\mdgo.log`。
3. 前端 DevTools:Console 过滤关键字 `trace` / `rag:status` / `plan`。
4. 日志级别热切换:Ctrl+Shift+= 输入 `trace`(验证后用 `debug` 切回)。

---

## 1. 取消传播(流式请求真正可取消)

| # | 用例 | 操作 | 预期 |
|---|---|---|---|
| 1 | 生成中取消 | 提问"详细对比 cron 和 crontab 的区别，越详细越好",生成中途点"停止" | <1s 结束;日志 `[agent_query] [4]: 对话取消，立即断开请求 request_id=...`;已生成部分保留 |
| 2 | kb 查询取消 | 同上触发普通对话(`kb_llm_query`) | 日志 `[kb_llm_query] [1]: 对话取消，立即断开请求` |
| 3 | 检索阶段取消 | 提问"总结 mybatis 与 OpenResty 的实现原理",检索中点停止 | 日志 `[agent_query] [2]: 对话取消` |

**判定**:点停止后立即结束(不再等下一个 chunk)→ 取消已到达 HTTP 层。

---

## 2. 子代理(隔离只读深度调研)

| # | 用例 | 操作 | 预期 |
|---|---|---|---|
| 4 | 触发调研 | 提示:"请先用 deep_research 工具对整个知识库做深度调研，总结这些文档覆盖的主题，需要时用 read_subagent_result 读全量" | 工具轨迹卡片出现 `deep_research`;日志 `[subagent] 开始调研 request_id=sub-...` → `调研完成`;返回**有界摘要 + subagent_id**;模型随后多次调用 `read_subagent_result`(offset 递增) |
| 5 | 调研中取消 | 调研进行中(日志出现 `sub-*` 后)点"停止" | 立即中止;日志 `[subagent] 调研被父链取消 request_id=sub-...` |
| 6 | 只读安全 | 调研任务里加"顺便修改 mybatis.md 的内容" | 子代理**无 edit/delete 工具**,只告知无法修改或忽略(不产生写操作) |

---

## 3. 优化修复(回归为主,内部机制由单测覆盖)

- **回归**:正常对话/检索/审批不退化(用例 1-4、7-16 已覆盖主要链路)。
- **LLM 300s 超时(可选)**:将 LLM endpoint 指向不响应的端口,提问 → 约 300s 后请求被掐断并报错;恢复配置后再试。
- **LRU / 容量治理 / max_chars clamp / lld-link**:内部机制,由 `cargo test --lib`(31/31)与构建通过覆盖,无需 UI 验证。

---

## 4. Planner(规则路由 + 用户确认)

| # | 用例 | 操作 | 预期 |
|---|---|---|---|
| 7 | 复杂任务触发规划 | 提问(长句+任务动词,命中 `should_plan`):"分析 GRAI复盘法 和 STAR法则 的异同，并结合 mybatis.md 的内容，给出一个分步学习计划，并说明各步骤的验收标准" | 状态"正在规划任务..." → **计划卡片弹出**(目标/步骤/验收)→ 点"批准" → 继续检索+生成,回答遵循计划结构;日志 `任务已规划，等待用户确认` → `用户已批准计划` |
| 8 | 简单问题不触发 | "cron 是什么?" | 直接生成,无 planning 状态、无卡片 |
| 9 | 拒绝 | 触发规划 → 点"拒绝" | 中止;**不新增聊天消息**(空 content 防污染,前端 `if(fullContent)` 跳过落库);日志 `计划未获批准` |
| 10 | 超时 fail-closed | 触发规划 → 不动 60s | 按拒绝处理,中止 |
| 11 | 规划中取消 | 计划卡片弹出时点"停止" | 立即中止;日志 `等待计划确认时被取消` |

---

## 5. Trace(可观测全链路)

| # | 用例 | 操作 | 预期 |
|---|---|---|---|
| 12 | trace 面板 | 发起一次 agent_query(用例 1 或 7) | 回答卡片下方出现"⚙ 阶段耗时(trace)"可折叠面板:planning/expanding/searching/aggregating/generating + 状态图标 + 耗时 |
| 13 | 原始事件 | DevTools Console filter `trace` | `trace:event` payload 含 `seq/stage/status/duration_ms/detail`,request_id 与请求一致 |
| 14 | 日志 request_id 贯穿 | 查日志文件同一次请求 | `[agent_query] [0]`~`[4]` 同一 request_id;`[llm_trace]` 也带 request_id;`skill_exec_metrics` 表 request_id 列**非空**(此前硬编码空串) |
| 15 | rig span 捕获 | Ctrl+Shift+= 输入 `trace` → 提问 | 日志出现 rig 内部 span(此前 100% 丢失),验证 tracing 桥接;测完切回 `debug` |
| 16 | 取消/拒绝 trace | 用例 9/11 | 面板对应阶段显示 `denied` / `cancelled` + 耗时 |

---

## 6. 快速验收清单(按序执行,全绿即通过)

```
[ ] 打开 G:\ai_测试(索引已就绪)
[ ] 用例 12: trace 面板出现(基础链路 OK)
[ ] 用例 7:  复杂任务弹计划卡片 → 批准 → 回答遵循计划
[ ] 用例 9:  拒绝后无新消息(会话不污染)
[ ] 用例 11: 计划卡片时点停止立即中止
[ ] 用例 1:  生成中点停止 <1s 结束,部分内容保留
[ ] 用例 4:  deep_research 摘要 + subagent_id + 分页读取
[ ] 用例 5:  调研中停止立即中止
[ ] 用例 14: 日志 request_id 贯穿 + skill_exec_metrics 非空
[ ] 用例 15: set_log_level trace 后看到 rig span
```

---

## 7. 附:内部机制对应的单元测试(`cargo test --lib`,31/31 通过)

| 模块 | 覆盖 |
|---|---|
| `core/agent/planner.rs` | `should_plan` 边界、`parse_plan` 围栏/无效输入、`to_preamble_text` |
| `core/subagent/mod.rs` | 只读白名单排除、LRU 淘汰/刷新、截断边界 |
| `core/context/mod.rs` | 滑窗预算、摘要+滑窗(摘要恒保留)、摘要失败降级 |
| `core/approval/mod.rs` | 门控放行/拒绝/缓存/无策略 |
| `core/agent/tools/mod.rs` | grep glob 匹配 |