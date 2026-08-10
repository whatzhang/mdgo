---
id: kb-search
scope: system
name: 知识库检索
description: 当用户询问知识库、文档、笔记中的具体内容，或要求搜索、检索、查询资料时触发。
priority: 50
tools: [kb_search, code_lookup, read]
top_k: 8
min_score: 0.4
max_docs: 5
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

当用户询问知识库、文档或笔记中的具体内容时，执行以下流程：

1. 先用 `kb_search` 在知识库中检索与问题相关的文档片段；可从不同角度多次调用，直到信息充足。
2. 问题涉及具体代码符号（函数/类/方法名）时，优先调用 `code_lookup` 定位代码定义。
3. 需要完整内容或上下文补充时，用 `read` 读取具体文件（限知识库目录内）。
4. 回答时标注引用来源 `filename.md`。若检索结果不足以回答，如实告知用户，不要编造。
