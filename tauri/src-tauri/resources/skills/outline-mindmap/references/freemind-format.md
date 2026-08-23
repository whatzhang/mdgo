# FreeMind（.mm）格式参考（读 / 写）

> 供 `outline-mindmap` 技能读取与写出 `.mm` 思维导图文件。
> 前端（jsMind 解析器）宽容解析：读只取「标题 + 嵌套」，写用最小骨架即可。

## 1. 文件骨架

```xml
<map version="1.0.1">
  <node TEXT="根主题" FOLDED="true" POSITION="right">
    <node TEXT="一级主题" LINK="https://example.com">
      <node TEXT="二级主题"/>
    </node>
    <node TEXT="一级主题"/>
  </node>
</map>
```

- 根：`<map>` 下第一个 `<node>`（整棵树的主题）。
- 子节点：`<node>` 递归嵌套，嵌套深度 = 思维导图分支层级。
- `version` 常见 `1.0.0` / `1.0.1`，可忽略。

## 2. 读取规则（宽容解析）

| 属性 | 含义 | 读取时 |
|---|---|---|
| `TEXT` | 节点标题（**必取**） | 用于提炼主题/内容 |
| `FOLDED` | 是否折叠 | 忽略 |
| `POSITION` | 左右分支（right/left） | 忽略 |
| `LINK` | 外链/文件链接 | 可留意（同主题关联线索），不阻塞解析 |
| `BACKGROUND_COLOR`/`COLOR` | 颜色 | 忽略 |
| `STYLE`、`FONT`、`ARROW`、`EDGE`、`CLOUD` | 视觉样式 | 忽略 |

- 属性名**大小写不统一**（`TEXT`/`Text`/`text` 等均可能出现）：按不区分大小写处理；拿不到 `TEXT` 时该节点视为无标题，如实标注。
- **富文本标题**：部分工具（如 XMind 导出）把标题写成 HTML 片段（`TEXT="&lt;html&gt;&lt;body&gt;需求分析&lt;/body&gt;&lt;/html&gt;"`）：剥掉标签取纯文本（「需求分析」）再用于主题判断。
- 文件不是合法 XML、根节点缺失或 `TEXT` 全为空 → 标注「无法解析」，不臆测主题。

## 3. 写出规则（保存为 .mm）

- **最小骨架**，只写 `TEXT` 属性，不写任何视觉属性：

```xml
<map version="1.0.1">
  <node TEXT="根主题">
    <node TEXT="一级主题">
      <node TEXT="二级主题"/>
    </node>
    <node TEXT="一级主题"/>
  </node>
</map>
```

- 每个节点一个 `<node>`；无子节点的节点自闭合 `<node TEXT="叶子"/>`（或开闭标签均可，前端宽容）。
- 缩进嵌套便于阅读（2 空格/层），前端不依赖缩进。
- 文件编码 UTF-8。

## 4. 属性值 XML 转义（写错即解析失败）

| 字符 | 转义 |
|---|---|
| `&` | `&amp;` |
| `<` | `&lt;` |
| `>` | `&gt;` |
| `"` | `&quot;` |
| `'` | `&apos;` |

示例：`TEXT="R&D <V2>"` → `TEXT="R&amp;D &lt;V2&gt;"`。

## 5. 与 .opml 的取舍

- 同一棵树可存为 `.opml`（大纲笔记，默认，支持 note/color/_images 等丰富属性）或 `.mm`（思维导图，仅标题与嵌套）。
- 用户未指定格式时默认 `.opml`；指定"思维导图"或明确要 `.mm` 时写 `.mm`。
- `.mm` 无法表达 note/color/_images：转换时若原树含这些属性，向用户说明会丢失，或建议用 `.opml`。
