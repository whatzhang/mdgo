# Agent Git 只读能力重构与增强方案

> 状态：已实施（2026-11-23）；`cargo test --lib` 全量 575 项通过（含 `core::git` 37 项）。
> 决策：D1/D2 采用推荐项；D3 采用**严格敏感过滤**（敏感文件名与内容都不进入 Agent 输出，`.env.example` 同样隐藏）。
> 范围：Agent 对话内对「当前打开知识库」所关联 Git 仓库的**读操作**：状态、历史、提交详情、差异、分支/标签、文件旧版本（可选 blame）。
> 不在本次范围：stage / unstage / commit / checkout / reset / stash / merge / rebase / fetch / pull / push / tag 写入 / 子模块更新 / hooks。
>
> **结论先行**：现有 `git_status` 能用，但只是 `git status --short` 的薄封装，存在「信息不足、不是严格只读、超时不杀进程、`.mdgo`/敏感内容泄漏、输出预算错位、与 UI 侧 Git 实现双轨」六类问题；且 Agent 侧完全没有「提交历史」这一最核心的读能力。建议先做**能力下沉 + `git_status` 重写**，再增量开放 `git_log` / `git_show` / `git_refs` / `git_diff` 增强，最后补安全与测试。

---

## 1. 现状评审

### 1.1 能力盘点

| 能力 | UI 侧（Tauri command） | Agent 侧（当前） | 本次方案 |
| --- | --- | --- | --- |
| 工作区状态 | `commands::git::git_status`（porcelain v1 `-z`，含重命名）`git.rs:892` | `git_status`（`status --short` 文本）`tools/mod.rs:1825` | 重写为 porcelain v2，补分支/上游/分组/计数 |
| 提交历史 | `git_log`（分页/按文件/按 ref）`git.rs:416` | ❌ 完全没有 | 新增 `git_log` |
| 某次提交改了什么 | `git_commit_files` + `git_read_blob` | ❌ | 新增 `git_show` |
| 工作区/暂存区 diff | 前端自行拼 blob 对比 | `git_diff`（staged/stat_only）`tools/mod.rs:1870` | 增强：范围、路径、上下文、预算内分块 |
| 分支/远程/标签/上游 | `git_parse_refs`（6 个 git 进程并发）`git.rs:568` | ❌（只有裸 status） | 新增 `git_refs` |
| 文件历史 | `git_log(filepath)` | ❌ | `git_log(path)` 覆盖 |
| 旧版本文件内容 | `git_read_blob` | ❌ | P3：`git_show` 的 `content=true` |
| 某行是谁改的 | ❌ | ❌ | P3 可选 `git_blame` |
| 按改动内容找提交 | ❌ | ❌ | `git_log` 的 `pickaxe` 参数 |
| 只读保证/取消/超时 | `run_git_with_timeout` 会 kill | `spawn_blocking + tokio timeout` 不 kill | 统一 runner |

结论：UI 侧的 Git **读能力其实已经比较完整**，Agent 侧只暴露了其中两个最薄的子集。最大的缺口不是「status 不够好」，而是**没有历史/提交详情/引用查询**。

### 1.2 `git_status` 逐项评审

| 编号 | 现状 | 风险/影响 | 目标 |
| --- | --- | --- | --- |
| P0-1 | `run_git_tool` 未设置 `GIT_OPTIONAL_LOCKS=0`、`GIT_TERMINAL_PROMPT=0`、`GIT_PAGER=cat`，stdin 未置空 `tools/mod.rs:1837-1864` | `git status` 可能刷新 index 并写 `.git/index.lock`；多个 `concurrency_safe=true` 的 git 工具并行时可能出现 `index.lock` 冲突；仓库配置的 fsmonitor hook 可能被触发 | runner 统一环境与 `-c core.fsmonitor=false`；状态/日志/diff 全程不写仓库 |
| P0-2 | 超时用 `tokio::time::timeout` 包 `spawn_blocking`，超时只放弃等待，**不 kill 子进程**；工具不读 `ctx.cancel` | 取消/超时后 git 进程继续跑，线程被占住；大仓库连续取消会堆积进程 | runner 内部持有 `Child`，超时/取消时 `kill + wait`；工具等待 `ctx.cancel` |
| P0-3 | 只输出 `status --short` 文本：无分支、上游、ahead/behind、detached/unborn；无「已暂存/未暂存/未跟踪/冲突」分组；重命名只是 `old -> new` 文本；最多 200 行且无续查提示 | 模型无法回答「我在哪个分支/领先远端多少/哪些已暂存」；200 行后信息直接丢失；重命名旧路径无法可靠解析 | `--porcelain=v2 --branch -z` 一次拿全；结构化分组 + 计数；截断显式提示 |
| P0-4 | `.mdgo/` 内部数据会出现在 status/diff/log/show 输出；`.env`、`*.pem`、`id_rsa` 等敏感文件 diff 未过滤（`git_ai` 有自己的 `filter_patch`，Agent 侧没有 `git_ai.rs:352-392`） | 应用配置/技能/索引路径与密钥内容可能被送进模型上下文 | 统一 `filter`：`.mdgo` 完全隐藏；敏感/生成文件隐藏内容但保留文件名/统计；输出加提示 |
| P1-1 | `GIT_DIFF_MAX_CHARS = 60_000`（`limits.rs:68`），但 Session 落历史时 24k 再截断一次（首 4/5 + 尾 1/5）；`git_diff` 只从头部硬切，后面的文件全丢 | 模型看到的截断与工具声称的预算不一致；大 diff 后半段不可见，模型容易误判「只有这些改动」 | 输出预算 ≤ 18k 字符；先给 stat 清单，再按 `diff --git` 分块装补丁；被省略文件明确列出 |
| P1-2 | 错误是 `Git 命令失败: {stderr}` 原文；未区分「不是仓库 / 空仓库 / 引用不存在 / git 未安装 / 超时 / dubious ownership」 | 模型收到不可操作的英文 fatal，容易重试同一调用；非 Git 知识库场景会反复失败 | `GitError` 分类 + 中文可操作提示（含下一步建议） |
| P1-3 | 两套 runner：`commands/git.rs:85-147`（stderr 独立线程、超时 kill、env 硬化）vs `tools/mod.rs:1837`（简化版）；两套路径校验/解析器 | 行为漂移；修一处漏一处；`git_ai.rs` 同时依赖两边（`:108,:124,:144`） | 下沉到 `core/git`，commands 与 tools 共用 |
| P1-4 | Agent 没有 `git_log` / `git_show` / `git_refs`；`repo-status` 技能却声明了「提交记录」触发词，只挂了 status/diff | 用户问「最近提交了什么」「这个提交改了什么」时，模型只能猜或用 `read` 翻文件 | 新增 3 个只读工具（见 §5） |
| P2-1 | `git_diff` 已计算 `structured`，但 `GitDiffTool` 直接 `Ok((text, _structured))` 丢弃（`loop_tools.rs:2979`）；`record_tool_result_structured` 目前无调用方 | 前端无法渲染 status 矩阵/diff stat 增强卡片；结构化能力是死代码 | 先保持文本输出；P3 再评估是否接 `structured`（需扩 sink 接口） |
| P2-2 | 调度器 `run_one` 与工具内部都打 `on_call/on_result`（`tool_calls.rs:123,159`；git 工具 `loop_tools.rs:2922,2925`） | 同一次调用可能出现两张轨迹卡片（文档已记录） | 新增 git 工具**不再自打点**，交给调度器；老工具清理另立任务 |
| P2-3 | Agent 侧 git 工具没有单测；`commands/git.rs` 只有解析器测试 | 路径注入、`.mdgo` 绕过、空仓库、重命名等回归无保护 | 解析器/校验器纯函数单测 + 临时仓库集成测试 |
| P2-4 | `git_parse_refs` 每次 6 个进程；status 想拿分支还要额外查 | 首屏/每次工具调用多几十毫秒 | status 用 porcelain v2 一次拿全；refs 先串行 3 次，实测慢再并行 |

