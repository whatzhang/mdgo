# mdgo Skill Engine 1.0 交付归档（一次性注入 + warm 态 + Policy + Session 恢复 + 回退开关）

> 归档日期：2026-08 · 适用范围：`tauri/src-tauri`（Rust 后端）
> 用途：为后续 Agent 相关操作提供设计依据、实现定位与已知取舍。
> 状态：已交付并通过 `cargo test --lib`（86/86 通过，0 警告）+ 独立 Code Review（主阻塞项已修复）。
> 关联：`docs/agent_capability_archive.md`（上一批交付）、`docs/agent_gap_plan.md`（能力规划）。

---

## 0. 背景与目标

mdgo 原有 Skill 体系：激活（`activate_skill`）后，技能正文经 `SkillInstructionHook` **每轮注入** preamble（`instructions()` 每轮拼接）——多轮 Agent 中技能正文重复消耗 token（10 轮 × 全量正文）。

对标 Reasonix / Pi Coding Agent 的 progressive disclosure（描述常驻、正文按需一次性加载），完成两项核心决策：

| 决策 | 结论 |
|---|---|
| P15 技能指令体积治理 | **一次性注入**：正文经 `activate_skill` 工具结果 / 请求入口 history 首条进入上下文一次，后续轮次不重复 |
| P18 多技能参数合并 | **保守合并**（top_k/max_docs/max_chunks 取 min、min_score 取 max），不改行为；根因由 description 排他触发治理 |

并补三块生产级能力：P3 warm 态（会话挂载）、P4 Policy 收拢、P5 Session 技能跨请求恢复，以及三拆后的**回退开关**。

---

## 1. 最终架构

```
             Skill Registry（定义层 SkillDefinition：SKILL.md，system/global/project）
                               │
                               ▼
                     Skill Catalog（L1：id+description 常驻，含挂载标注）
                               │
      ┌───────────────┬────────┴──────────┬────────────────┐
      ▼               ▼                   ▼                ▼
 /技能名[显式]     会话挂载(warm)      LLM activate_skill  回退模式(PERSISTENT_INJECTION)
      │               │                   │                │
      └───────────────┴─────────┬─────────┴────────────────┘
                                ▼
                    Skill Activation（SkillActivation 状态机）
                                │
         ┌──────────────────────┴──────────────────────┐
         ▼                                             ▼
  Context Injector（一次性注入 history）        Policy Resolver（技能>全局>clamp）
         │                                             │
         └───────────────────┬─────────────────────────┘
                             ▼
                 Runtime Skill Context（activations + policy + tools + budget）
                             ▼
                       Agent Executor（Hook：L1 + 预算预警 + active_tools 窄化）
```

## 2. 数据模型（三拆）

```rust
// core/skill/activation.rs
enum SkillLifetime { Turn, Session }          // 激活生命周期层（与存储层 SkillScope 正交）
enum ActivationStatus { Candidate, Active, Failed, Expired }  // 顺序执行下无中间态

struct SkillActivation {                       // 运行时激活记录，不持有正文
    skill_id, scope, version,
    lifetime: SkillLifetime,
    status: ActivationStatus,
    mode: ActivationSource,                    // Manual=Explicit / Attached=Mounted / Llm=Auto
    loaded_once: bool,                         // 幂等依据：正文是否已注入
    tools: Vec<String>,                        // allowed_tools 聚合 + 工具轨迹溯源
    activated_at, summary,
}

// core/skill/policy.rs
struct RuntimePolicy { top_k, min_score, rerank_min_score, max_docs, max_chunks_per_doc }

// core/context/mod.rs
struct SessionSkillRef { skill_id, scope, version }   // P5 跨请求恢复引用
struct CompactionState { summary, cutoff_msg_id, tokens_before, session_skills: Vec<SessionSkillRef> }
```

预算常量：`MAX_SKILL_BODY_CHARS=4000`（单技能）、`MAX_SKILL_INJECTION_CHARS=8000`（入口总量）、`PERSISTENT_INJECTION=false`（回退开关，`core/agent/limits.rs`）。

