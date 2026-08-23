# OPML 格式规约（与前端解析/序列化 1:1 对齐）

> 本文件是 `outline-mindmap` 技能的 **.opml 格式规约**（读 / 写 / 转换通用）。

## 1. 文件骨架

```xml
<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head>
    <title>根主题标题</title>
    <dateCreated>2026-08-16T10:00:00.000Z</dateCreated>
  </head>
  <body>
    <outline text="一级主题">
      <outline text="二级主题"/>
    </outline>
  </body>
</opml>
```

- 根主题标题写在 `<head><title>`（**不是** body 里的 outline）。
- `<body>` 下直接放根主题的**子节点**（一级主题），每个 `<outline>` 一层。
- `dateCreated` 可选（前端序列化会写，AI 可不写）。

## 2. 节点属性（与前端字段映射）

| 属性 | 含义 | 对应 Knowledge Tree 字段 | 前端读取兼容写法 |
|---|---|---|---|
| `text` | 节点标题 | `text` | `text` / `TEXT` |
| `note` | 备注（Markdown） | `note` | `note` / `NOTE` |
| `COLOR` | 颜色标记 | `color` | `COLOR` / `color` |
| `_images` | 配图（相对路径/链接，多个用逗号或分号分隔） | `images` | `_images` / `images` / `_local_images` / `_mubu_images` |

属性书写顺序无要求；**未使用的属性不要写**（保持文件干净）。

## 3. 层级与闭合规则

- 有子节点的 outline：开闭标签包裹，子节点换行 + 缩进 2 空格（层级越深缩进越多）：

```xml
<outline text="父">
  <outline text="子">
    <outline text="孙"/>
  </outline>
</outline>
```

- 无子节点的 outline：自闭合 `<outline text="叶子"/>`。
- 树深度建议 ≤ 6 层（超出 jsMind 显示拥挤）。

## 4. XML 转义（必做，写错即解析失败）

| 字符 | 转义 |
|---|---|
| `&` | `&amp;` |
| `<` | `&lt;` |
| `>` | `&gt;` |
| `"` | `&quot;` |
| `'` | `&apos;` |

示例：`text="R&D <V2>"` → `text="R&amp;D &lt;V2&gt;"`。
**Markdown 备注（note）里若有 `<`/`>`/`&`（如代码、HTML），必须逐字符转义**——这是最常见的出错点。

## 5. 常见错误清单（AI 自查）

- ❌ 标签不闭合 / 大小写错误（必须是 `outline`/`opml`/`head`/`body`/`title` 全小写，属性名按上表）。
- ❌ 属性值缺引号或引号未转义（值内含 `"` 必须 `&quot;`）。
- ❌ 忘了转义 `&`（`A & B` 必须写 `A &amp; B`）。
- ❌ 把根主题写成 body 下的第一个 outline（根在 `<head><title>`）。
- ❌ 同级节点缩进不一致（前端宽容解析但观感差）。
- ❌ 写入非 UTF-8 编码（保持 UTF-8）。

## 6. 与其它格式的边界

- **本技能处理 `.opml` 与 `.mm` 两类文件**，以及可树状化的源（md 标题层级、目录、嵌套列表等）。
- **写**：新建/保存默认写 `.opml`（本规约）；用户指定思维导图格式时写 `.mm`（见 `freemind-format.md` §3）。
- **读**：`.opml` 按本规约解析（根在 `<head><title>`，`<outline>` 递归嵌套）；`.mm`（FreeMind）宽容解析，只取 `TEXT` 标题与嵌套。
- **转换**：先在 Knowledge Tree 上完成结构，再按目标格式序列化（见 `knowledge-tree-schema.md` §4）。
- `.md` 等其它文件**只作为树状知识的提取来源**，不直接改写。

## 7. 完整示例

```xml
<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head>
    <title>RAG 系统知识树</title>
  </head>
  <body>
    <outline text="架构">
      <outline text="Query 流水线"/>
      <outline text="索引结构"/>
    </outline>
    <outline text="检索">
      <outline text="向量检索"/>
      <outline text="关键词检索（BM25）"/>
    </outline>
    <outline text="重排" note="- 交叉编码器&#10;- 得分融合"/>
    <outline text="评估" COLOR="green">
      <outline text="指标"/>
      <outline text="基准集"/>
    </outline>
  </body>
</opml>
```

> 注：note 内如需换行，可写 `&#10;` 或直接换行文本（前端均能显示）。