### 1.3 架构评审

当前调用链：

```text
Agent tool ──> core/agent/tools::git_status ──> run_git_tool ──┐
                                                               ├─> git 子进程
UI command ──> commands/git::git_status ──> run_git_with_timeout┘
git_ai ─────> commands::git::git_status + core/agent/tools::git_diff（两边都调）
```

问题不在「哪个函数写得不好」，而在**同一种能力有两个实现、两套语义**：

- 路径基准：`-C dir` + `status --short` 是 cwd 相对；porcelain v1/v2 是仓库根相对；`git diff` 输出是仓库根相对。Agent 目前没有统一基准。
- 只读保证：UI runner 有 env 硬化 + kill，Agent runner 没有。
- 错误语义：UI 返回原始 stderr，Agent 再包一层 `Git 命令失败`，两边都无法分类。
- 预算：UI 不设字符预算，Agent 60k，Session 24k，三层口径。

因此本次不是「给 git_status 加参数」，而是**先把 Git 读能力收敛为一个 core 模块，再在其上做工具**。

---

## 2. 目标与非目标

### 2.1 目标

1. **单一事实源**：所有 git 子进程只从 `core/git` 发出；commands 与 Agent tools 共用 runner、模型、解析器、路径校验。
2. **只读且可证明**：读工具不写工作区、不写 index、不留下 lock；仓库配置不能借 textconv/external diff/fsmonitor 执行任意命令。
3. **能力完整**：状态、历史、提交详情、diff、分支/标签/远程、文件历史；覆盖空仓库、detached HEAD、无上游、子目录知识库、非 Git 目录。
4. **模型友好**：输出有分组、有计数、有明确截断提示与下一步建议；错误可操作；预算与 Session 截断对齐。
5. **安全一致**：`.mdgo` 永不出现；敏感/生成文件内容不送模型；提交信息与 patch 视为不可信数据做注入包裹。
6. **可测试**：解析器/校验器纯函数单测；临时仓库集成测试覆盖真实 git 行为；只读性有断言。

### 2.2 非目标

- 不做任何 Git 写操作；不新增 `git_add` / `git_reset` / `git_branch` 等。
- 不做网络操作（fetch/pull/push）；`ahead/behind` 只读本地跟踪引用，不联网。
- 不做 Git GUI/历史页面改造；commands 的返回结构保持兼容。
- 不引入 Git 库（git2/libgit2）；继续调用系统 `git`，避免打包体积与行为差异。
- 不在本期做 blame（列为 P3 可选）。
- 不在本期做结构化 UI 卡片（列为 P3 可选）。

---

## 3. 关键设计决策（请评审拍板）

| ID | 决策 | 选项 | 推荐 | 影响 |
| --- | --- | --- | --- | --- |
| D1 | 工具粒度 | A. 5 个核心工具：`git_status`/`git_log`/`git_show`/`git_diff`/`git_refs`；B. 4 个（refs 折叠进 status）；C. 8 个（再加 blame/cat/tag/remote） | **A**（blame 作为 P3 可选第 6 个） | 工具 schema 占上下文；5 个覆盖 95% 读场景，模型选择成本低 |
| D2 | 知识库是仓库子目录时 | A. 元数据（log/refs）全仓库，内容（status/diff/show 的 patch）只限知识库子树；B. 全部只限知识库子树；C. 必须打开仓库根目录 | **A** | B 会让「本仓库历史」缺提交；C 会拒绝常见用法；A 在输出里注明「仓库根 / 作用域」 |
| D3 | 敏感文件 | A. 文件名/统计保留，只隐藏内容；B. 文件名+内容完全隐藏；C. 不过滤 | **B（严格模式）**：`.env*`（含 `.env.example`）、`*.pem/*.key/id_rsa*` 等一律完全隐藏；显式 path 指向敏感文件直接拒绝 | 最安全；代价是模型不知道敏感文件存在，状态里只给「已隐藏 N 项」 |
| D4 | 旧版本文件内容 | A. `git_show` 加 `content=true`（配合 `path`）；B. 单独 `git_cat` 工具；C. 不做 | **A**（P3 实现） | 少一个工具；schema 稍复杂但可接受 |
| D5 | 输出形态 | A. 先纯文本（模型友好），结构化卡片后续再做；B. 本次就扩展 sink 输出 structured | **A** | B 要改 `ToolEventSink`/调度器/前端协议，风险与工作量都大 |
| D6 | 命令层改造 | A. 保持 sync Tauri command，新建 sync runner（内部 kill 开关）供异步工具 `spawn_blocking` 调用；B. 全部 commands 改 async + tokio::process | **A** | B 改动面大、收益主要是取消；A 用 kill 开关即可达到同等效果 |

> 已确认：D1=A（5 个核心工具）、D2=A（元数据全仓库/内容限知识库子树）、D3=B（敏感文件严格完全隐藏）；D4/D5/D6 按推荐执行。

---

## 4. 目标架构

### 4.1 分层

```text
┌─────────────────────────────────────────────────────────────┐
│ 表达层                                                       │
│  commands/git.rs（UI IPC，薄封装，签名/结构保持兼容）          │
│  core/agent/loop_tools/git.rs（5 个 Tool，schema + 事件）    │
└──────────────────────────────┬──────────────────────────────┘
                               │ 只依赖 core::git 的公开接口
┌──────────────────────────────▼──────────────────────────────┐
│ core/git（唯一 Git 读实现）                                  │
│  GitReadService：status / log / show / diff / refs / blame   │
│  GitRunner：进程、env、超时、取消、输出上限、错误归一         │
│  RepoScope：仓库根、知识库前缀、路径/引用校验                 │
│  model / parse / filter / render：类型、纯解析、安全过滤、渲染 │
└─────────────────────────────────────────────────────────────┘
```

