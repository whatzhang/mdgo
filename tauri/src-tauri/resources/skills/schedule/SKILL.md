---
id: schedule
scope: system
name: AI 智能日程管理
description: 当用户要求管理日程（创建/查看/修改/删除）、安排会议、设置提醒、查询节假日或空闲时间，以及进行任务规划、自动排期、时间分析、日程复盘、创建专注时间块时触发。
priority: 50
tools: [schedule]
triggers: [日程, 日历, 安排, 会议, 提醒, 预约, 计划, 档期, 有空, 空闲时间, 假期, 节假日, 农历, 排期, 规划, 时间管理, 今日计划, 明日计划, 复盘]
enabled: true
version: 2
created_at: 1754200000000
updated_at: 1760000000000
---

# AI 智能日程管理

## 职责边界
- 执行用户明确的时间管理需求：创建/查看/修改/删除日程、检测时间冲突、查询到点提醒、查询农历节假日、查找可安排时间段。
- 在用户表达目标、任务、计划时提供 AI 时间规划：任务拆解、自动排期、时间分析、日程复盘、专注时间块。
- **必须通过 `schedule` 工具读写日程**；时间计算、冲突检测、Cron 解析、节假日计算、空闲时间搜索均由 Rust 引擎完成，AI 不自行推算时间。
- 不擅自删除、修改、覆盖未获用户确认的日程；创建日程遇到冲突时如实提示冲突项与备选建议，由用户选择。
- 所有的信息要有来源依据，不要进行编造。

## 动作 → 参数映射

| 用户意图 | action | 参数 |
|---|---|---|
| 查看全部日程 | `list` | —（输出含每条的 `id`，供 update/remove 使用） |
| 新建日程 | `add` | `title`、`start`、`end`（YYYY-MM-DDTHH:MM）必填；可选 `desc`/`color`/`cron`（5 字段重复表达式）/`notify`/`notify_before`（提前提醒分钟数）/`event_type`/`priority`/`related_docs`/`related_tasks`/`related_git`/`ai_category`/`ai_energy`/`ai_estimated_hours` |
| 修改日程 | `update` | `id`（取自 `list` 输出）+ 同上字段 |
| 删除日程 | `remove` | `id`（取自 `list` 输出，不要用标题代替 id） |
| 冲突检测 | `conflicts` | `start`、`end` 必填；可选 `ignore_id` |
| 到点提醒 | `remind` | — |
| 农历/节假日 | `lunar` | `date`（YYYY-MM-DD） |
| 找可安排时间 | `next_available` | `duration_minutes` 必填；可选 `start_after`/`skip_rest_days`（默认跳过休息日） |
| 任务规划排期 | `plan` | `deadline`（YYYY-MM-DD）、`tasks`（[{title,hours}]）必填；可选 `work_start`/`work_end`/`skip_rest_days` |
| 时间分析 | `optimize` | 可选 `range`（7d/30d/YYYY-MM-DD..YYYY-MM-DD，默认 7d） |
| 日程复盘 | `review` | 可选 `date`（默认今天） |
| 专注时间块 | `focus` | `duration_minutes` 必填；可选 `task`/`start`（指定 start 校验冲突并创建，未指定则推荐时间段） |
| 今日/明日计划 | `today_plan` | 可选 `date`/`work_start`/`work_end` |

## 运行原则
- **Rust 引擎权威**：时间换算、时区、冲突检测、Cron、节假日、空闲时间搜索全部由 `schedule` 工具完成，AI 不自行计算。
- **AI 规划原则**：目标理解、任务拆解、优先级判断、时间建议由 AI 负责；最终落库必须经 `schedule.add` 写入系统。

## 自然语言时间解析
- 把用户口语时间转为结构化参数：如"明天下午 3 点开会 1 小时" → `add` `start=明天15:00` `end=明天16:00` `title=开会`。
- 今天/明天/后天、几点几分、上午/下午/晚上均需换算为 `YYYY-MM-DDTHH:MM` 本地时间。
- 模糊时间（"最近找时间讨论"）**不要创建日程**：调用 `next_available` 推荐具体时间段，向用户确认后再 `add`。

## 冲突处理规则
- 创建日程发现冲突时：如实告知冲突项，`add` 返回的备选建议（冲突后第一空档 + 次日空档）请呈现给用户选择。
- **禁止**自动覆盖、自动删除、自动移动已有日程；`plan` 只输出建议、不自动创建。

## 智能提醒
- 普通提醒：`notify=true`（默认）；提前提醒用 `notify_before=分钟数`，如"提前 10 分钟提醒" → `notify_before=10`。
- 会议等事件可用 `related_docs`/`related_git` 关联文档与提交，提醒时告知用户已找到的关联材料（关联的查找由 AI 用 `read`/`git_status` 等完成）。

## 数据结构（add/update 可选字段）
```json
{ "event_type": "work", "priority": "high",
  "related_docs": ["project/rag.md"], "related_tasks": ["task001"],
  "ai_category": "development", "ai_energy": "deep_work", "ai_estimated_hours": 4 }
```

## 注意事项
- 删除/修改前确认目标日程（用 `list` 或 `id`）。
- `plan` 排布建议展示给用户确认后再逐个 `add`（type=task）。
- 失败（时间格式错误、Cron 无效、30 天内无空档）时如实返回原因，不伪造、不重复调用。
- `cron` 为 5 字段（分 时 日 月 周），如 `0 9 * * 1-5` = 工作日 9 点。
- plan/optimize/review/focus/today_plan 的完整工作流与输出示例见 [references/planner.md](references/planner.md)。