## 3. 入口语义与生命周期

| 入口 | 生命周期 | 正文注入 | 工具解锁 | 预检索 |
|---|---|---|---|---|
| `/技能名` | Turn（请求结束失效） | history 首条 system 消息一次 | ✅（loaded_once=true） | ✅ |
| 会话挂载 | Session，默认 **warm**（Candidate） | 不注入（LLM 激活才注入） | ❌（Candidate 不解锁） | ✅（warm 声明检索 → 预检索开启） |
| LLM `activate_skill` | 动态激活为 Turn；**激活挂载中的技能保留 Session** | 工具结果返回正文核心段（XML 标识） | ✅ | ✅ |
| P5 Session 激活 | Session | **由挂载 mode 持久化驱动**（active 挂载每请求激活+注入；warm 不自动恢复） | ✅（active 挂载） | ✅ |

## 4. 注入链路（一次性 vs 回退）

```
一次性模式（PERSISTENT_INJECTION=false，默认）：
  预激活（/技能名、active 挂载） → history 首条 system（format_skill_instructions 预算截断）
  LLM 激活         → activate_skill 工具结果（<active_skill id=...> 正文，幂等）
  后续轮次         → 不重复注入（Hook 仅 L1 + 预算预警 + active_tools）

回退模式（PERSISTENT_INJECTION=true）：
  llm.rs 跳过 history 注入（避免双份）
  SkillInstructionHook.persistent_instructions()：每轮从 SkillRegistry 按 active_only()
  记录重新查询正文拼接注入（总量受预算截断）
```

**幂等**：`activate_skill` 检查 `state.is_loaded(id)` → `already_active`，不重复返回正文。

## 5. 各 Phase 实现定位

| 文件 | 职责 |
|---|---|
| `core/skill/activation.rs` | SkillLifetime/ActivationStatus/SkillActivation；ActiveSkillState（activate/activate_warm/active_only/allowed_tools/retrieval_enabled/计时/幂等） |
| `core/skill/context.rs` | resolve_preactivated（手动 Turn / 挂载 warm）；format_skill_instructions（priority 排序 + 预算截断 + 截断技能名提示） |
| `core/skill/policy.rs` | resolve_retrieval_policy：技能声明优先 → 请求级 top_k / 全局兜底 → Security clamp |
| `core/skill/metrics.rs` | 不变（dispatch/execution 指标，source 复用 ActivationSource） |
| `core/agent/mod.rs` | SkillInstructionHook（一次性不注入 + 回退 persistent_instructions + active_tools 窄化 + 预算预警）；PERSISTENT_INJECTION 引用 |
| `core/agent/tools/mod.rs` | activate_skill（幂等 + XML 正文 + 截断 + read 兜底提示 + Session lifetime 保留）；read L3 / 工具溯源改用 active_only |
| `core/agent/limits.rs` | PERSISTENT_INJECTION（集中配置） |
| `core/context/mod.rs` | CompactionState.session_skills + SessionSkillRef（serde 向后兼容） |
| `commands/llm.rs` | 预激活（/技能名、active 挂载）history 注入（回退模式跳过）；检查点写回 session_skills（观测/未来扩展）；P4 policy 调用；catalog 挂载标注（自动准备/立即生效） |

## 6. Code Review 发现与修复（反驳型审查，verdict=block → 已修复）

独立 review 子代理 + 链路自查共发现 9 项，主阻塞 1 项、should-fix 2 项，已全部处置：

