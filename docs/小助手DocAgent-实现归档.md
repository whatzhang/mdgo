# 小助手 DocAgent 实现归档（架构与文件地图）

> 配套 PRD：`docs/PRD-小助手文档Agent.md`（§16 实现状态）　验收：`docs/小助手DocAgent-P0验收清单.md`
> 基线：代码经 `cargo check` / `cargo test --lib`（348/348）/ `node --check` 校验。

## 1. 架构：一能力两宿主

```
DocAgent（文档子 Agent）
 ├─ core/docagent：单文件→章节+行号锚点解析；预算内按问题选章节；mtime 缓存；选区原语
 ├─ 宿主 A 浮层小助手（2/3）：commands/llm.rs::doc_agent_query（流式 llm:*，取消/任务/事件溯源）
 │      └─ 前端 css_js/modules/doc-qa.js + main.html #doc-qa-*
 ├─ 宿主 B 主 Agent 工具：doc_agent / parallel_doc_agent（loop_tools.rs，共享 run_doc_agent_direct）
 └─ 会话：type='doc' + chat_sessions.file_key（按文件聚类，服务端）
```

## 2. 文件地图（本次新增/改动）

| 层 | 文件 | 内容 |
|---|---|---|
| 内核 | `tauri/src-tauri/src/core/docagent/mod.rs` | DocSection/DocFile/DocMeta、read_doc(缓存)、build_context、build_selection_context、estimate_tokens、§N 引用协议、路径防穿越 |
| 常量 | `core/agent/limits.rs` | DOC_MAX_TURNS / DOC_DEFAULT_BUDGET_TOKENS / DOC_SECTION_MARKER / DOC_MAX_EDIT_BYTES |
| 命令 | `commands/doc.rs` | doc_read_meta / doc_build_context（已注册） |
| 命令 | `commands/llm.rs::doc_agent_query` | 流式问答（选区/整篇/关联文件 extra_files、reasoning 档位、任务中心、事件溯源） |
| 工具 | `core/agent/loop_tools.rs` | DocAgentTool / ParallelDocAgentTool / run_doc_agent_direct；doc_agent* 纳入 BASE_TOOLS |
| 会话 | `services/chat.rs` + `commands/chat.rs` | chat_sessions.file_key（ALTER）、set_session_file_key、list_sessions_by_file、chat_sessions_by_file |
| 规约 | `tauri/src-tauri/resources/agent/doc_agent.md` | DocAgent 角色与 `[§N]`/行号引用协议 |
| 前端 | `main.html` | #doc-qa-*（2/3 浮层、快捷动作、历史/新对话、深度档、写回模式、预览弹层）；FAB/选区入口接线 |
| 前端 | `css_js/modules/doc-qa.js` | 会话持久化与 file_key、流式渲染、引用定位、写回（复制/插入/替换/存笔记/存会话）、@提及/[[双链]]、Mermaid 预览、Explore/Execute |
| 文档 | docs/PRD、目录索引、P0 验收清单、本归档 | 需求/验收/实现三表联动 |

## 3. 状态摘要（与 PRD §16 同步）
- P0-1~P0-12：主体完成（含端到端用例清单）；写回/定位等细项标注在验收清单「受限/待补」。
- P1：1-5、8、9 完成；6 由后端历史压缩 + 浮层本地裁剪覆盖；7（相关文件提示）为简化形态。
- P2：全部未实现（双向定位/多源/全局唤起/记忆/图级），见 PRD 待办。

## 4. 验证基线
- `cargo check`：绿；`cargo test --lib`：348 passed / 0 failed
- `cargo test --lib docagent`：5/5（解析/防穿越/预算/选区）
- `node --check css_js/modules/doc-qa.js`：通过；main.html 内联脚本 V8 解析 0 error

## 5. P1 增强批次（2026-09，追加）
- 新增命令：`doc_related`（同目录词面重叠相关 TopN）、`doc_dir_files`（目录候选）、`doc_agent_query` 参数 `extra_folders` / `system_template`。
- 浮层新增：资料圈多选、大纲/脑图快捷动作、通用围栏代码预览（Mermaid 渲染）、会话风格模板、Explore 差异预览、语义相关 chips、压缩提示。
- 决策：#标签 目录内匹配已在浮层实现（front_matter_tags + doc_tag_files）；跨库标签检索仍归主 Agent 整库 RAG。P1 验收用例见 `小助手DocAgent-P0验收清单.md` §5。
