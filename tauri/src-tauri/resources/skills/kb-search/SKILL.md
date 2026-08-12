---
id: kb-search
scope: system
name: 知识库检索
description: 当用户询问知识库（当前打开目录）中具体文档、笔记、资料的内容或事实，或明确要求搜索、检索、查询时触发。总结综述类需求请用 kb-summary；纯代码符号定位请用 code-lookup。
priority: 70
tools: [kb_search, code_lookup, read]
top_k: 8
min_score: 0.4
max_docs: 5
max_chunks_per_doc: 3
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

当用户询问知识库中的具体内容时，执行以下流程：

0. 检索范围 = 当前打开的项目目录（含文档与代码，`docs/` 是文档目录，全部已索引文件均可命中）。
1. 先用 `kb_search` 在知识库中检索与问题相关的文档片段；可从不同角度多次调用，直到信息充足（注意模型调用预算，信息够就收敛，不要无谓多轮检索）。
2. 问题涉及具体代码符号（函数/类/方法名）时，优先调用 `code_lookup` 定位代码定义。
3. 需要完整内容或上下文补充时，用 `read` 读取具体文件（限知识库目录内）。
4. 回答时标注引用来源（格式如 `[来源] docs/xxx.md`，与前端引用列表一致）。若检索结果不足以回答，如实告知用户，不要编造。
