---
id: code-lookup
scope: system
name: 代码定位
description: 当用户询问某个函数、类、符号、变量在哪里定义、哪里使用，或要求定位代码实现时触发。
priority: 50
trigger_rules:
  type: hybrid
  keywords: ["在哪", "定义", "实现", "函数", "类", "符号", "代码", "查找", "定位", "function", "class"]
  similarity_threshold: 0.5
mutex: []
token_budget: 2500
input_schema:
  - { name: "symbol", type: "string", required: true, description: "要定位的符号名" }
output_format: markdown
roles: ["owner"]
timeout_ms: 45000
tools: [code_lookup, read_file]
top_k: 10
min_score: 0.3
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

# 代码定位

## 适用场景
- 用户需要定位符号（函数/类/变量）的定义位置与调用位置

## 执行步骤
1. 先用 code_lookup 检索符号相关的代码片段
2. 对命中的关键文件用 read_file 读取完整上下文
3. 汇总定义位置与关键实现逻辑

## 输出规范
- 给出符号定义文件与行号；简要说明实现逻辑；引用关键代码片段
