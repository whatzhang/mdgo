# mdgo 下一代 AI 知识图谱 PRD

**文档版本**：V1.0
**产品名称**：mdgo Knowledge Graph
**产品定位**：AI Native Local Knowledge Graph / 本地 AI 知识图谱
**产品形态**：Tauri 2 Desktop Application
**目标用户**：个人开发者、程序员、技术人员、研究人员、知识工作者
**核心目标**：将 mdgo 从“本地文档检索系统”升级为“可理解、可探索、可推理、可持续进化的个人知识图谱系统”。

---

# 1. 产品概述

## 1.1 产品背景

当前 mdgo 已经具备完整的本地知识库基础设施：

* Markdown / HTML / PDF / Code 等多格式文档解析
* Document AST
* Semantic Chunk
* LanceDB 向量检索
* Tantivy BM25
* Hybrid Search
* Reranker
* Watcher 增量同步
* Bookmark / Chat 等知识资产
* 文件关系图
* Markdown Wikilink 图
* AI Agent / GraphRAG 基础能力

但是当前图谱仍然属于：

> “文件关系可视化”

而不是：

> “知识关系可计算、可推理、可被 AI 使用的知识图谱”。

当前图谱存在三个核心问题：

### 问题一：图谱只有“连接”，没有“理解”

当前主要关系：

```text
目录
  ↓
文件

文件
  ↓
WikiLink
  ↓
文件
```

系统不知道：

```text
Redis
属于 → 缓存技术

Redis
解决 → 缓存问题

Bloom Filter
解决 → 缓存穿透

某个项目
使用 → Redis

某篇文章
解释 → Redis 缓存
```

---

### 问题二：图谱无法体现知识结构

当前节点基本是：

```text
文件
目录
```

下一代图谱需要：

```text
Document
Chunk
Concept
Entity
Technology
Project
Problem
Solution
Experience
Decision
Event
Person
```

---

### 问题三：图谱无法真正参与 AI 推理

当前：

```text
用户问题
 ↓
Vector Search
 ↓
BM25
 ↓
Reranker
 ↓
LLM
```

目标：

```text
用户问题
 ↓
Query Understanding
 ↓
Graph Search
 ↓
Hybrid Retrieval
 ↓
Graph Expansion
 ↓
Evidence Ranking
 ↓
Context Assembly
 ↓
LLM
```

因此，本项目的知识图谱不是单独的 UI 功能，而是：

> mdgo AI Knowledge OS 的核心知识智能层。

---

# 2. 产品目标

## 2.1 总目标

构建一个：

> 本地优先、AI 原生、持续进化、可解释、可视化的个人知识图谱。

核心闭环：

```text
文件进入知识库
      ↓
文档理解
      ↓
知识抽取
      ↓
图谱构建
      ↓
知识聚类
      ↓
知识探索
      ↓
GraphRAG
      ↓
AI 推理
      ↓
发现知识关系
      ↓
知识持续进化
```

---

# 3. 产品核心原则

## 3.1 Local First

所有核心知识资产默认保存在本地：

```text
知识库/
└── .mdgo/
    ├── mdgo.db
    ├── lancedb/
    ├── bm25/
    └── graph/
```

AI 分析可以使用本地模型或用户配置的远程模型，但：

> 图谱数据的最终所有权属于用户。

---

## 3.2 AI First

AI 不是“图谱上的一个按钮”。

AI 应该参与：

* 图谱构建
* 实体抽取
* 关系抽取
* 聚类
* 知识摘要
* 关系解释
* 知识缺口发现
* GraphRAG
* 知识演化分析

---

## 3.3 Human + AI Co-building

图谱不要求用户手工维护。

默认：

```text
AI 自动发现
      ↓
生成候选知识
      ↓
计算置信度
      ↓
用户确认 / AI 自动接受
      ↓
进入正式图谱
```

---

## 3.4 渐进式展示

绝不一次加载整个百万节点图谱。

采用：

```text
全局
 ↓
聚类
 ↓
领域
 ↓
节点
 ↓
邻域
 ↓
关系
 ↓
文档
 ↓
Chunk
```

---

# 4. 产品信息架构

知识图谱顶部提供 5 个核心视图。

```text
知识图谱

├── 全局视图
├── 文档关系
├── 主题聚类
├── 领域地图
└── 时间轴
```

---

## 4.1 全局视图

用于：

> 查看整个知识库的知识结构。

核心展示：

```text
根节点
 ↓
知识领域
 ↓
知识簇
 ↓
核心节点
```

默认不是展示所有文件，而是展示：

> Cluster + Core Entity + Major Relationship。

---

## 4.2 文档关系

用于：

> 查看真实文档之间的结构关系。

展示：

```text
Document
Folder
WikiLink
Reference
Import
Dependency
```

适合：

* 文件依赖分析
* 文档关系
* 项目结构
* WikiLink
* Code dependency