依赖规则：

- `core/git` **不依赖** `core::agent`、Tauri、LLM；只依赖 `std`、`tokio`、`serde`、`chrono`（日期显示）、`core::security`（注入包裹）。
- `core/git` 不读 `KbSearchConfig`；调用方传 `dir_path` 与 `GitReadLimits`。
- `commands/git.rs` 与 `core/agent/loop_tools/git.rs` 不互相引用。
- `commands/git_ai.rs` 改为只依赖 `core::git`，删除本地 `filter_patch`/`should_skip_path` 副本。

### 4.2 文件清单

| 文件 | 职责 | 预估 |
| --- | --- | --- |
| `core/git/mod.rs` | `GitReadService` 门面、`pub use` 模型 | 100 行 |
| `core/git/error.rs` | `GitError` + 中文映射 + stderr 归类 | 150 行 |
| `core/git/runner.rs` | 子进程执行、env、超时、取消、kill、输出上限 | 250 行 |
| `core/git/scope.rs` | `RepoScope`、路径/引用校验、`--literal-pathspecs` | 200 行 |
| `core/git/model.rs` | status/log/show/diff/refs 的 serde 模型 | 200 行 |
| `core/git/parse.rs` | porcelain v2 / log -z / name-status -z / numstat -z / refs / blame 解析 | 400 行 |
| `core/git/filter.rs` | `.mdgo`、敏感、生成文件、二进制判定 | 120 行 |
| `core/git/render.rs` | 模型可读文本、分组、预算、截断提示 | 300 行 |
| `core/git/limits.rs` | Git 读预算常量（字符/条数/超时/字节） | 50 行 |
| `core/agent/loop_tools/git.rs` | 5 个 Tool 的 spec/参数解析/execute | 450 行 |
| `core/git/tests.rs` 或各文件 `#[cfg(test)]` | 单测 + 临时仓库集成测试 | 600 行 |

> `loop_tools.rs` 保持文件形式，新增 `pub mod git;`，对应 `loop_tools/git.rs`（Rust 允许 `foo.rs` + `foo/bar.rs`）。

---

## 5. 工具契约

所有工具共同约定：

- 只读、`concurrency_safe = true`、无需审批（不进 `DestructiveWritePolicy`）。
- `params.dir` 不接受：仓库固定为当前知识库目录，模型无法指定其他仓库。
- 路径参数一律相对**当前知识库目录**；越界、绝对路径、盘符、`..`、控制字符、`.mdgo`、敏感文件直接拒绝。
- `.mdgo` 与敏感文件采用**严格模式**：不输出路径、不输出统计、不输出内容；status 仅给「已隐藏 N 项」计数；显式 `path` 指向它们直接报错。
- 引用参数先经严格字符校验，再 `rev-parse --verify --end-of-options` 解析为完整 SHA 后使用。
- 输出末尾带「下一步」提示（缩小 path / 增大 skip / stat_only / show）。
- 输出统一经 `security::wrap_suspicious`（提交信息、patch、文件内容均可能被注入）。

### 5.1 总表

| 工具 | 一句话 | 关键参数 | 默认预算 |
| --- | --- | --- | --- |
| `git_status` | 工作区/暂存区状态 + 分支/上游 | `path?`, `max_entries?` | 300 项 / 12k 字符 |
| `git_log` | 提交历史（支持分页/路径/作者/时间/消息/改动搜索） | `limit?`, `skip?`, `rev?`, `path?`, `author?`, `since?`, `until?`, `grep?`, `pickaxe?`, `ignore_case?` | 20 条（max 100） |
| `git_show` | 某次提交的元信息 + 文件清单 + 补丁 | `rev`, `path?`, `stat_only?` | 100 文件 / 18k 补丁 |
| `git_diff` | 工作区/暂存区/两个提交之间的差异 | `staged?`, `from?`, `to?`, `path?`, `stat_only?`, `context?`, `ignore_whitespace?` | 18k 字符 |
| `git_refs` | 分支/远程分支/标签 + 当前分支/上游 | `kind?`, `max?` | 50/组 |
| `git_blame`（P3） | 指定行范围的逐行作者 | `path`, `start_line?`, `end_line?`, `rev?` | 200 行 |

### 5.2 `git_status`

参数：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "path": { "type": "string", "description": "只看知识库内某个相对路径（可选）" },
    "max_entries": { "type": "integer", "minimum": 1, "maximum": 500, "description": "最多展示条目数，默认 200" }
  }
}
```

输出示例：

```text
Git 仓库：mdgo（仓库根：G:\gitProject\mdgo；作用域：当前知识库）
分支：main（上游 origin/main，领先 2，落后 0）
HEAD：a1b2c3d
改动：共 12 项（已暂存 3 / 未暂存 5 / 未跟踪 4 / 冲突 0）
已隐藏：.mdgo 内部数据 2 项；知识库范围外 0 项

[已暂存]（3）
M  src/core/git/runner.rs
A  src/core/git/status.rs
R  docs/old.md -> docs/new.md

[未暂存]（5）
 M src/main.rs
 D docs/tmp.md
...

[未跟踪]（4）
?? notes/today.md
...
```

超过 `max_entries` 时追加：

```text
[提示] 条目已截断：仅展示前 200 项（共 312 项）；用 path 过滤或增大 max_entries（最多 500）。
```

实现要点：

- 一条命令：`git status --porcelain=v2 --branch --untracked-files=all -z`。
- 解析 `# branch.oid/head/upstream/ab` 头；`1/2/u/?/!` 条目。
- 已暂存 = X 非 `.` 且非 `?`；未暂存 = Y 非 `.`；未跟踪 = `?`；冲突 = `u`。
- 一个文件可能同时出现在「已暂存」和「未暂存」，计数按组分别统计，不强行去重；因此分组之和可能大于总条目数。
- `-z` 下头行与条目都可能是 NUL 分隔；解析器应对 `#` 头行同时兼容 `\0`/`\n` 结束（不同 Git 版本行为差异），不依赖固定行序。
- 空仓库：`branch.oid (initial)` → 输出「仓库尚无提交（unborn HEAD），当前文件均为未跟踪」。
- detached：`branch.head (detached)` → 输出「游离 HEAD（detached）」并给 `HEAD` SHA。
- 裸仓库：`--is-bare-repository` 为真 → 返回「裸仓库无工作区，可查看历史/分支」。
- 非 Git 目录：`RepoScope::resolve` 失败 → 「当前知识库目录不是 Git 仓库…」。

### 5.3 `git_log`

