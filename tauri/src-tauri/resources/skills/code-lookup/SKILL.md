---
id: code-lookup
scope: system
name: 代码定位
description: 当用户询问某个函数、类、符号、变量在哪里定义、哪里使用，或要求定位代码实现时触发。
priority: 50
roles: ["owner"]
tools: [code_lookup, read]
top_k: 10
min_score: 0.5
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
2. 对命中的关键文件用 read 读取完整上下文
3. 汇总定义位置与关键实现逻辑

## 输出规范
- 给出符号定义文件与行号；简要说明实现逻辑；引用关键代码片段
