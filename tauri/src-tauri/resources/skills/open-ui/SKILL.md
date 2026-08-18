---
id: open-ui
scope: system
name: 打开文件 / 页面
description: 当用户要求打开知识库中的某个文件、跳转到某个页面或视图（文件图谱、关联图谱、系统首页、日历、知识库监控、技能管理、MCP 管理、时间线、画布、白板、Mermaid 图表、词云、Git 管理、番茄钟、临时编辑器、编码器、视频、RAW 照片、正则测试、Cron 测试、书签预览、目录空间、Swagger、GraphQL、Nginx 配置编辑器等）时触发。
priority: 55
tools: [open-ui, ls, glob]
triggers: [打开, 跳转, 打开文件, 打开页面, 跳转页面, 跳转到, 打开ui, 打开 UI, open file, open page, open ui, 文件预览, 打开预览]
enabled: true
version: 2
created_at: 1760000000000
updated_at: 1761000000000
---

# 打开文件与页面

## 核心职责
- **打开知识库文件**：`open_file`（`relativePath` 为知识库内相对路径）
- **跳转应用页面/视图**：`open_page`（`page` 仅支持枚举内 26 种）fileGraph 文件图谱、noteGraph 文档关联图谱、dashboard 系统首页、calendar 日历/日程、knowledge 知识库监控面板、skill 技能管理页面、mcp MCP管理页面、timeline 文件时间线页面、canvas 画布、whiteboard 白板、mermaid mermaid图表预览编辑页面、wordCloud 词云、gitRecords Git 管理页面、pomodoro 番茄钟页面、tempEditor 临时编辑器、urlEncoder 编码器页面、video 视频播放页面、raw RAW 照片预览页面、regexTest 正则表达式测试页面、cron Cron 表达式测试页面、bookmarks 书签预览页面、dirSpace 目录空间数据统计大屏、swaggerDemo swagger api预览页面、graphQLPlayground GraphQL 预览接口测试页面、openRestyEditor nginx配置编辑器、fileType 文件类型分布

## 工作流
1. **定位文件**：用户给出明确路径直接用；只给文件名/模糊描述时，先用 `ls`/`glob` 在知识库内定位相对路径，确认唯一后再 `open_file`。
2. **打开**：调用 `open_file`（相对路径）或 `open_page`（页面枚举）。
3. **汇报**：说明已打开的文件/页面；失败时如实返回原因（文件不存在/路径非法/页面不支持）。

## 格式与约束
- `relativePath` **必须是知识库内相对路径**：如 `notes/plan.md`；禁止 `../`、绝对路径、盘符（前端会拒绝）。
- `page` 取值**只能使用工具枚举中的页面名**（模型不得自行发明页面名）。
- **不修改文件内容**：本工具只负责打开查看，删除/复制/还原等写操作不在本工具能力内。
- 打开失败（文件不存在、页面不支持、前端不可用）时如实反馈，不编造"已打开"。
- 页面打开或跳转后，**禁止主动向用户发起反问和推荐性建议以及引导性话语**, 文件打开后可主动向用户发起反问

## 注意事项
- 文件路径不确定时先 `glob`/`ls` 确认，避免打开错误文件。
- 需要读取文件**内容**做分析时，用 `read` 工具（`open_file` 只是打开查看，不把内容返回给模型）。
- 用户说"打开 xxx 看看内容/总结一下"：意图是**读取内容**，应优先用 `read`，而非 `open_file`。