---

## 4.3 主题聚类

用于：

> AI 自动发现知识主题。

例如：

```text
AI
├── RAG
├── Embedding
├── Reranker
├── GraphRAG

Java
├── Spring Boot
├── Spring Cloud
├── JVM

Infrastructure
├── Docker
├── Kubernetes
├── Istio
```

---

## 4.4 领域地图

用于：

> 从知识体系角度理解用户掌握的领域。

例如：

```text
Backend
AI
Database
Cloud Native
Frontend
DevOps
```

可以进一步显示：

```text
掌握度
知识量
最近增长
知识缺口
核心知识
```

---

## 4.5 时间轴

用于：

> 查看知识如何形成和演化。

例如：

```text
2025
 |
Spring Boot
 |
RAG
 |
GraphRAG
 |
2026
```

支持：

* 新增知识
* 修改知识
* 知识关系变化
* 技术替代
* 知识增长

---

# 5. 页面总体 UI

参考当前设计稿，最终采用：

> 飞书 / Notion 风格浅色桌面 UI。

整体结构：

```text
┌─────────────────────────────────────────────────────────────┐
│ Logo │ 全局视图 文档关系 主题聚类 领域地图 时间轴 │ 搜索 AI │
├──────────────┬──────────────────────────────┬───────────────┤
│              │                              │               │
│  图谱概览     │                              │   节点详情     │
│              │                              │               │
│  分类过滤     │        Knowledge Graph       │   Overview    │
│              │                              │   Content     │
│  关系过滤     │                              │   Relations   │
│              │                              │   Analysis    │
│  布局模式     │                              │               │
│              │                              │               │
│  AI 分析      │                              │               │
│              │                              │               │
├──────────────┴──────────────────────────────┴───────────────┤
│ Legend                                  MiniMap / Controls  │
└─────────────────────────────────────────────────────────────┘
```

---

# 6. 顶部导航

## 6.1 Logo

左侧：

```text
知识图谱
KNOWLEDGE GRAPH
```

点击：

返回知识库主界面。

---

## 6.2 一级视图

```text
全局视图
文档关系
主题聚类
领域地图
时间轴
```

当前选中项使用：

* 浅蓝背景
* 蓝色文字
* 底部蓝色指示线

---

## 6.3 全局搜索

搜索框：

```text
搜索节点、文档、概念...
```

快捷键：

```text
⌘K / Ctrl+K
```

支持搜索：

```text
文件
实体
概念
技术
项目
关系
标签
```

搜索结果分组：

```text
节点
文档
主题
关系
```

---

## 6.4 AI 助手

右上角：

```text
AI 助手
```

点击打开 AI Graph Assistant。

支持：

```text
这个知识库主要有哪些领域？

Redis 和 Kafka 有什么关系？

我关于 RAG 的知识体系完整吗？

哪些知识存在冲突？

最近增长最快的知识领域是什么？
```

---

# 7. 左侧面板

## 7.1 图谱概览

展示：

```text
总节点数
总边数
```

例如：

```text
1,351 节点
2,847 关系
```

第二行：

```text
文档
概念
代码
实体
```

点击统计项可以直接过滤图谱。

---

# 8. 分类过滤

分类不再只支持：

```text
文档
目录
```

升级为：

```text
全部
文档
目录
代码
配置
图片
概念
实体
脚本
项目
其他
```

每类显示数量。

---

## 8.1 分类颜色

推荐固定颜色语义：

```text
目录     蓝色
文档     黄色
代码     橙色
配置     绿色
图片     粉色
概念     紫色
脚本     青色
实体     灰蓝
```

颜色只用于：

* 节点
* 图例
* Filter
* Badge

不能同时承担其他语义。

---

# 9. 关系类型过滤

关系类型：

```text
包含
引用
依赖
实现
相关
派生
属于
解决
使用
替代
产生
```

每种关系显示：

```text
名称
数量
颜色
```

点击后：

> 只显示该类型关系。

---

# 10. 布局模式

提供四种布局：

```text
力导向
层级
放射
聚类
```

---

## 10.1 力导向

适合：

> 自由探索关系。

---

## 10.2 层级

适合：

```text
目录
领域
知识体系
```

---

## 10.3 放射

适合：

> 查看某个中心节点的关系。

例如：

```text
              Redis
                |
      ┌─────────┼─────────┐
      ↓         ↓         ↓
    Cache     Bloom      DB
```

---

## 10.4 聚类

默认推荐。

核心目标：

> 解决当前“节点全部挤在一起”的问题。

---

# 11. 聚类布局核心算法

这是本次 UI 改造最重要的部分。

当前：

```text
forceManyBody
```

导致：

```text
所有节点
 ↓
全部参与物理模拟
 ↓
节点互相挤压
 ↓
形成中心大团
```