参数：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "本页条数，默认 20" },
    "skip": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "跳过条数，用于翻页，默认 0" },
    "rev": { "type": "string", "description": "起始引用（分支/标签/提交 SHA），默认 HEAD" },
    "path": { "type": "string", "description": "只看涉及该路径的提交（知识库内相对路径）" },
    "author": { "type": "string", "description": "作者过滤（git --author 正则）" },
    "since": { "type": "string", "description": "起始时间（git 可解析格式，如 2025-11-01 / 2 weeks ago）" },
    "until": { "type": "string", "description": "结束时间" },
    "grep": { "type": "string", "description": "提交消息搜索（固定字符串）" },
    "pickaxe": { "type": "string", "description": "搜索哪次提交新增/删除了该字符串（git -S）" },
    "ignore_case": { "type": "boolean", "description": "grep 忽略大小写（--regexp-ignore-case），默认 false" }
  },
  "required": []
}
```

输出示例：

```text
提交历史（HEAD -> main；显示 20 条，可能还有更早提交）
* a1b2c3d 2025-11-20 14:03 +0800 张三  修复登录超时 (#123)  [HEAD -> main, tag: v1.2]
* d4e5f6a 2025-11-19 09:12 +0800 李四  合并 feature/login（merge）  [feature/login]
...

[提示] 结果达到 limit=20；用 skip=20 继续，或加 path/author/since 缩小范围。
```

实现要点：

- `git log -z --no-color --no-show-signature --topo-order --decorate=short --format=%H%x00%h%x00%P%x00%aI%x00%cI%x00%an%x00%ae%x00%D%x00%s`
- 用 `-z` 做提交分隔、`%x00` 做字段分隔，**修掉当前 `commands/git.rs` 用 `\x1e` 做记录分隔可能被提交消息内容命中的隐患**；解析时按固定 9 字段读取，并容忍提交之间可能出现的空 token。
- 列表只取 subject，不取 body（body 交给 `git_show`），控制 token。
- `pickaxe` 用 `-S<string>`；`grep` 用 `--fixed-strings --grep=<str>`；`ignore_case` 加 `--regexp-ignore-case`（作用于 grep）。
- `path` 用 `--literal-pathspecs` + `--`；`rev` 解析为完整 SHA 后传给 git。
- 空仓库直接返回「仓库尚无提交」；不调用 `git log`。
- `len == limit` 时提示可能还有更多；不额外跑 `rev-list --count`（省一次进程）。

### 5.4 `git_show`

参数：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "rev": { "type": "string", "description": "提交 SHA / 分支 / 标签（必填）" },
    "path": { "type": "string", "description": "只看该路径的补丁（可选，知识库内相对路径）" },
    "stat_only": { "type": "boolean", "description": "只输出文件清单与增删统计，不输出补丁，默认 false" }
  },
  "required": ["rev"]
}
```

输出示例：

```text
提交 a1b2c3d（完整 SHA：...）
作者：张三 <zhang@example.com>  2025-11-20 14:03 +0800
提交者：张三 <zhang@example.com>  2025-11-20 14:03 +0800
引用：[HEAD -> main]
标题：修复登录超时 (#123)
正文：
  登录超时后未刷新 token；调整重试上限为 1。
变更：3 个文件（+42 -7）
M src/auth.rs (+30 -5)
A tests/auth_test.rs (+12 -2)
R old.rs -> new.rs (+0 -0)

补丁（按 18,000 字符预算裁剪）：
diff --git a/src/auth.rs b/src/auth.rs
...
[提示] 补丁已截断；用 path="src/auth.rs" 精读，或 stat_only=true 只看统计。
```

实现要点：

- 元信息：`git show -s --no-color --no-show-signature --format=... <sha>`。
- 文件清单：复用 `diff-tree -r -M --name-status -z` + `--numstat -z`（与现有 `git_commit_files` 同口径）。
- 合并提交：与**第一父提交**比较，输出注明「merge commit，diff vs first parent」；根提交用 `--root`。
- 补丁：`git diff --patch --no-color --no-ext-diff --no-textconv --unified=3 <parent> <sha> -- <path>`；根提交用 `git show --format= --patch --root`。
- 正文最多 4k 字符；文件清单最多 100；补丁按 `diff --git` 分块装预算。
- 不输出 GPG 签名、不跑 textconv、不跑 external diff。
- P3：`content=true` + `path` 时输出 `git show <sha>:<path>` 的原始内容（带二进制/大小上限）。

### 5.5 `git_diff`（增强）

参数：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "staged": { "type": "boolean", "description": "查看已暂存（--cached）差异，默认 false" },
    "from": { "type": "string", "description": "起始引用（与 to 组成范围；单独给 from 表示工作区 vs from）" },
    "to": { "type": "string", "description": "结束引用（必须与 from 同时给）" },
    "path": { "type": "string", "description": "只看知识库内某个相对路径" },
    "stat_only": { "type": "boolean", "description": "只输出文件级增删统计，默认 false" },
    "context": { "type": "integer", "minimum": 0, "maximum": 10, "description": "补丁上下文行数，默认 3" },
    "ignore_whitespace": { "type": "boolean", "description": "忽略空白差异，默认 false" }
  }
}
```

规则：

- `staged=true` 与 `from/to` 互斥；同时给 → `InvalidArgs`。
- `to` 不给 `from` → `InvalidArgs`（避免「to 相对谁」的歧义）。
- 都不给 → 工作区 vs 暂存区；`staged=true` → 暂存区 vs HEAD。
- 输出先 stat 清单，再补丁；补丁分块装预算，敏感/生成/二进制块跳过并计数。
- 空结果区分「工作区干净」「暂存区为空」「该范围无差异」。

### 5.6 `git_refs`

参数：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "kind": { "type": "string", "enum": ["all", "branches", "tags", "remotes"], "description": "默认 all" },
    "max": { "type": "integer", "minimum": 1, "maximum": 100, "description": "每组最多展示条数，默认 50" }
  }
}
```

输出示例：

```text
当前分支：main（HEAD a1b2c3d）
本地分支（3）：
* main a1b2c3d [origin/main ↑2 ↓0]
  feature/x d4e5f6a
远程分支（2）：
  origin/main a1b2c3d
  origin/dev 9988776
标签（1）：
  v1.2 -> abc1234
[提示] 仅展示前 50 条/组；共 120 个本地分支。
```

实现要点：

- `git for-each-ref --format=%(refname)%00%(objectname)%00%(*objectname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)` 一次拿全。
- `%(upstream:track)` 解析 `[ahead 2, behind 1]`、`[gone]`。
- `symbolic-ref -q --short HEAD` 判断当前分支；失败时用 `rev-parse --short HEAD` 判断 detached/unborn。
- 不输出 remote URL（可能含凭据），只输出名字/分支。
- 每类最多 `max` 条，超出给总数。

### 5.7 `git_blame`（P3 可选，建议后置）

参数：`path`（必填）、`start_line`、`end_line`（默认 1..50）、`rev?`。
输出：`<short> <author> <YYYY-MM-DD> <line>: <code>`，最多 200 行。
风险：大文件慢、输出大；默认不开放，等前 5 个工具稳定后再评估。

