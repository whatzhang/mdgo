# mdgo Knowledge Graph OS — 前端 UI 设计定稿

> 目标：在**不动现有代码**（fileGraph / noteGraph / main.html 全部维持现状）的前提下，
> 以 `graph.html` + 独立 JS/CSS 模块构建下一代「知识资产图谱」前端。
> 渲染引擎：**Sigma.js v3**（百万级节点 WebGL）。后端 `graph_*` 命令为契约先行，前端按契约开发，后端 Phase 1 落地后即通。

---

## 一、总体架构：主页面 iframe 嵌入

```
┌────────────────────────────────────────────────────────────┐
│ main.html（主页面，现状不动）                                 │
│                                                             │
│  toggleFile('graphOS')  ──►  graph-bridge.js                 │
│       │                        │ ① 设 src='./css_js/graph/graph.html'   │
│       ▼                        │ ② switchToView(iframeCommonContainer)  │
│  iframe-common-container  ◄────┘ ③ 视图切换时 destroyEverything()       │
│  └── <iframe id="common-iframe">   └─► cleanupCommonIframe()            │
│         │                                   │ 调用 contentWindow.destroyCommon()
│         ▼                                   ▼
│  ┌─────────────────────────────────────────────────────┐   │
│  │ graph.html（新页面，独立文档上下文）                     │   │
│  │   ├─ graph.css        （独立样式）                     │   │
│  │   ├─ graph-app.js     （入口/组装，暴露 destroyCommon） │   │
│  │   └─ graph/*.js       （SOLID 模块，见下）             │   │
│  └─────────────────────────────────────────────────────┘
└────────────────────────────────────────────────────────────┘
```

### 关键契约（复用 CommonIframe）

| 契约 | 内容 |
|---|---|
| **页面打开** | 主页面 `graph-bridge.js` 暴露 `window.graphBridge.open()`；`toggleFile('graphOS')` 分支调用它；设置 `iframe.src = './css_js/graph/graph.html'` 后 `switchToView(iframeCommonContainer, 'flex')` |
| **页面清理** | graph.html 暴露 `window.destroyCommon()`；主页面现有 `cleanupCommonIframe()`（main.html:33102）已约定调用 `iframe.contentWindow.destroyCommon()`，**零改动复用** |
| **同源访问** | graph.html 与 main.html 同源（`tauri://localhost` 或 `http://localhost:5173`），iframe 内直接 `window.parent.__TAURI__`、`window.parent.getRootHandle()` 复用 Tauri 桥与根目录句柄；**不重复加载适配层** |
| **上下文传递** | 打开时主页面把 `{ dirPath, focusNodeId? }` 写入 `iframe.dataset`（同步可读）；graph.html 启动时从 `window.frameElement.dataset` 读取。后续动态传参走 `postMessage`（见 §五） |
| **构建分发** | graph.html 置于 `css_js/graph/`，vite staticCopy 已复制 `css_js/**` 进 dist —— **零 vite 配置改动** |

> ⚠️ 浏览器（非 Tauri）模式下 `window.parent.__TAURI__` 不存在：graph.html 检测到缺 Tauri 时降级为「演示数据模式」（内置 mock graph），保证页面可独立预览。

---

## 二、文件结构

```
css_js/graph/
├── graph.html            # 图谱页（壳 + 布局 DOM + 契约挂载点）
├── graph.css             # 全部图谱页样式（独立文件）
├── graph-app.js          # 入口：依赖注入组装 + 生命周期（destroyCommon）
├── graph-api.js          # GraphApiClient：graph_* invoke 封装（数据访问）
├── graph-model.js        # 图数据模型（GraphNode/GraphEdge/LodLayer + 校验）
├── graph-store.js        # GraphStore：客户端图缓存 + 局部状态（单一数据源）
├── graph-renderer.js     # Sigma 渲染控制器（视口/LOD/样式，依赖注入 renderer 实现）
├── graph-layout.js       # 布局策略接口 + 内置布局（布局可替换）
├── graph-interaction.js  # 交互控制器（点选/展开/缩放/拖拽）
└── graph-panel.js        # 侧栏面板（节点详情/搜索/统计/关系列表）

css_js/modules/
└── graph-bridge.js       # 主页面桥接（open/close/上下文传递，IIFE 暴露 window.graphBridge）

docs/
└── graph-os-frontend-design.md   # 本文档
```

---

## 三、SOLID 模块设计

> 遵循项目现有模块惯例（`css_js/modules/*.js` IIFE + `window.xxx` 暴露），
> 但 graph 内部采用**显式依赖注入**（`app.js` 组装时传入依赖），模块间不互相 require 全局。

### 模块职责矩阵