必须改为：

> Cluster-aware Layout。

---

## 11.1 第一级：发现 Cluster

Cluster 来源优先级：

### Level 1

已有目录结构。

### Level 2

文档主题。

### Level 3

Embedding 聚类。

### Level 4

LLM Semantic Clustering。

最终得到：

```text
Cluster A
Cluster B
Cluster C
...
```

---

## 11.2 第二级：Cluster 布局

不要让所有节点互相排斥。

先布局：

```text
Cluster

Cluster

Cluster

Cluster
```

例如：

```text
        Web
          ○○○

Python ○○○      Java ○○○


       Root

AI ○○○           Docs ○○○


       Tools
        ○○○
```

---

## 11.3 第三级：Cluster 内部布局

每个 Cluster 内部再执行：

```text
Force Layout
```

但：

> Force 只作用于 Cluster 内部。

---

## 11.4 Cluster 之间

使用：

```text
Cluster Repulsion
```

保持足够间距。

---

## 11.5 Cluster 核心节点

每个 Cluster 选出：

```text
中心节点
```

计算：

```text
centrality
degree
PageRank
semantic importance
```

显示为：

```text
AI & 机器学习

98 个节点
```

---

# 12. 中央图谱

中央区域是整个产品的核心。

---

## 12.1 默认显示

默认：

```text
Cluster

+
Cluster Core Node

+
Cluster Relationship

+
重要节点
```

不直接加载百万文件节点。

---

## 12.2 LOD 分层

### L0

仅显示 Cluster。

```text
AI
Java
Python
Docs
```

---

### L1

显示：

```text
Cluster
+
核心节点
```

---

### L2

显示：

```text
Cluster
+
重要节点
+
关键文件
```

---

### L3

显示完整邻域。

---

### L4

显示：

```text
Chunk
+
Entity
+
细粒度关系
```

默认：

```text
L1
```

---

# 13. 节点设计

节点分为：

## 普通节点

小圆点。

---

## 重要节点

较大圆点。

---

## Cluster Node

使用：

```text
Icon
+
名称
+
节点数量
```

例如：

```text
┌──────────────┐
│  AI 图标      │
│              │
│ AI & 机器学习 │
│ 98 个节点     │
└──────────────┘
```

---

# 14. 节点交互

## 14.1 Hover

显示 Tooltip：

```text
名称
类型
关系数
所属 Cluster
来源
```

不打开右侧面板。

---

## 14.2 单击

打开右侧：

```text
节点详情
```

并：

```text
高亮节点
高亮邻居
弱化其他节点
```

---

## 14.3 双击

进入：

> 节点聚焦模式。

中心化：

```text
当前节点
 ↓
1-hop
 ↓
2-hop
```

---

## 14.4 右键

Context Menu：

```text
查看详情
打开文档
展开关系
查看邻居
加入收藏
AI 分析
生成摘要
查找相关知识
隐藏节点
设为中心
```

---

# 15. 边设计

不同关系使用不同视觉：

```text
引用       实线
依赖       实线 + 箭头
包含       层级线
相关       虚线
派生       双向
替代       虚线箭头
```

边默认弱化。

只有：

> Hover / Select / AI 推理

时增强显示。

---

# 16. 节点详情面板

右侧面板：

```text
节点详情
```

---

## 16.1 Header

显示：

```text
Icon

AI & 机器学习

概念聚类

98 个节点
156 条边
```

---

## 16.2 Tab

```text
概览
内容
关系
分析
```

---

# 17. 概览 Tab

## 描述

AI 自动生成：

```text
该知识簇主要包含机器学习、
深度学习、自然语言处理、
AI 工程等相关知识。
```

---

## 主要内容

Tags：

```text
机器学习算法
深度学习模型
NLP
数据预处理
模型训练
推理部署
```

---

## 关键文件

展示 Top 5：

```text
Transformer模型详解.md
BERT实现原理.py
数据预处理工具.py
模型训练配置.yaml
实验结果分析.md
```

排序依据：

```text
相关度
访问/引用关系
中心性
最近修改时间
```

---

# 18. 内容 Tab

显示：

```text
节点关联文档
```

支持：

* 搜索
* 排序
* 类型过滤
* 时间过滤

每个文档：

```text
文件名
路径
摘要
相关度
修改时间
```

点击：

> 打开原始文档。

---

# 19. 关系 Tab

显示：

```text
入边
出边
双向关系
```

例如：

```text
AI & 机器学习

使用
 ↓
PyTorch

相关
 ↓
Transformer

包含
 ↓
NLP
```

点击关系：

> 图谱自动聚焦到关系两端。

---

# 20. 分析 Tab

AI 分析结果：

```text
知识密度
知识中心性
知识增长
知识缺口
知识冲突
```

例如：