### 5.8 错误与截断约定

错误统一形如：

```text
Git 读取失败：<中文原因>
建议：<下一步>
```

| 场景 | 文案（要点） |
| --- | --- |
| 非 Git 目录 | 当前知识库目录不是 Git 仓库（未找到 `.git`）。请打开仓库目录，或在目录中执行 `git init`。 |
| 空仓库 | 仓库尚无提交（unborn HEAD），当前文件均为未跟踪。 |
| 裸仓库 | 当前目录是裸仓库（bare），没有工作区；可查看历史/分支，无法查看状态/差异。 |
| 引用不存在 | 找不到引用 `x`；可用 `git_log` 查看最近提交，或用 `git_refs` 查看分支/标签。 |
| 路径非法 | 路径必须是知识库内相对路径，不能是绝对路径、不能包含 `..`、不能指向 `.mdgo`。 |
| 超时 | Git 读取超时（Ns）。仓库可能过大或位于慢速磁盘；请用 path/limit 缩小范围。 |
| 输出过大 | Git 输出超过上限；请用 path 缩小范围，或使用 stat_only=true。 |
| 索引被占用 | Git 索引被其他操作占用；请稍后重试。不要删除 `.git/index.lock`，除非确认没有 Git 进程在运行。 |
| 所有者不一致 | Git 拒绝访问（dubious ownership）；请在终端执行 `git config --global --add safe.directory <仓库路径>` 后重试。 |
| 未安装 Git | 未找到 git 可执行文件；请安装 Git 并确保在 PATH 中。 |

截断约定：

- 任何被截断的输出必须带 `truncated=true` 语义的提示，并说明「怎么看到剩余部分」（path / skip / stat_only / show）。
- 不允许只从中间硬切一行或一个 UTF-8 字符；按 `diff --git` 块或整行切。
- 补丁预算 18,000 字符（低于 Session 的 24k 截断线）；status 12,000；log 以条数为主。

---

## 6. 核心层设计

### 6.1 `GitRunner`

```rust
pub struct GitRunner {
    program: String, // 默认 "git"；测试可注入
}

pub struct RunOptions<'a> {
    pub args: &'a [String],
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub cancel: Option<&'a AtomicBool>,
}

pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub stdout_truncated: bool,
}

impl GitRunner {
    pub fn run(&self, root: &Path, opts: RunOptions<'_>) -> Result<GitOutput, GitError>;
    pub async fn run_async(
        &self,
        root: PathBuf,
        args: Vec<String>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<GitOutput, GitError>;
}
```

实现要点：

- 固定环境：`GIT_TERMINAL_PROMPT=0`、`GIT_OPTIONAL_LOCKS=0`、`GIT_PAGER=cat`、`GIT_ATTR_NOSYSTEM=1`、`LC_ALL=C`；`stdin=null`；Windows `CREATE_NO_WINDOW`。
- 固定前缀：`-C <root>`、`--no-pager`、`--literal-pathspecs`、`-c core.quotepath=false`、`-c color.ui=false`、`-c core.fsmonitor=false`、`-c log.showSignature=false`。
- stdout/stderr 各一个读线程，读到 `cap` 后继续 drain 但只计数（避免子进程阻塞），记录 `stdout_truncated`。
- 主线程每 10ms `try_wait`，检查 deadline 与 cancel；触发后 `kill + wait`，返回 `Timeout`/`Cancelled`。
- 异步封装：`spawn_blocking` + `RunControl`（`Arc<AtomicBool>`）；`tokio::select!` 取消时置位并等待阻塞任务退出，确保子进程被杀。
- 不 `shell`，所有参数为独立 `arg`。
- 错误统一走 `GitError`，禁止把原始 stderr 直接拼给模型。

### 6.2 `RepoScope`

```rust
pub struct RepoScope {
    pub root: PathBuf,      // git rev-parse --show-toplevel 的 canonical 路径
    pub base: PathBuf,      // 知识库目录 canonical 路径
    pub prefix: String,     // base 相对 root 的路径（"" 表示 base == root）
}
```

- `resolve(dir)`：canonicalize → `git -C dir rev-parse --show-toplevel` → 校验 `base.starts_with(root)`；Windows 大小写不敏感比较。
- `.git` 是文件（worktree）时 `--show-toplevel` 仍可用；裸仓库用 `--is-bare-repository` 识别。
- 路径参数：
  - 统一 `/`，拒绝绝对路径、盘符、`..`、控制字符、NUL、`:`（避免 `rev:path` 歧义）、首字符 `-`、`.mdgo` 组件（大小写不敏感）。
  - 转为仓库根相对：`prefix + "/" + path`；校验仍在 `base` 内（词法 + canonical）。
- 引用参数：
  - 允许 `HEAD`、分支、标签、短/长 SHA、`~`/`^` 后缀；拒绝空白、`..`、`@{`、`\`、`:`、首字符 `-`、超长。
  - `git rev-parse --verify --quiet --end-of-options <rev>^{commit}` 解析为完整 SHA；`--end-of-options` 不被旧版 Git 支持时回退到无该参数的同一命令（引用已严格校验）；失败给 `RevNotFound`。
  - `git diff` 的 `from/to` 分别解析，不使用模型传入的字符串直接拼命令。
- pathspec 统一 `--literal-pathspecs` + `--`；不再依赖 `:(literal)`，避免魔法前缀被误解析。

### 6.3 解析器（纯函数，带单测）

- `parse_status_v2(bytes) -> GitStatus`：`# branch.*` + `1/2/u/?/!`；`-z`；重命名 `2` 的后一个 NUL token 是旧路径。
- `parse_log_z(bytes) -> Vec<CommitSummary>`：`-z` 提交分隔 + `\0` 字段；固定 9 字段，避免 `\x1e` 碰撞。
- `parse_name_status_z` / `parse_numstat_z`：沿用现有实现，补 `-z` 边界与二进制。
- `parse_refs(bytes) -> RefList`：`for-each-ref` 的 6 字段；annotated tag 用 peeled OID。
- `parse_blame_porcelain`（P3）。
- 所有解析器输入 `&[u8]`，路径用 `String::from_utf8_lossy`；控制字符在渲染层转义，结构化数据保留原样。

### 6.4 过滤与安全

`filter.rs` 提供：

```rust
pub fn is_mdgo_internal(path: &str) -> bool;      // 任一路径组件 == .mdgo（忽略大小写）
pub fn is_sensitive_path(path: &str) -> bool;     // .env* / *.pem / *.key / id_rsa* / ...
pub fn is_generated_path(path: &str) -> bool;     // dist/ / node_modules/ / target/ / *.min.js / ...
pub fn is_binary_stat(add: &str, del: &str) -> bool;
```