| 文件 | 职责（单一职责） | 依赖（注入） | SOLID 落点 |
|---|---|---|---|
| `graph-api.js` | 封装 `graph_*` Tauri 命令为 Promise API；失败降级/超时 | `window.parent.__TAURI__`（注入） | S：唯一数据访问点 |
| `graph-model.js` | `GraphNode`/`GraphEdge`/`LodLayer` 纯数据结构 + 工厂/校验；**无 IO** | 无 | S：纯模型 |
| `graph-store.js` | 已加载节点/边缓存、LOD 状态、选中态、展开历史；发布订阅 | model | S：单一数据源；O：可扩展缓存策略 |
| `graph-renderer.js` | Sigma 实例管理：图数据装载、视口、LOD 切换、主题样式 | Sigma 构造器（注入）、store | D：依赖 Sigma 抽象，不直接 new；O：渲染器实现可替换 |
| `graph-layout.js` | 布局策略接口 `GraphLayout` + 内置实现（force/radial/circle） | graphology 布局（注入） | L：依赖接口；O：新增布局不改调用方 |
| `graph-interaction.js` | 用户交互：点选节点→展开二跳、拖拽、缩放、双击聚焦 | renderer、store、api | S：交互与渲染分离 |
| `graph-panel.js` | 侧栏 UI：详情/搜索/统计/关系列表渲染与事件 | store、api | S：UI 只做展示 |
| `graph-app.js` | 组装全部依赖、启动流程、`window.destroyCommon`、iframe 通信监听 | 全部模块 | S：唯一组装点（Composition Root） |
| `graph-bridge.js` | 主页面侧：open/close、dataset 上下文、postMessage 代理 | main.html 全局（window 注入） | S：主页面与 iframe 的解耦 |

### 关键接口（骨架即定契约）

```js
// graph-api.js —— 后端 graph_* 命令契约（Phase 1 后端实现后原样对齐）
class GraphApiClient {
  constructor({ invoke })                 // invoke = window.parent.__TAURI__.core.invoke
  status(dirPath)                         // → { schemaVersion, nodeCount, edgeCount, building }
  related({ dirPath, nodeId, depth, maxNodes, maxEdges, relationFilter, weightMin })
                                          // → { nodes: GraphNode[], edges: GraphEdge[], truncated: boolean }
  search({ dirPath, keyword, limit })     // → GraphNode[]
  expand({ dirPath, nodeId, depth })      // → { nodes, edges }（二跳展开）
  stats({ dirPath })                      // → { byType: {...}, topDegree: [...], lastBuiltAt }
}

// graph-renderer.js —— 渲染抽象（Sigma 实现注入）
class GraphRenderer {
  constructor({ sigmaFactory, store, container })
  mount()                                 // 创建 Sigma 实例
  setData(nodes, edges)                   // graphology 图装载
  focusNode(nodeId)
  setLod(layer)                           // LOD 层级切换
  setHighlight(nodeId, neighborIds)
  destroy()
}

// graph-layout.js —— 布局策略接口
class GraphLayout {
  constructor({ factory })                // graphology 布局工厂注入
  apply(graph, options)                   // 返回 Promise
  static presets()                        // ['force', 'radial', 'circle']
}
```

---

## 四、Sigma.js v3 渲染与 LOD

### 依赖引入（本地优先，随应用分发）

Sigma v3 + graphology 以**本地 ESM** 形式放入 `css_js/cdn/sigma/`（离线可用，与现有 `css_js/cdn/*` 惯例一致）：

```html
<script type="module">
  import { Sigma } from './sigma/sigma.esm.min.js';
  import Graph from './sigma/graphology.min.js';
  import { forceAtlas2 } from './sigma/forceatlas2.min.js';
</script>
```

> 打包说明：`npm i @sigma/core graphology graphology-layout-forceatlas2` 后取 `dist/` 产物拷入
> `css_js/cdn/sigma/`（构建时由 vite staticCopy 随 `css_js/**` 分发）。骨架阶段 app.js 动态
> `import()` 上述路径，文件缺失时提示「图谱引擎未安装」。

### LOD（Level of Detail）——百万节点架构核心

| 层级 | 触发条件 | 渲染内容 | 数据来源 |
|---|---|---|---|
| L0 概览 | 初始/全图缩放至最小 | 聚类聚合节点（≤5,000 实体） | 后端 `graph_overview`（聚合图） |
| L1 局部 | 缩放进入 | 当前邻域 1-2 跳（≤2,000 节点） | 后端 `graph_related`（按需） |
| L2 焦点 | 点击节点 | 该节点 1 跳全量 + 二跳截断 | 后端 `graph_expand` |

- **前端只渲染视口内 LOD 层，绝不加载全图**（对标现有 fileGraph 的 O(n²) 全量构建，这是架构性差异）；
- Sigma 内置 `camera` 事件驱动 LOD 切换：`graph-renderer.js` 监听 `cameraUpdated` → 按缩放比调 `setLod`；
- 节点样式按类型着色（doc/chunk/entity/experience/memory），大小按 degree 映射（对数刻度）。

---

## 五、iframe 通信协议