```text
AI & 机器学习

知识规模：98
核心度：92
增长率：+24%

发现：

RAG 相关知识较丰富。

但：

Evaluation
知识明显不足。
```

---

# 21. AI 图谱助手

这是整个产品区别于传统知识图谱的核心功能。

---

## 21.1 AI 助手入口

支持：

```text
AI 助手
```

---

## 21.2 推荐问题

根据当前图谱自动生成：

```text
这个知识库主要有哪些知识领域？

哪些知识领域之间联系最紧密？

我的知识体系有哪些缺口？

哪些知识已经过时？

最近新增了哪些重要知识？

哪些概念存在重复？
```

---

# 22. Graph AI Query

用户输入：

> Redis 和 Kafka 有什么关系？

系统：

```text
Query
 ↓
Intent
 ↓
Entity Detection
 ↓
Graph Search
 ↓
Hybrid Search
 ↓
Evidence
 ↓
LLM
```

回答：

```text
Redis 和 Kafka 都属于基础设施组件。

Redis：
主要用于缓存、KV存储。

Kafka：
主要用于消息流和事件处理。

你的知识库中二者存在 7 个关联项目，
其中 3 个项目同时使用了 Redis 和 Kafka。
```

---

# 23. GraphRAG

现有：

```text
Vector
+
BM25
+
Reranker
```

增加：

```text
Graph Expansion
```

最终：

```text
Query
 ↓
Intent
 ↓
Vector Search
 ↓
BM25
 ↓
Graph Search
 ↓
RRF
 ↓
Reranker
 ↓
Graph Expansion
 ↓
Context Builder
 ↓
LLM
```

---

# 24. Graph Search

后端提供：

```text
get_node()
get_neighbors()
get_subgraph()
find_path()
find_related()
find_common_neighbors()
```

例如：

```text
find_path(
  Redis,
  Kubernetes
)
```

返回：

```text
Redis
 ↓
Cache
 ↓
Application
 ↓
Docker
 ↓
Kubernetes
```

---

# 25. AI 自动建图

这是图谱的核心后台业务。

---

## 25.1 文件进入

```text
Watcher
 ↓
Document Parser
 ↓
AST
 ↓
Semantic Chunk
```

分叉：

```text
             Document
                 |
       ┌─────────┴─────────┐
       ↓                   ↓
Vector Index          Graph Builder
```

---

# 26. Graph Builder

分四级。

## Level 1：结构关系

无需 LLM。

直接解析：

```text
目录
WikiLink
Markdown Link
代码 import
代码 symbol
OPML
```

---

## Level 2：语义关系

使用：

```text
Embedding
```

发现：

```text
相似
同主题
同领域
```

---

## Level 3：实体关系

使用 LLM：

```text
Entity
Relation
Concept
```

---

## Level 4：经验关系

LLM 提取：

```text
Problem
Decision
Solution
Result
Experience
```

---

# 27. AI 抽取结果必须有置信度

每条 AI 关系：

```text
confidence
```

例如：

```text
Redis
 ── used_for ──>
Cache

confidence: 0.94
source:
redis-cache.md
```

---

# 28. AI 关系不能直接覆盖用户事实

关系状态：

```text
candidate
confirmed
rejected
auto_confirmed
```

规则：

```text
confidence >= 0.90
+
来源可信
+
关系明确
```

可以自动确认。

否则：

> 进入待确认队列。

---

# 29. AI 智能分析

左侧增加：

```text
智能分析
```

例如：

```text
发现 12 个知识聚类

建议查看：

AI 相关文档聚类
```

---

## 分析类型

### 主题发现

```text
发现新知识领域
```

### 知识缺口

```text
RAG

已有：

Embedding
Vector DB
Retriever
Reranker

缺少：

Evaluation
Benchmark
```

### 知识冲突

例如：

```text
文档 A：

使用 Elasticsearch。


文档 B：

已迁移到 OpenSearch。
```

AI：

> 检测到潜在知识冲突。

---

# 30. 知识演化

每个节点记录：

```text
created_at
updated_at
first_seen
last_seen
```

关系记录：

```text
created_at
confidence
source
```

支持：

```text
时间轴
```

例如：

```text
2024
Redis

2025
Redis + Kafka

2026
Redis → Valkey
```

---

# 31. Knowledge Evolution AI

AI 自动发现：

```text
知识新增
知识衰减
知识替代
知识冲突
知识增长
```

例如：

> “你的 Kubernetes 知识在过去 3 个月增长 42%，但 Service Mesh 相关知识没有变化。”

---

# 32. 数据模型

新增：

```text
graph_nodes
graph_edges
graph_clusters
graph_node_sources
graph_ai_candidates
```

---

## graph_nodes

```sql
CREATE TABLE graph_nodes (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    metadata JSON,
    source_type TEXT,
    source_id TEXT,
    confidence REAL,
    created_at INTEGER,
    updated_at INTEGER
);
```