- `.mdgo` / 敏感文件（严格模式）：status/diff/show/log(path)/blame **路径与内容全部不输出**；status 只计数「已隐藏 N 项」；path 参数指向它们直接拒绝；显式 `git_show path=...` 同样拒绝。
- 生成文件：status 保留文件名；diff/show 不输出内容，只输出 `[已跳过：生成文件]`；统计可保留。
- 二进制：numstat `-` 或 patch 含 `Binary files` → 不输出内容，标注。
- 注入：`render` 末尾对完整输出调用 `security::wrap_suspicious`；提交信息、分支名、补丁都算不可信数据。
- 命令注入：永不 `shell`；路径/引用经 §6.2 校验；不把 stderr 原文当命令。
- 配置注入：`--no-ext-diff --no-textconv`；`core.fsmonitor=false`；`GIT_ATTR_NOSYSTEM=1`。

### 6.5 渲染与预算

- `render_status`：仓库/分支/上游/HEAD/计数 → 分组 → 隐藏计数 → 截断提示。
- `render_log`：每条一行 `* <short> <date> <author> <subject> [refs]`；merge 标注。
- `render_show`：元信息 → 变更清单 → 补丁（分块）→ 提示。
- `render_diff`：范围说明 → stat 清单 → 补丁（分块）→ 提示。
- `render_refs`：当前分支 → 本地 → 远程 → 标签 → 提示。
- 所有渲染器接受 `GitReadLimits`，返回 `(String, RenderMeta { truncated, omitted_files, hidden_count })`，便于工具事件摘要/未来结构化。

---

## 7. 与现有代码的接线

### 7.1 命令层

- `commands/git.rs`：
  - 删除本地 `run_git*`、`parse_*`、`normalize_rel_path`、`validate_rev/oid`、`has_head`、`first_parent`、`read_blob_spec`（迁移到 `core/git`）。
  - `git_status` 改为调用 `GitReadService::status`，再映射为现有 `Vec<StatusEntry>`（保持 `path/x/y/from` 字段与前端兼容）。
  - `git_log` / `git_commit_files` / `git_read_blob` / `git_read_index_blob` / `git_parse_refs` 改为薄封装，签名与返回结构不变。
  - 写命令（stage/unstage/discard/commit/checkout/identity/pull/push）保留原有语义，但 runner 换成新 runner（获得 env 硬化、kill、错误分类）；`git_checkout` 的路径校验改用 `RepoScope`，修掉 `a/../.mdgo` 绕过。
- `commands/git_ai.rs`：
  - `git_status` / `git_diff` / `git_parse_refs` 调用改 `core::git`。
  - 删除 `should_skip_path` / `filter_patch` / `parse_numstat` 本地副本，改用 `filter` 与模型层。
  - 输出与现有 `CommitSuggestion` 字段保持一致，前端无感。

### 7.2 Agent 工具层

- 新建 `core/agent/loop_tools/git.rs`，实现 5 个 Tool：
  - `spec()`：`read_only_spec` + `timeout_ms = Some(25_000)`；描述写清「只读/路径相对知识库/`.mdgo` 与敏感内容不返回」。
  - `execute()`：解析参数（纯函数）→ `GitReadService` → `render` → `Ok(Value::String(text))`；错误转 `ToolError::Failed`。
  - **不自打点**：不在 execute 里 `ctx.sink.on_call/on_result`，交给调度器统一打点，避免双卡片。
- `loop_tools.rs`：
  - 旧 `GitStatusTool` / `GitDiffTool` 实现迁移为 `git.rs` 的新实现；旧 `git_status`/`git_diff` 读辅助与 `run_git_tool` 删除（`git_commit`/`git_checkout` 同步改用 core runner，逻辑不变）。
  - `build_loop_tool_registry` 注册 5 个新工具。
  - `read_only_spec` 改为 `pub(crate)` 供子模块使用。
- `core/agent/mod.rs`：`BASE_TOOLS` 增加 `git_log` / `git_show` / `git_refs`（5 个都常驻）；`ACTION_CLAIMS` 的 `git_commit.observe_tools` 增加三个只读工具。
- `core/skill.rs`：`ALLOWED_TOOLS` 增加 3 个新工具 + `tool_label` 中文名；测试 `allowed_tools_cover_registry` 会兜底。
- `core/subagent/mod.rs`：`read_only_tool_set()` 增加 3 个（子代理可查历史）。
- `commands/llm.rs`：`apply_grounding_validator` 的 `observed_local` 增加 3 个；测试同步。

### 7.3 技能与文档

- `resources/skills/repo-status/SKILL.md`：
  - `tools: [git_status, git_diff, git_log, git_show]`（移除写工具 `git_checkout`）。
  - 步骤改为：状态 → 需要历史用 `git_log` → 需要某次提交详情用 `git_show` → 需要差异用 `git_diff`。
  - 触发词保留「提交记录 / git log / 版本历史」。
- `resources/agent/rag_agent.md`：能力行补「Git 历史/提交详情」。
- 文档表格更新：
  - `docs/blog/04-工具系统-契约-注册与并行调度.md:399`（Git 行 + 工具数）；
  - `docs/blog/09-子代理与多 Agent 协同.md:76`（只读白名单）；
  - `docs/Agent 内核架构.md:287`（写/文件/Git 表）；
  - `docs/Agent 全链路评审与 DSH 对比.md:151`（工具全清单）；
  - `docs/Agent 能力建设归档.md:59`（只读工具集）。

### 7.4 审批与防幻觉

- 审批：5 个读工具不进 `DestructiveWritePolicy`；新增单测断言 `evaluate("git_log"/"git_show"/"git_refs", ...) == None`。
- 防幻觉：
  - `git_commit.observe_tools` 加入 5 个读工具（复述历史不算「声称提交」）。
  - `apply_grounding_validator` 视为「读过本地文件」。
  - `repo-status` 技能输出规范里要求「历史结论必须来自 `git_log/git_show`，不要凭记忆」。

---

## 8. 实施步骤（PR 拆分）

### PR1：core/git 下沉 + `git_status` 重写（低风险，先合）

- [ ] 新建 `core/git/{mod,error,runner,scope,model,parse,filter,render,limits}.rs`。
- [ ] `GitRunner` 实现 env、超时、kill、取消、输出上限；补 runner 单测。
- [ ] `RepoScope` + 路径/引用校验 + 单测（绝对路径、`..`、盘符、`-` 开头、`.mdgo`、`:`, 空）。
- [ ] `parse_status_v2` + `render_status`；覆盖 clean/unborn/detached/rename/conflict/untracked/中文/空格路径。
- [ ] `commands/git.rs` 的 `git_status` 改用新服务；保持 `Vec<StatusEntry>` 结构。
- [ ] `core/agent/tools/mod.rs` 的 `git_status` 读辅助与 `run_git_tool` 删除；`GitStatusTool` 改调新服务（参数 `path`/`max_entries`）；`git_commit`/`git_checkout` 同步改用 core runner（逻辑不变，`git_checkout` 路径走 `RepoScope`，顺带修 `a/../.mdgo` 绕过）。
- [ ] 只读性集成测试：status 前后 `.git/index` 字节/mtime 不变、无 `index.lock`。
- [ ] `cargo fmt` / `cargo clippy` / `cargo test`。

