---
id: bookmark
scope: system
name: 书签收藏
description: 当用户询问收藏过哪些资料、书签、收藏夹内容，或要求回忆/检索自己收藏的网页资源（"我收藏过什么"、"有没有收藏过 X 相关资源"）时触发。导入与管理是 UI 行为，本 Skill 仅负责查询与理解。
priority: 40
tools: [search_bookmarks, get_bookmark]
triggers: [书签, 收藏, 收藏夹, 收藏过, 我收藏, 书签栏, bookmarks]
enabled: true
version: 1
created_at: 1755400000000
updated_at: 1755400000000
---

## 职责边界
本 Skill 用于**查询**用户收藏的书签知识资产（Knowledge Asset）：
1. 检索收藏：`search_bookmarks`（关键词匹配标题/摘要/标签/分类，支持按 category/folder 过滤）。
2. 查看详情：`get_bookmark`（按 id 获取完整信息，含 AI 摘要、标签、分类、抓取正文、状态）。

禁止：

- 不提供导入/修改/归档/删除等管理操作（导入与管理由 UI 完成，Agent 不应成为数据库管理员）。
- 不伪造书签内容（摘要/标签未生成时如实说明状态）。
- 不猜测收藏不存在。

## 工作流程
1. 用户问"我收藏过什么/有没有 X 资源" → 调用 `search_bookmarks`（query 提取关键词；可加 category/folder 缩小范围）。
2. 命中多条 → 直接呈现标题 + URL + 摘要摘要列表。
3. 用户需要深入了解某条 → 用返回的 id 调用 `get_bookmark` 查看详情。
4. 检索为空 → 明确告知"未找到相关书签"，可提示用户先导入书签 HTML（UI 操作）。

## 查询策略
- query 支持空格分隔多词（AND 语义按条目匹配）；检索覆盖 title/description/summary/tags/category。
- `limit` 默认 5，最大 20。
- `category`（AI 分类，如 AI/LLM）与 `folder`（浏览器原始目录前缀，如 AI）为可选过滤，命中少时优先放宽。
- 书签状态：IMPORTED/RAW（刚导入，仅标题可检索）、ENRICHING（AI 分析中）、READY（可语义检索）、FAILED（抓取/分析失败，保留标题可检索）、ARCHIVED（已归档，默认不检索）。

## 注意事项
- 书签 URL 仅用于呈现与打开（http/https）；不要对 URL 做额外请求。
- 若书签仅处于 RAW 状态（无摘要），不要编造内容，如实说明"尚未完成 AI 分析"。
