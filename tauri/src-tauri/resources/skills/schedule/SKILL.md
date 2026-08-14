---
id: schedule
scope: system
name: 日程管理
description: 当用户要求安排会议、创建/查看/修改/删除日程、设置提醒、查询节假日或寻找可安排时间时触发。
priority: 40
tools: [schedule]
triggers: [日程, 日历, 安排, 会议, 提醒, 预约, 计划, 档期, 有空, 假期, 节假日, 农历]
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

# 日程管理

## 职责边界
- 执行用户明确的日程意图：创建/查看/修改/删除日程、检测时间冲突、查询到点提醒、查询农历节假日、查找可安排时间段。
- 所有日程逻辑由 Rust 引擎（core::schedule）计算，数据持久化于知识库 `.mdgo/index_schedule.json`。
- 不擅自删除或修改未获用户确认的日程；创建日程遇到时间冲突时向用户提示并建议备选时间。

## 动作 → 参数映射

| 用户意图 | action | 参数 |
|---|---|---|
| 查看全部日程 | `list` | — |
| 新建日程 | `add` | `title`、`start`、`end`（YYYY-MM-DDTHH:MM）必填；可选 `desc`/`color`/`cron`（5 字段重复表达式）/`notify` |
| 修改日程 | `update` | `id` + 同上字段 |
| 删除日程 | `remove` | `id` |
| 冲突检测 | `conflicts` | `start`、`end` 必填；可选 `ignore_id` |
| 到点提醒 | `remind` | — |
| 农历/节假日 | `lunar` | `date`（YYYY-MM-DD） |
| 找可安排时间 | `next_available` | `duration_minutes` 必填；可选 `start_after`/`skip_rest_days`（默认跳过休息日） |

## 自然语言时间解析
- 把用户口语时间转为结构化参数：如"明天下午 3 点开会 1 小时" → `add` `start=明天15:00` `end=明天16:00` `title=开会`。
- 今天/明天/后天、几点几分、上午/下午/晚上均需换算为 `YYYY-MM-DDTHH:MM` 本地时间。
- 无法确定具体时间时向用户追问，不要臆造时间。

## 注意事项
- 新建日程冲突时：如实告知冲突项，可用 `next_available` 推荐备选时间段（默认避开周末与节假日）。
- 删除/修改前确认目标日程（用 `list` 或 `id`）。
- 失败（时间格式错误、Cron 无效、30 天内无空档）时如实返回原因，不伪造、不重复调用。
- `cron` 为 5 字段（分 时 日 月 周），如 `0 9 * * 1-5` = 工作日 9 点。
