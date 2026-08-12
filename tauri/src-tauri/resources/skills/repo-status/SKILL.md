---
id: repo-status
scope: system
name: git状态汇报
description: 当用户询问工作区状态、改动、变更、未提交、diff、最近的提交记录时触发。
priority: 45
tools: []
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

# git仓库状态汇报

## 适用场景
- 用户需要了解当前工作区的改动、文件变更与 Git 状态

## 执行步骤
1. 调用 git_status 获取工作区状态矩阵
2. 必要时调用 ls 列出相关目录结构辅助说明

## 输出规范
- 按"未提交改动 / 新增文件 / 已暂存"分组列出；对每个文件说明改动类型（新增/修改/删除）
- 统计变更文件总数；无改动时明确说明"工作区干净"