| # | 严重度 | 问题 | 修复 |
|---|---|---|---|
| 1 | 🔴 阻塞 | **P5 恢复注入不写 ActiveSkillState**：恢复正文进了 history，但技能仍 warm（Candidate）→ 工具不解锁、read L3 不可读、LLM 再 activate_skill 重复注入正文 | 首轮以恢复注入时 `active_skills.activate(...)` 同步升级修复；**MountPreference 落地后移除恢复注入**——挂载 mode 已是跨请求激活的持久化声明，active 由 resolve_preactivated 每请求激活注入，warm/已移除技能不应被自动恢复推翻 |
| 2 | 🟡 should-fix | 恢复注入与预激活注入顺序：恢复指令排在预激活之前（当前显式意图应优先） | 调换顺序；恢复注入移除后不再适用 |
| 3 | 🟡 should-fix | 回退模式与 history 注入并存 → 正文双份 | 回退模式下 llm.rs 跳过 history 注入（两条路径互斥） |
| 4 | 🟢 | format_skill_instructions 首个技能即超预算时只输出提示、无正文且不指明技能 | 截断提示携带被截断技能名 |
| 5 | 🟢 | activate_skill 幂等不看 version（请求内技能更新后重复激活不返回新正文） | 注释说明（请求内技能更新罕见，接受） |
| 6 | 🟢 | session_skills 仅压缩时写回（未压缩保留旧引用） | 注释说明（版本校验兜底） |
| 7-9 | 🟢 | 外部并发改动兼容（limits.rs 常量集中化、DEFAULT_MAX_TURNS=10、BASE_TOOLS 扩展） | 核对无冲突 |

## 7. 决策记录

- **Q1**：检索参数技能显式声明优先、请求级/全局兜底、Security 仅 clamp——保持现状，不引入 System 覆盖技能（否则 frontmatter 参数失效）。
- **P18**：保守合并保留（top_k min / min_score max）；否决 priority merge（"帮我找文件"会被 kb-summary 规则接管）。
- **拒绝 Dedicated Context Block**：与一次性注入矛盾、偏离 Rig 架构；warm + 短正文已化解"污染"担忧。
- **拒绝 Warming/Activating 中间态**：rig 工具顺序执行，无并发竞争。
- **激活决策仍归 LLM**：Router 降级为 Candidate Layer（catalog 标注 + 挂载标注），不做确定性规则匹配。

## 8. 验证

- `cargo test --lib`：**86 passed / 0 failed / 0 warnings**
  - activation：状态机、幂等、allowed_tools 并集、warm 不解锁工具但开预检索、deactivate 计时、空声明
  - context：指令拼接排序/截断/空输入、截断提示含技能名
  - policy：技能优先、请求/全局兜底、Security clamp
  - context/compaction：session_skills roundtrip + 旧数据兼容
- 最终改动面：`commands/llm.rs`、`core/agent/{limits,mod,tools/mod}.rs`、`core/context/mod.rs`、`core/skill.rs`、`core/skill/{activation,context,policy}.rs`、7 个内置 SKILL.md、新增 `core/skill/policy.rs`、`pomodoro/references/pomodoro.md`。

## 9. 已知遗留（后续可做）

- ~~`MountPreference` 用户配置层~~ **已落地（2026-08 同批）**：`chat_session_skills` 增 `mount_mode`（warm=自动准备/active=立即生效，开发阶段直接改表不迁移）；`skill_attach(mode)`/`skill_set_mount_mode`/`AttachedSkill`；resolve_preactivated 按 mode 分派（warm→activate_warm、active→激活+skills 注入）；catalog 产品化标注（自动准备/立即生效）；前端三态产品化 UI（⚪关闭/🟢自动准备/⚡立即生效 + 状态菜单 + title 说明，状态选择器替代点击循环）。**体验优化**：挂载不再要求先建会话——无会话时点击挂载/切模式自动 `chat_session_create`（RAG 模式建 rag 类型）并设为当前会话，用户无感。`/技能名 --session` 明确不做（输入框选择器已覆盖挂载入口）。
- Skill Chunk Loading（SKILL.md section 拆分 + read_skill_section）：当前正文短，显式拆块留未来 100+ 技能场景。
- 一次性注入的实际 token 收益：建议真实对话对比验证（对比 `PERSISTENT_INJECTION` 两态）。
- 用户级技能屏蔽策略（未来）：若需"本项目永远禁用某技能"，应新增 `chat_session_skill_preferences` 策略表，不污染挂载表。