验收：UI 历史/状态页不回归；Agent `git_status` 能回答分支/上游/分组；非 Git 目录给友好提示；并行 20 次 status 无 lock 错误。

### PR2：历史/提交/引用工具（新能力）

- [ ] `parse_log_z`、`parse_name_status_z`、`parse_numstat_z`、`parse_refs` + 单测。
- [ ] `GitReadService::{log, show, refs}`；`render_log/show/refs`。
- [ ] 新建 `core/agent/loop_tools/git.rs`：`GitLogTool` / `GitShowTool` / `GitRefsTool`（+ 保留 status/diff 迁移）。
- [ ] 注册 + `BASE_TOOLS` + `ALLOWED_TOOLS` + `read_only_tool_set` + grounding/observe 列表。
- [ ] `repo-status` 技能更新。
- [ ] 集成测试：空仓库、merge、无上游、detached、路径过滤、pickaxe、分页。
- [ ] 文档表格更新。

验收：Agent 能回答「最近提交」「某提交改了什么」「有哪些分支/标签」「这个文件的历史」；输出预算内；无审批弹窗。

### PR3：`git_diff` 增强 + 安全加固

- [ ] `git_diff` 支持 `from/to/path/context/ignore_whitespace`；分块预算 + stat 清单。
- [ ] `filter` 统一 `.mdgo`/敏感/生成/二进制；`git_show`/`git_diff`/`git_log` 输出包裹 `wrap_suspicious`。
- [ ] `--no-ext-diff --no-textconv`、`core.fsmonitor=false`、`GIT_ATTR_NOSYSTEM=1` 全链路生效。
- [ ] 路径/引用注入测试矩阵；敏感文件集成测试；`git_checkout` 的 `.mdgo` 绕过回归测试。
- [ ] 错误分类 + 中文文案 + 单测。

验收：`.env`/`id_rsa` 内容不出现在工具输出；`.mdgo` 路径与内容完全不可见；`--output=`/`-p`/`..` 全部被拒；无 textconv 执行。

### PR4：打磨与可选能力

- [ ] `git_show` 的 `content=true`（旧版本文件内容）。
- [ ] `git_blame`（可选，默认不注册；先评估性能与需求）。
- [ ] 结构化结果（可选）：扩展 `ToolEventSink` 或 `record_tool_result_structured`，前端卡片。
- [ ] 清理旧 `git_status`/`git_diff`/`run_git_tool` 残留与文档引用。
- [ ] 性能基准：10 万提交 log、1 万文件 status、巨型 diff。
- [ ] 手工 QA 清单。

---

## 9. 测试计划

### 9.1 单元测试（纯函数）

- `normalize_scope_path`：`""`、`/abs`、`C:\x`、`a/../b`、`./a`、`a//b`、`-p`、`a\b`、`.mdgo/x`、`A/.MDGO/x`、`a:b`、含 NUL/换行。
- `validate_rev`：`HEAD`、`main`、`v1.0`、`HEAD~2`、`HEAD^`、`abc1234`、`--all`、`-p`、`HEAD:path`、`a..b`、`a@{1}`、空、超长。
- `parse_status_v2`：clean、staged、unstaged、untracked all、rename、copy、conflict、unborn、detached、ahead/behind、中文/空格/制表符路径。
- `parse_log_z`：多提交、merge、decorations、subject 含 `|`/换行、作者非 ASCII、空仓库。
- `parse_name_status_z` / `parse_numstat_z`：`R100`、`C`、`A/D/M`、二进制 `-`、重命名空 path。
- `parse_refs`：annotated tag peeled、无 upstream、`[gone]`、detached。
- `render_*`：计数、分组、预算、截断提示、转义控制字符。
- `filter`：`.mdgo` 任意层级、`.env`/`.env.example`（按最终决策）、`*.pem`、`dist/`、`node_modules/`。

### 9.2 集成测试（临时仓库）

用 `tempfile` + `git init`（dev-dependency 已有），设置 `user.name/email`、`commit.gpgsign=false`：

- 干净仓库 / 未跟踪 / 已暂存 / 重命名 / 冲突（构造 index stage）/ 二进制 / 空仓库 / detached / 无上游。
- `git_status` 只读性：记录 `.git/index` 的 bytes + mtime + 工作区文件 hash，调用后完全一致；无 `.git/index.lock`。
- 并发：20 个 `git_status`/`git_diff`/`git_log` 并行，无 `index.lock` 错误。
- 路径注入：`path="--output=C:\\temp\\x"`、`path="-p"`、`path="..\\..\\x"`、`path=".mdgo/config"` → 拒绝且不执行。
- 引用注入：`rev="--all"`、`rev="HEAD --output=x"` → 拒绝。
- 敏感：提交 `.env` / `id_rsa`，`git_diff`/`git_show` 输出不含值，只含「已跳过」。
- `.mdgo`：提交/未跟踪 `.mdgo/config`，所有读工具不出现该路径/内容。
- 非 Git 目录：`git_status` 返回中文提示，而不是 stderr。
- 取消/超时：runner 用测试注入的 `git` alias（`!powershell -Command Start-Sleep 10`）验证 kill 与 `Cancelled`；断言无残留进程（平台允许时）。

### 9.3 手工 QA

1. 真实知识库仓库问「现在有哪些改动？」→ 分组/计数/分支正确。
2. 「最近 10 条提交」→ 条数、作者、时间、refs 正确；`skip=10` 续页。
3. 「看下 <sha> 改了什么」→ 文件清单/统计/补丁；截断提示可操作。
4. 「这个文件的历史」→ 只列涉及该文件的提交。
5. 「哪个提交删了 XXX」→ `pickaxe` 命中。
6. 「有哪些分支/标签」→ 当前分支/上游/ahead-behind 正确。
7. 「main 和 feature 的差异」→ range diff。
8. 非 Git 目录 / 空仓库 / detached / 无上游 → 友好提示，不重试。
9. 观察工具卡片：无重复卡片；摘要/结果不超长。
10. 观察仓库：`git status` 前后无变化、无 lock、无新进程残留。
11. 敏感文件：`.env` 修改后问 diff → 内容不出现。
12. `.mdgo`：确认状态/历史/差异都不出现内部路径。

### 9.4 回归

- `cargo test` 全量；`cargo clippy -- -D warnings`；`cargo fmt --check`。
- UI 历史页/状态页/提交详情页手工回归（commands 签名/结构未变）。
- `git_ai` 生成提交信息手工回归（输出字段/统计不变）。
- `core/skill.rs::allowed_tools_cover_registry_and_base_tools` 通过。