---

## graph_edges

```sql
CREATE TABLE graph_edges (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    weight REAL,
    confidence REAL,
    source_id TEXT,
    status TEXT,
    created_at INTEGER,
    updated_at INTEGER
);
```

---

## graph_clusters

```sql
CREATE TABLE graph_clusters (
    id TEXT PRIMARY KEY,
    name TEXT,
    description TEXT,
    algorithm TEXT,
    centroid TEXT,
    node_count INTEGER,
    confidence REAL,
    created_at INTEGER,
    updated_at INTEGER
);
```

---

# 33. 图谱构建增量机制

绝对禁止：

```text
修改一个 Markdown
 ↓
重新构建整个图谱
```

必须：

```text
File Changed
 ↓
找到旧 Node
 ↓
删除旧 Source Relations
 ↓
重新解析
 ↓
重新抽取
 ↓
Merge
```

---

# 34. Graph Builder Worker

Rust 后端新增：

```text
core/graph/

├── model.rs
├── storage.rs
├── builder.rs
├── extractor.rs
├── merger.rs
├── cluster.rs
├── query.rs
├── evolution.rs
└── worker.rs
```

---

# 35. Graph Build 状态机

```text
PENDING
 ↓
PARSING
 ↓
EXTRACTING
 ↓
MERGING
 ↓
CLUSTERING
 ↓
READY
```

异常：

```text
FAILED
```

支持：

```text
retry
```

---

# 36. 前后端 API

## 获取图谱概览

```text
GET graph/stats
```

返回：

```json
{
  "nodes": 1351,
  "edges": 2847,
  "clusters": 12
}
```

---

## 获取 Cluster

```text
GET graph/clusters
```

---

## 获取 Cluster 子图

```text
GET graph/cluster/:id
```

---

## 获取邻居

```text
GET graph/node/:id/neighbors?depth=1
```

---

## 获取节点详情

```text
GET graph/node/:id
```

---

## 路径查询

```text
GET graph/path?source=A&target=B
```

---

## GraphRAG

```text
POST graph/query
```

请求：

```json
{
  "query": "Redis和Kafka有什么关系？",
  "depth": 2,
  "top_k": 20
}
```

---

# 37. 前端与后端职责

## Rust 后端负责

```text
文件扫描
图谱存储
图谱构建
实体抽取
关系抽取
聚类
Graph Query
GraphRAG
增量更新
```

---

## 前端负责

```text
Graph Rendering
LOD
Viewport
交互
节点选择
Cluster 展开
动画
筛选
详情面板
```

原则：

> 前端不再持有完整知识图谱。

---

# 38. 前端图谱性能架构

当前：

```text
全部节点
 ↓
前端内存
 ↓
ForceSimulation
 ↓
Canvas
```

改为：

```text
Backend Graph Store
 ↓
Cluster API
 ↓
Viewport Graph
 ↓
WebGL Renderer
```

---

# 39. 百万节点策略

绝对不：

```text
1,000,000 nodes
 ↓
Browser
```

默认：

```text
100 clusters
```

用户点击：

```text
Cluster
 ↓
加载 100~500 节点
```

继续点击：

```text
Node
 ↓
加载邻域
```

---

# 40. LOD 策略

```text
Zoom < 0.5
→ Cluster

0.5 ~ 1
→ Cluster + Core Node

1 ~ 2
→ Important Node

> 2
→ Full Node
```

---

# 41. 图谱缓存

前端缓存：

```text
clusterCache
neighborCache
nodeCache
layoutCache
```

缓存 Key：

```text
graphVersion
+
clusterId
+
depth
```

Watcher 修改图谱后：

```text
graphVersion++
```

旧缓存自动失效。

---

# 42. 图谱版本

后端维护：

```text
graph_version
```

每次：

```text
graph mutation
```

版本：

```text
v100
→
v101
```

前端请求：

```text
If-Version
```

避免缓存不一致。

---

# 43. MiniMap

右下角显示：

```text
全局 Cluster 分布
```

MiniMap 不显示所有节点。

只显示：

```text
Cluster
```

点击：

> 主图跳转。

---

# 44. 图谱工具栏

右侧：

```text
定位
+
-
适应窗口
```

增加：

```text
聚焦
撤销
重置
```

---

# 45. 搜索定位

输入：

```text
Redis
```

系统：

```text
搜索结果
 ↓
定位节点
 ↓
Zoom In
 ↓
高亮
 ↓
右侧打开详情
```

---

# 46. 节点打开文档

点击：

```text
查看全部 98 个文件
```

进入：

> 文件列表。

双击文件：

> 回到编辑器 / Markdown Viewer。

并自动定位到：

> 与当前知识节点最相关的段落。

---

# 47. “来源证据”机制

这是 AI 知识图谱必须具备的功能。

任何 AI 关系都必须能追溯：

