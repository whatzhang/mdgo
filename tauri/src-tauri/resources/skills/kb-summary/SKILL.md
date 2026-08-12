---
id: kb-summary
scope: system
name: 知识库综述
description: 当用户要求对知识库中的某个主题或一批文档进行总结、综述、概览、归纳、提炼时触发。单纯查询具体内容请用 kb-search；纯代码符号定位请用 code-lookup。
priority: 65
tools: [kb_search, read]
top_k: 12
min_score: 0.5
max_docs: 6
max_chunks_per_doc: 3
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

# 知识库综述

## 适用场景
- 用户需要对某主题在知识库中的内容做整体性总结
- 检索范围 = 当前打开的项目目录（含文档与代码，`docs/` 是文档目录）

## 执行步骤
1. 先用 kb_search 从多个角度检索（可检索 2~3 轮，每次聚焦单一角度；注意模型调用预算，信息够就收敛）
2. 对高相关文档用 read 精读关键章节
3. 按主题归纳，而非按文档罗列

## 输出规范
- 用 ## 分级标题组织；每个结论标注来源文档名（格式如 `[来源] docs/xxx.md`）
- 信息不足时明确说明，不要编造
