---
id: pomodoro
scope: system
name: 番茄钟
description: 当用户要求开始、停止或查询番茄钟（专注计时器）时触发。
priority: 40
tools: [pomodoro]
triggers: [番茄钟, 番茄, 专注, 休息, pomodoro, 计时]
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

## 职责边界
- 仅执行用户明确的番茄钟计时意图（开始 / 停止 / 查询 / 自动衔接设置），不要自行开始计时。
- 系统同时只存在一个番茄钟任务：开始新任务时会自动关闭旧任务（含暂停未完成、自动衔接中的），无需额外处理。

## 动作 → 参数映射

| 用户意图 | action | 参数 |
|---|---|---|
| 开始专注 | `start` | `mode=focus`；用户指定时长时传 `minutes`，未指定默认 25 |
| 开始休息 | `start` | `mode=break`；用户指定时长时传 `minutes`，未指定默认 5 |
| 开启/关闭自动休息 | `autoBreak` | `openEnable=true/false` |
| 开启/关闭自动专注 | `autoFocus` | `openEnable=true/false` |
| 停止 | `stop` | — |
| 查询状态 | `status` | — |

## 注意事项
- 同时设置"开始 + 自动衔接"时，`autoBreak` / `autoFocus` 设置要在 `start` 之前调用。
- 动作完成后，向用户简要说明当前番茄钟的状态。
- 模式时长与自动衔接的详细规则见 [references/pomodoro.md](references/pomodoro.md)。