```text
关系：

Redis
used_for
Cache

来源：

redis.md
第 32 行
```

用户点击：

> 查看证据。

跳转原文。

原则：

> AI 结论必须可解释。

---

# 48. 用户手工编辑图谱

支持：

```text
创建节点
删除节点
创建关系
删除关系
修改名称
修改类型
添加标签
```

用户创建的关系：

```text
source = user
confidence = 1.0
status = confirmed
```

AI 不允许覆盖。

---

# 49. AI 与用户关系冲突

如果：

```text
用户：

A → B
```

AI：

```text
A → C
```

不能自动删除 A→B。

显示：

```text
检测到潜在关系冲突
```

让用户选择：

```text
保留 A→B
接受 A→C
两者并存
忽略 AI 建议
```

---

# 50. 图谱收藏

节点支持：

```text
收藏
```

形成：

```text
My Knowledge
```

后续可用于：

* AI 上下文
* 快速访问
* 首页推荐

---

# 51. AI 自动推荐

根据：

```text
当前节点
+
用户知识
+
访问记录
+
Graph
```

推荐：

```text
你可能还需要了解：

RAG Evaluation

因为它与你当前的：

RAG
Retriever
Reranker

存在较强关系。
```

---

# 52. 知识缺口分析

核心业务能力。

算法：

```text
当前知识 Cluster
 ↓
Graph Topology
 ↓
Embedding Similarity
 ↓
External / Internal Concept
 ↓
Gap Detection
```

结果：

```text
RAG

已覆盖：
Embedding
Retriever
Reranker
Vector DB

缺失：
Evaluation
Observability
Benchmark
```

---

# 53. 知识重复检测

检测：

```text
概念 A
≈
概念 B
```

例如：

```text
Redis Cache

Redis缓存
```

AI 建议：

> 检测到两个高度相似概念，建议合并。

---

# 54. 知识冲突检测

例如：

```text
文档 A：

Redis 是单线程。


文档 B：

Redis 使用多线程。
```

系统：

```text
Conflict Candidate
```

AI 分析上下文：

> 两个说法可能分别对应不同版本 / 不同执行阶段。

最终：

```text
事实
+
版本
+
来源
```

---

# 55. 未来 Experience Graph

第二阶段之后加入：

```text
Problem
Decision
Solution
Result
Experience
```

例如：

```text
问题
 ↓
Redis 缓存击穿

方案
 ↓
Bloom Filter

实现
 ↓
Project A

结果
 ↓
QPS 提升
```

这将成为 mdgo 与普通知识图谱最大的产品差异。

---

# 56. AI Agent 使用知识图谱

最终：

```text
Agent
 ↓
Knowledge Graph
 ↓
Personal Context
 ↓
Experience
 ↓
Reasoning
```

Agent 可以调用：

```text
get_user_context()
find_related_knowledge()
find_previous_experience()
find_solution()
find_evidence()
reason_over_graph()
```

---

# 57. 产品核心壁垒

最终形成：

```text
Document Graph
       +
Semantic Graph
       +
Entity Graph
       +
Experience Graph
       +
Memory Graph
       +
Evolution Graph
```

而不是简单：

```text
File Graph
```

---

# 58. MVP 范围

第一版本不要全部实现。

## P0

### 后端

* [ ] SQLite Graph Store
* [ ] graph_nodes
* [ ] graph_edges
* [ ] Document Node
* [ ] Directory Node
* [ ] Chunk Node
* [ ] WikiLink Relation
* [ ] Contains Relation
* [ ] Reference Relation
* [ ] 增量 Graph Builder
* [ ] Graph Query API

### 前端

* [ ] 全局视图
* [ ] Cluster View
* [ ] LOD
* [ ] 节点详情
* [ ] 节点搜索
* [ ] 节点聚焦
* [ ] Cluster 展开
* [ ] Filter
* [ ] MiniMap
* [ ] 浅色 UI

---

# 59. P1

* [ ] Embedding Cluster
* [ ] Entity Extract
* [ ] Relation Extract
* [ ] AI Cluster Summary
* [ ] GraphRAG
* [ ] AI Graph Assistant
* [ ] 来源证据
* [ ] 知识缺口
* [ ] 知识冲突
* [ ] 知识重复

---

# 60. P2

* [ ] Experience Graph
* [ ] Memory Graph
* [ ] Knowledge Evolution
* [ ] Personal Knowledge Map
* [ ] Agent Memory
* [ ] MCP Graph API
* [ ] AI 主动知识推荐
* [ ] 自动知识维护

---

# 61. 第一阶段技术实施顺序

不要同时开发所有能力。

严格按照：

