---
id: repo-status
scope: system
name: git状态汇报
description: 当用户询问工作区状态、改动、变更、未提交、diff、最近的提交记录时触发。
priority: 45
tools: [git_status, git_diff, git_log, git_show, git_refs]
triggers: [git状态, 未提交, 未暂存, 已暂存, 提交记录, git log, 版本历史, 仓库状态]
enabled: true
version: 3
created_at: 1754200000000
updated_at: 1763839800000
---

# git仓库状态汇报

## 适用场景
- 用户需要了解当前工作区的改动、文件变更与 Git 状态
- 用户询问最近的提交记录、某次提交改了什么、有哪些分支/标签
- 用户询问某个文件/目录的历史

## 执行步骤
1. 先调用 `git_status` 获取工作区状态矩阵（分支、上游、已暂存/未暂存/未跟踪/冲突）
2. 需要看具体差异时调用 `git_diff`（`staged=true` 看已暂存；`path` 限定文件）
3. 用户问“最近提交了什么/版本历史”时调用 `git_log`（可加 `limit`/`path`/`author`/`since`）
4. 用户问“某次提交改了什么”时调用 `git_show`（先看文件清单；补丁被裁剪时用 `path` 精读）
5. 用户问“有哪些分支/标签”时调用 `git_refs`
6. 需要定位某段代码何时被删除/新增时，用 `git_log` 的 `pickaxe` 参数

## 输出规范
- 按“未提交改动 / 新增文件 / 已暂存 / 未跟踪”分组列出；对每个文件说明改动类型（新增/修改/删除/重命名）
- 统计变更文件总数；无改动时明确说明“工作区干净”
- 历史结论必须来自 `git_log` / `git_show` 的实际返回，不要凭记忆或上下文编造提交 SHA、作者、时间
- `.mdgo` 内部数据与敏感文件不会被工具返回；不要猜测或编造它们的内容
- 输出被截断时，明确说明截断范围，并给出下一步（`path` 缩小、`skip` 翻页、`stat_only` 只看统计）