---

## 10. 风险与缓解

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| commands API 变化导致 UI 回归 | 高 | 保持签名/字段；`StatusEntry` 只增不改；UI 手工回归 |
| 路径/引用校验过严，正常中文/空格路径被拒 | 中 | 单测覆盖；只拒绝危险字符，不按 ASCII 白名单限路径 |
| `.mdgo`/敏感过滤误伤（如 `.env.example`） | 中 | 决策 D3 明确；过滤规则集中且可单测；输出标注「已跳过」 |
| 输出预算仍被 Session 24k 二次截断 | 中 | 工具预算 ≤18k；stat 先行；分块而非硬切 |
| 巨型仓库 status/log 超时 | 中 | 10-20s 超时 + kill；提示用 path/limit；不缓存但可后续加 per-request scope 缓存 |
| Git 版本差异（porcelain v2 / `--end-of-options`） | 中 | 最低建议 2.24；`--end-of-options` 失败时回退（引用已严格校验）；porcelain v2 需 2.11+，过老给升级提示 |
| submodule/worktree 行为差异 | 低 | `--show-toplevel` 处理 worktree；submodule 只显示 gitlink 变更并标注 |
| 取消 kill 后留下 `.git/index.lock` | 低 | `GIT_OPTIONAL_LOCKS=0`；测试断言；不自动删 lock，提示用户 |
| 事件双打点 | 低 | 新 git 工具不自打点；老工具另立清理任务 |
| 工具数量增加导致 schema 上下文变大 | 低 | 5 个工具、描述精简；不新增 blame 直到有需求 |
| 安全过滤被仓库配置绕过（textconv/external diff/fsmonitor） | 高 | `--no-ext-diff --no-textconv`、`core.fsmonitor=false`、`GIT_ATTR_NOSYSTEM=1`；集成测试用自定义 driver 验证 |

---

## 11. 验收标准

1. `rg "run_git_tool"` 无结果；所有 `git` 子进程只在 `core/git/runner.rs` 中 spawn；Git 子进程单一出口。
2. `git_status` 输出包含：仓库/作用域、分支/上游/ahead-behind、HEAD、四类计数、分组条目、隐藏计数、截断提示。
3. Agent 可完成：状态、最近历史、某提交详情、工作区/暂存/范围 diff、分支/标签、文件历史；空仓库/detached/无上游/非 Git 均有友好输出。
4. 只读性：读工具调用前后 `.git/index` 与工作区文件 hash 不变、无 `.git/index.lock`；并行 20 次无 lock 错误。
5. 安全：`.mdgo` 与敏感文件的路径/统计/内容零出现（严格模式）；`--output=`/`-p`/`..`/绝对路径全部拒绝；textconv/external diff 不执行。
6. 预算：单次工具输出 ≤18k 字符（补丁）/12k（status）；截断提示可操作；Session 不再二次硬切。
7. 测试：新增解析/校验/渲染单测 ≥25 个、集成测试 ≥10 个；全量测试/clippy/fmt 通过。
8. 文档：技能与 5 处工具清单文档同步；`allowed_tools_cover_registry` 通过。
9. 手工 QA 12 项全过；无重复工具卡片；无审批弹窗。

---

## 12. 工作量与里程碑

| 阶段 | 内容 | 预估 |
| --- | --- | --- |
| P0 | 本方案评审、决策拍板 | 0.5 天 |
| P1 | core/git 下沉 + runner/scope/status + PR1 | 1.5–2 天 |
| P2 | log/show/refs + 工具注册 + 技能/文档 + PR2 | 2 天 |
| P3 | diff 增强 + 安全加固 + 错误/测试 + PR3 | 1–1.5 天 |
| P4 | 可选 content/blame/结构化 + 性能/QA | 1 天 |
| 合计 | 5–6.5 天（含自测；不含评审等待） | |

建议节奏：PR1 先合入验证「单一 runner + status」，再开 PR2；PR3 的安全加固不要与 PR2 混在一起，便于回滚。

---

## 13. 已确认决策

1. **D1 工具粒度**：5 个核心工具（status/log/show/diff/refs），blame 后置。
2. **D2 子目录知识库**：历史/分支按全仓库、内容按知识库子树。
3. **D3 敏感文件**：严格模式——文件名与内容都不进入 Agent 输出，`.env.example` 也隐藏；显式 path 指向敏感文件直接拒绝。UI 侧 commands 保持原行为（用户自己能看到自己的文件），仅 Agent 工具做严格过滤。
4. **旧版本文件**：`git_show` 的 `content=true`（P3）。
5. **`git_refs` 命名**：保留 `git_refs`。
6. **最低 Git 版本**：建议 2.24+；旧版回退并提示。

> 按 §8 的 PR1 开始实施；敏感文件严格模式作为验收硬条件，不随 PR 延后。

---

## 14. 实施结果（2026-11-23）

已落地：

- `core/git/`：`runner`（env 硬化、超时/取消 kill、输出上限）、`scope`（仓库根/知识库前缀、路径/引用校验）、`parse`（porcelain v2 / log -z / name-status / numstat / refs）、`filter`（严格 `.mdgo`/敏感/生成物）、`render`、`service`（status/log/show/diff/refs/commit_files/read_blob）。
- Agent 工具：`git_status`、`git_log`、`git_show`、`git_diff`、`git_refs` 全部注册为只读、`concurrency_safe=true`、25s 超时；严格模式下 `.mdgo` 与敏感文件的路径/统计/内容都不返回。
- 代码接线：`BASE_TOOLS` / `ALLOWED_TOOLS` / 子代理只读白名单 / Grounding Validator / Action Claim observe 列表 / `repo-status` 技能 / `rag_agent.md` 已同步。
- commands：`git_status`、`git_log`、`git_commit_files`、`git_read_blob`、`git_read_index_blob` 改走 `core/git`；所有 git 子进程统一从 `core/git/runner` 发出。
- 测试：`cargo test --lib` → 575 passed / 0 failed（其中 `core::git` 37 项）。覆盖只读性（index 不变、无 lock）、严格敏感隐藏、log/show/diff、refs、路径/引用注入、非 Git 目录、并行 status。
- 2026-11-23 UI 回归修复：`git_status` 命令层把 porcelain v2 的未变更位 `.` 映射回空格（前端提交页沿用 v1 约定），修复“取消暂存后仍留在待提交列表”；`gitRefreshStatus()` 同时把选中项迁移到新分组并刷新 diff 视角。

后续可选（本次未做）：`git_blame`、`git_show content=true` 的旧版本文件内容、结构化 UI 卡片、清理 `commands/git.rs` 中已无调用的旧解析器（当前仅产生 dead_code 警告，不影响行为）。