```text
Step 1
修复现有索引硬伤
A1
A2
A3

↓

Step 2
Graph Storage

↓

Step 3
Document Graph

↓

Step 4
Incremental Graph Builder

↓

Step 5
Cluster Engine

↓

Step 6
Cluster-aware Visualization

↓

Step 7
LOD

↓

Step 8
Node Detail

↓

Step 9
Graph Query

↓

Step 10
GraphRAG

↓

Step 11
AI Entity Graph

↓

Step 12
Experience Graph
```

---

# 62. 当前 UI 的具体改造要求

## 必须删除

当前：

```text
顶部老式 Select
```

改为：

```text
Segmented Control
```

---

## 必须删除

大面积深色背景。

全部改为：

```text
#FFFFFF
#F7F8FA
#F2F4F7
```

---

## 边框

不要重边框。

使用：

```text
1px #E5E7EB
```

---

## 阴影

使用极轻：

```text
0 2px 8px rgba(...)
```

---

## 圆角

统一：

```text
6px
8px
10px
```

不要大量使用：

```text
16px+
```

---

# 63. UI 视觉语言

整体：

> 飞书 / Notion / Linear 风格。

关键词：

```text
Light
Clean
Professional
Information Dense
Soft
Modern
AI Native
```

而不是：

```text
Cyberpunk
Neon
Dark
Glow
Gaming
```

---

# 64. 图谱颜色原则

使用柔和颜色：

```text
蓝
紫
青
绿
橙
粉
黄
```

降低饱和度。

节点：

```text
实色
```

边：

```text
低透明度
```

Cluster：

```text
非常浅的透明背景
```

避免大量发光效果。

---

# 65. 视觉层级

必须遵循：

```text
Cluster
   ↓
Core Node
   ↓
Important Node
   ↓
Normal Node
   ↓
Edge
```

视觉重要程度：

```text
Cluster > Node > Edge
```

当前图谱最大问题之一：

> 所有节点视觉权重相同。

必须解决。

---

# 66. 图谱默认状态

打开知识图谱：

```text
Loading
 ↓
读取 graph stats
 ↓
加载 clusters
 ↓
显示 Cluster
 ↓
加载核心节点
 ↓
完成
```

不能：

```text
Loading
 ↓
读取 100 万节点
 ↓
浏览器卡死
```

---

# 67. 空状态

没有图谱：

```text
还没有建立知识图谱

你的知识库中已有：

883 个文档
1,351 个知识资产

立即构建知识图谱

[开始构建]
```

---

# 68. 构建状态

显示：

```text
正在构建知识图谱

文档解析       ✓
结构关系       ✓
语义分析       63%
实体抽取       31%
主题聚类       等待

已处理：
3,821 / 6,421
```

支持：

```text
暂停
取消
后台运行
```

---

# 69. 构建失败

不能只显示：

```text
构建失败
```

应该：

```text
知识图谱构建遇到问题

失败阶段：
实体抽取

原因：
AI 模型不可用

已完成：
文档关系
主题聚类

[重试]
[跳过 AI 抽取]
```

---

# 70. 性能指标

第一阶段目标：

### 10 万节点

```text
初始打开 < 2s
Cluster 展开 < 300ms
节点搜索 < 100ms
邻域展开 < 300ms
```

### 100 万节点

要求：

```text
全局视图可用
```

但：

> 只加载 Cluster，不加载全部节点。

---

# 71. Graph Rendering 指标

目标：

```text
LOD Cluster：
60 FPS

500 节点：
60 FPS

2,000 节点：
≥45 FPS

10,000 节点：
允许降低 FPS，但不能阻塞 UI
```

---

# 72. 数据一致性

Watcher：

```text
File Changed
 ↓
Index Update
 ↓
Graph Update
 ↓
graph_version++
 ↓
Frontend invalidate cache
```

必须解决当前：

> refreshTree 与 graph cache 不联动。

---

# 73. Graph Version

所有图谱查询返回：

```json
{
  "graph_version": 123
}
```

前端缓存：

```text
graph_version = 123
```

如果发现：

```text
124
```

自动：

```text
invalidate
reload
```

---

# 74. 可观测性

Graph Builder 需要统计：

```text
节点数量
边数量
Cluster 数量
构建耗时
LLM 调用次数
LLM Token
失败数量
待确认关系
```

---

# 75. AI 成本控制

不能：

```text
500,000 files
 ×
LLM extraction
```

否则成本不可接受。

采用：

```text
Rule
 ↓
Embedding
 ↓
Candidate Selection
 ↓
LLM
```

只让高价值内容进入 LLM。

---

# 76. AI 优先级

优先处理：

```text
高引用文档
高中心性节点
用户打开频率高的文档
核心项目
新产生文档
用户主动查询相关文档
```

低价值：

```text
临时文件
日志
缓存
构建产物
```

默认不进入 AI Graph。

---

# 77. 安全原则

AI 抽取不能修改原始文件。

所有 AI 结果进入：

```text
Graph Store
```

而不是修改 Markdown。

用户确认后才能：