```
主页面 graph-bridge.js                    graph.html
─────────────────────                    ─────────────────────
open(dirPath, focus?)  ── dataset ──►  boot() 读 frameElement.dataset
                                      graph-app.init()
                                          ▲
switchToView(...)  ◄──────────────────  ready 事件（postMessage 'graph:ready'）
                                          │
close() ── destroyCommon() 调用 ──►  dispose()（反向销毁：renderer→store→listeners）
```

| 消息 | 方向 | 载荷 | 用途 |
|---|---|---|---|
| `graph:ready` | iframe → 父 | `{ ok, schemaVersion? }` | 主页面确认加载完成（可更新菜单态） |
| `graph:open-node` | iframe → 父 | `{ nodeId, path? }` | 用户点击文件节点 → 主页面打开该文件（复用现有打开链路） |
| `graph:focus-request` | 父 → iframe | `{ nodeId }` | 主页面（如搜索结果/引用跳转）聚焦图谱节点 |
| `graph:refresh` | 父 → iframe | `{}` | watcher 事件后提示图谱增量刷新 |

> 全部经 `window.postMessage` 传递（跨 iframe 安全），graph.html 内统一 `GraphBridge` 小模块监听（骨架内置于 app.js）。

---

## 六、UI 布局设计

```
┌──────────────────────────────────────────────────────────────────┐
│ Toolbar（顶栏）：[图谱类型 ▾] [布局 ▾] [LOD] [搜索框] [刷新] [统计]   │
├────────────┬─────────────────────────────────────────────────────┤
│ 左侧边栏    │                    图谱画布（Sigma）                    │
│ (可折叠)    │                                                     │
│  · 类型过滤  │   ┌─────────────────────────────────────────┐      │
│  · 节点搜索  │   │  (L0/L1/L2 随缩放切换)                    │      │
│  · 统计卡    │   │  节点：彩色圆点（type→色，degree→大小）     │      │
│  · 构建状态  │   │  边：relation→样式（实线/虚线/箭头）        │      │
│            │   └─────────────────────────────────────────┘      │
├────────────┴─────────────────────────────────────────────────────┤
│ 底部状态栏：[节点数] [边数] [LOD] [构建进度] [引擎版本]               │
└──────────────────────────────────────────────────────────────────┘
```

- **右侧浮动详情卡**（点击节点弹出）：节点属性 / 关系列表 / 引用来源 / 「展开二跳」按钮 / 「打开文件」（文件类节点）；
- **空态**：未构建时显示引导（调用后端 `graph_status`，提示「正在后台构建」+ 进度）；
- **主题**：跟随主页面 `--t1/--t2/--t3/--color-primary` CSS 变量（iframe 内定义同名变量继承视觉风格）。

---

## 七、后端 graph_* API 契约（前端先行定义）

| 命令 | 入参 | 返回 | 说明 |
|---|---|---|---|
| `graph_status` | `dirPath` | `{ schema_version, node_count, edge_count, building, progress_pct }` | 图构建状态 |
| `graph_related` | `dirPath, nodeId, depth=2, max_nodes=200, max_edges=400, relations?, weight_min=0.3` | `{ nodes, edges, truncated }` | **邻域查询（L1/L2 数据源）**，后端 BFS + 扇出截断 |
| `graph_expand` | `dirPath, nodeId, depth=1` | `{ nodes, edges }` | 单节点增量展开（点击展开二跳） |
| `graph_search` | `dirPath, keyword, limit=20` | `GraphNode[]` | 节点搜索（name/aliases LIKE） |
| `graph_overview` | `dirPath, max_nodes=5000` | `{ clusters, nodes, edges }` | 聚合概览图（L0 数据源） |
| `graph_stats` | `dirPath` | `{ by_type, top_degree, last_built_at }` | 统计 |

`GraphNode = { id, type, name, path?, meta?, degree? }`；`GraphEdge = { source, target, relation, weight?, confidence? }`

> 前端 `graph-api.js` 已按此契约封装；后端 Phase 1（SQLite Graph Engine）落地后按相同签名实现，
> 前端零改动即可接通。mock 模式（无 Tauri）返回演示数据。

---

## 八、实施约束与验收

1. **现状零改动**：main.html / index.html / fileGraph / noteGraph / vite.config.js 均不动；
   唯一主页面改动 = 引入 `graph-bridge.js` + `toggleFile('graphOS')` 分支（新增，不改既有分支）。
2. **独立文件**：全部新 JS/CSS 为独立文件，graph.html 内不内联业务脚本。
3. **SOLID**：模块矩阵见 §三；组装唯一入口 `graph-app.js`；模块间仅依赖注入的接口。
4. **验收**：
   - `toggleFile('graphOS')` 打开 iframe 图谱页，切走后 `destroyCommon` 被调用且无泄漏；
   - 无 Tauri 时演示数据可渲染；Sigma 引擎缺失时有明确提示；
   - 点击节点 → 详情卡 + 二跳展开；搜索 → 高亮聚焦；LOD 随缩放切换。