```text
写入 Markdown
```

---

# 78. 验收标准

## UI

* [ ] 全部使用浅色主题
* [ ] 符合飞书 / Notion 风格
* [ ] 图谱不再形成中心大团
* [ ] Cluster 明确分离
* [ ] 节点视觉层级清晰
* [ ] 支持 Cluster 展开
* [ ] 支持节点聚焦
* [ ] 支持搜索定位
* [ ] 支持右侧详情
* [ ] 支持 MiniMap
* [ ] 支持 LOD

---

## 图谱

* [ ] Document Graph 正常构建
* [ ] WikiLink 自动生成关系
* [ ] Directory 自动生成关系
* [ ] 文件变更能够增量更新
* [ ] 删除文件同步删除节点
* [ ] 图谱缓存自动失效
* [ ] graph_version 正常工作

---

## AI

* [ ] AI 可以生成 Cluster
* [ ] AI 可以生成摘要
* [ ] AI 可以抽取实体
* [ ] AI 可以抽取关系
* [ ] AI 关系包含 confidence
* [ ] AI 关系可以追溯来源
* [ ] GraphRAG 可以使用图谱
* [ ] AI 可以回答图谱关系问题

---

# 79. 最终产品形态

最终 mdgo 的知识图谱不应该只是：

```text
                    ●
                ●       ●
             ●             ●
          ●                   ●
```

而应该是：

```text
                  ┌─────────────┐
                  │  Web 开发    │
                  │  ● ● ● ●    │
                  │ ● ● ◎ ● ●   │
                  └──────┬──────┘
                         │
                         │
 ┌─────────────┐      ┌──▼──────┐      ┌─────────────┐
 │ Python      │──────│  根目录  │──────│ Java        │
 │ ● ● ◎ ●     │      │  ◎      │      │ ● ● ◎ ●     │
 │ ● ● ● ●     │      └──┬──────┘      └─────────────┘
 └─────────────┘         │
                         │
                ┌────────▼──────┐
                │ AI & 机器学习  │
                │ ● ● ◎ ● ●     │
                │ ● ● ● ●       │
                └───────────────┘
```

用户看到的不是：

> “883 个文件的关系。”

而是：

> “我的知识世界由哪些领域构成，这些领域如何连接，哪些知识是核心，哪些知识正在增长，哪里存在缺口，以及 AI 能从这些知识中推导出什么。”

---

# 80. 最终产品战略

mdgo 的知识图谱最终形成六层能力：

```text
                 AI Knowledge Agent
                         ↑
                         │
                 Knowledge Reasoning
                         ↑
                         │
                Experience / Memory
                         ↑
                         │
                 Entity / Concept
                         ↑
                         │
                Semantic Document Graph
                         ↑
                         │
                Document / Chunk Graph
                         ↑
                         │
             LanceDB + BM25 + AST
```

因此：

**第一阶段**解决“看得懂”。

**第二阶段**解决“连得上”。

**第三阶段**解决“问得明白”。

**第四阶段**解决“推得出来”。

**第五阶段**解决“记得住”。

**最终阶段**解决：

> **让 AI 真正拥有一个属于用户自己的知识世界。**

这也是 mdgo 与 Obsidian、Notion、Neo4j、普通 GraphRAG 产品之间最值得建立的长期差异化。

# 81. 开发优先级总表

| 模块                  | 优先级 | 目标              |
| ------------------- | --: | --------------- |
| A1-A3 索引可靠性         |  P0 | 支撑大规模知识库        |
| Graph Store         |  P0 | 建立持久化图谱         |
| Document Graph      |  P0 | 第一层真实图谱         |
| Incremental Builder |  P0 | 支持实时知识更新        |
| Cluster Engine      |  P0 | 解决节点挤压          |
| Cluster UI          |  P0 | 下一代图谱视觉基础       |
| LOD                 |  P0 | 支持大规模数据         |
| Node Detail         |  P0 | 完成图谱交互闭环        |
| Graph Query         |  P0 | 图谱成为可查询基础设施     |
| GraphRAG            |  P1 | 图谱进入 AI         |
| Entity Graph        |  P1 | AI 理解知识         |
| AI Summary          |  P1 | 自动解释知识          |
| Knowledge Gap       |  P1 | AI 分析知识体系       |
| Conflict Detection  |  P1 | AI 维护知识质量       |
| Experience Graph    |  P2 | 建立核心壁垒          |
| Memory Graph        |  P2 | AI 长期记忆         |
| Knowledge Evolution |  P2 | 知识持续进化          |
| Agent / MCP         |  P2 | 图谱成为 Agent 基础设施 |

---

# 82. 一句话产品定义

> **mdgo Knowledge Graph 不是“把文件画成图”，而是把用户的文件、知识、概念、实体、经验和 AI 推理统一成一个可持续进化的本地知识世界。**
