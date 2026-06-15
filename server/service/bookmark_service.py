import os
import json
import uuid
import re
import random

import logging
logger = logging.getLogger(__name__)

try:
    import fcntl
    _HAS_FCNTL = True
except ImportError:
    _HAS_FCNTL = False
import requests
import jieba
from time import sleep
from concurrent.futures import ThreadPoolExecutor, as_completed
from bs4 import BeautifulSoup
from datetime import datetime
import trafilatura
from service.config import (
    DATA_DIR, BOOKMARKS_JSONL, TAGS_JSONL, RELATIONS_JSONL,
    INVERTED_JSON, EMBEDDINGS_JSON, GRAPH_NODES_JSON,
    LOCAL_LLM_API, LOCAL_LLM_API_TOKEN, LOCAL_LLM_API_MODEL,
    LOCAL_EMBEDDING_API, LOCAL_EMBEDDING_API_TOKEN, LOCAL_EMBEDDING_API_MODEL
)

os.makedirs(DATA_DIR, exist_ok=True)


def read_jsonl(filepath):
    items = []
    if os.path.exists(filepath):
        with open(filepath, "r", encoding="utf-8") as f:
            if _HAS_FCNTL:
                fcntl.flock(f.fileno(), fcntl.LOCK_SH)
            try:
                for line in f:
                    line = line.strip()
                    if line:
                        items.append(json.loads(line))
            finally:
                if _HAS_FCNTL:
                    fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    return items


def write_jsonl(filepath, items):
    tmp_path = filepath + ".tmp"
    with open(tmp_path, "w", encoding="utf-8") as f:
        if _HAS_FCNTL:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            for item in items:
                f.write(json.dumps(item, ensure_ascii=False) + "\n")
        finally:
            if _HAS_FCNTL:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    os.replace(tmp_path, filepath)


def append_jsonl(filepath, item):
    with open(filepath, "a", encoding="utf-8") as f:
        if _HAS_FCNTL:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            f.write(json.dumps(item, ensure_ascii=False) + "\n")
        finally:
            if _HAS_FCNTL:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)


def write_json_atomic(filepath, data):
    tmp_path = filepath + ".tmp"
    with open(tmp_path, "w", encoding="utf-8") as f:
        if _HAS_FCNTL:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            json.dump(data, f, ensure_ascii=False)
        finally:
            if _HAS_FCNTL:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    os.replace(tmp_path, filepath)


def prompt_build(prompt: str):
    return {
        "model": LOCAL_LLM_API_MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "bookmark",
                "strict": True,
                "schema": {
                    "type": "object",
                    "properties": {
                        "summary": {"type": "string"},
                        "category": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "score": {"type": "integer"}
                    },
                    "required": ["summary", "category", "tags", "score"],
                    "additionalProperties": False
                }
            }
        }
    }


def _call_openclaw_api(payload, timeout=120):
    headers = {
        'User-Agent': 'PostmanRuntime/7.26.8',
        "Content-Type": "application/json",
        "Authorization": f"Bearer {LOCAL_LLM_API_TOKEN}"
    }
    try:
        resp = requests.post(LOCAL_LLM_API, json=payload,
                             headers=headers, timeout=timeout)
        resp.raise_for_status()
        result = resp.json()
        ai_content = result["choices"][0]["message"]["content"]
        data = json.loads(ai_content)
        return True, data
    except Exception:
        return False, None


def call_openclaw_ai_by_content(title, content):
    prompt = f"""你是一个信息结构化引擎，负责将文章转换为标准化 JSON 数据。

⚠️ 这是一个严格结构化任务：
- 只允许输出 JSON
- 不允许输出解释、注释、markdown、换行说明
- 输出必须可被 JSON.parse 直接解析

====================
【任务】
对文章进行：摘要 + 分类 + 标签 + 评分
====================

【输出格式】
{{
  "summary": string,
  "category": string,
  "tags": string[],
  "score": number
}}

====================
【字段规则】
====================

1. summary
- 80~120字
- 提炼核心观点 + 结论
- 禁止复述段落
- 禁止无信息句

2. category
- 必须从以下枚举中选择一个（严格匹配）：
["技术","学习","工具","新闻","娱乐","社交","生活","购物","金融","工作","政务","资源","搜索","创作","其他"]

【分类判定（强约束）】
- 技术：编程/AI/系统/工程实现
- 学习：教程/课程/知识讲解
- 工具：软件/平台/效率工具
- 新闻：最新事件/行业动态（优先级最高）
- 娱乐：视频/音乐/游戏
- 社交：社区/论坛/互动平台
- 生活：非技术日常/经验/观点
- 购物：商品/电商/消费
- 金融：投资/股票/加密货币
- 工作：职场/招聘/办公
- 政务：政府/政策/公共服务
- 资源：素材/模板/下载/数据
- 搜索：搜索引擎/导航/聚合入口
- 创作：博客/写作/内容发布
- 其他：仅在无法归类时使用

【冲突处理（必须遵守）】
- "教程 + 工具" → 工具优先
- "讲解 + 技术原理" → 技术优先
- "资讯类内容" → 新闻优先
- "平台介绍" → 工具
- "个人博客但技术内容" → 技术

3. tags
- 数量：5~8个
- 类型：关键词（不是句子）
- 必须包含：
  - 领域词
  - 关键技术词或主题词
- 禁止：
  - 空泛词（如：文章/方法/总结）
  - 重复词

4. score
- 整数：1~5
- 标准：
  1 = 噪音
  2 = 一般
  3 = 有参考价值
  4 = 高质量
  5 = 高价值/深度内容

====================
【质量检查（必须满足）】
====================
- JSON 合法
- 字段齐全
- 无多余字段
- tags 数量正确
- category 合法
- summary 长度符合要求

====================
【输入】
====================

标题：
{title}

内容：
{content[:3000]}
"""
    payload = prompt_build(prompt)
    success, data = _call_openclaw_api(payload, timeout=120)
    if success and isinstance(data, dict):
        return data
    return {
        "summary": "",
        "category": "解析失败",
        "tags": ["解析内容失败"],
        "score": 0
    }


def call_openclaw_ai_by_url(url):
    prompt = f"""你是一个基于 URL 的网页分类引擎。

⚠️ 任务限制：
- 只能基于 URL 本身进行判断（不能假设页面内容）
- 不允许编造信息
- 如果信息不足，必须选择最保守分类

⚠️ 输出要求：
- 只输出 JSON
- 必须可被 JSON.parse 解析
- 不允许任何解释或多余内容

====================
【任务】
根据 URL 判断网页类型，并生成结构化信息：
- 分类
- 标签
- 简要推测摘要
- 质量评分
====================

【输出格式】
{{
  "summary": string,
  "category": string,
  "tags": string[],
  "score": number
}}

====================
【分析规则（必须遵守）】
====================

请按以下优先级解析 URL：

1. domain（最重要）
- 如 github.com → 技术
- amazon.com → 购物
- twitter.com → 社交

2. path（次重要）
- /docs /learn → 学习
- /product /item → 购物
- /news → 新闻
- /blog → 创作

3. 关键词识别
- ai / dev / api → 技术
- course / tutorial → 学习
- tool / editor → 工具
- video / music → 娱乐

4. 无法判断时：
- 使用 "其他"
- summary 写为"无法从URL判断具体内容"

====================
【category（必须严格选择）】
====================

["技术","学习","工具","新闻","娱乐","社交","生活","购物","金融","工作","政务","资源","搜索","创作","其他"]

====================
【字段规则】
====================

1. summary
- 30~80字
- 基于 URL 合理推测
- 如果不确定，必须说明不确定

2. tags
- 3~6个标签（比内容版少）
- 来源：
  - domain 名
  - path 关键词
- 示例：
  github → ["GitHub","代码托管","开发"]

3. score
- 基于 URL 信息完整度：
  1 = 无法判断
  2 = 弱判断
  3 = 一般可信
  4 = 高可信（明确网站类型）
  5 = 非常明确（如 github / youtube）

====================
【强约束】
====================
- 不允许假设文章内容
- 不允许生成不存在的信息
- category 必须合法
- tags 数量必须正确

====================
【输入】
====================

URL：
{url}
"""
    payload = prompt_build(prompt)
    success, data = _call_openclaw_api(payload, timeout=120)
    if success and isinstance(data, dict):
        return data
    return {
        "summary": "",
        "category": "解析失败",
        "tags": ["解析URL失败"],
        "score": 0
    }


def call_openclaw_embedding(text):
    payload = {
        "model": "text-embedding-qwen3-embedding-0.6b",
        "input": text
    }
    headers = {
        'User-Agent': 'PostmanRuntime/7.26.8',
        "Content-Type": "application/json",
        "Authorization": f"Bearer {LOCAL_EMBEDDING_API_TOKEN}"
    }
    try:
        resp = requests.post(LOCAL_EMBEDDING_API,
                             json=payload, headers=headers, timeout=30)
        resp.raise_for_status()
        result = resp.json()
        embedding = result["data"][0]["embedding"]
        if isinstance(embedding, list) and len(embedding) > 0:
            return embedding
        else:
            return []
    except Exception as e:
        logger.error(f"调用OpenCLAW嵌入模型失败: {e}")
        return []


def cosine_similarity(a, b):
    if not a or not b:
        return 0.0
    dot = sum(x * y for x, y in zip(a, b))
    norm_a = (sum(x * x for x in a)) ** 0.5
    norm_b = (sum(y * y for y in b)) ** 0.5
    if norm_a == 0 or norm_b == 0:
        return 0.0
    return dot / (norm_a * norm_b)


def fetch_by_requests(url, session=None):
    try:
        headers = {
            "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36",
            "Accept-Language": "zh-CN,zh;q=0.9"
        }
        if session:
            resp = session.get(url, headers=headers, timeout=30)
        else:
            resp = requests.get(url, headers=headers, timeout=30)
        try:
            resp.encoding = 'utf-8'
            resp.text.encode('utf-8').decode('utf-8')
        except (UnicodeDecodeError, UnicodeError):
            resp.encoding = resp.apparent_encoding
        html = resp.text
        soup = BeautifulSoup(html, "html.parser")
        title = soup.title.string.strip() if soup.title and soup.title.string else url
        for tag in soup(["script", "style", "noscript"]):
            tag.decompose()
        text = soup.get_text(separator="\n", strip=True)
        text = re.sub(r"\s+", " ", text)
        if len(text) < 100:
            return None
        return {"title": title, "content": text[:3000]}
    except Exception:
        return None


def fetch_by_trafilatura(url):
    try:
        downloaded = trafilatura.fetch_url(url)
        if not downloaded:
            return None
        content = trafilatura.extract(downloaded)
        if not content or len(content.strip()) < 100:
            return None
        metadata = trafilatura.extract_metadata(downloaded)
        title = metadata.title if metadata and metadata.title else url
        return {"title": title.strip(), "content": content.strip()[:3000]}
    except Exception:
        return None


def fetch_webpage(url, session=None):
    try:
        result = fetch_by_requests(url, session)
        if result:
            result["method"] = "requests"
            return result
        result = fetch_by_trafilatura(url)
        if result:
            result["method"] = "trafilatura"
            return result
        return {"title": "", "content": "", "error": "all_methods_failed"}
    except Exception as e:
        return {"title": "", "content": "", "error": str(e)}


def tokenize(text):
    original = text.lower()
    words = []
    for w in re.findall(r"[a-z0-9_]+", original):
        if len(w) >= 2:
            words.append(w)
    clean = re.sub(r"[a-z0-9_]+", " ", original)
    for part in re.findall(r"[\u4e00-\u9fff]+", clean):
        words.extend(jieba.lcut(part))
    return list(set(w for w in words if len(w) >= 2))


def rebuild_indices():
    build_inverted_index()
    build_graph()


def build_inverted_index():
    bookmarks = read_jsonl(BOOKMARKS_JSONL)
    index = {}
    for b in bookmarks:
        bid = b["id"]
        text = f"{b['title']} {b.get('summary', '')} {' '.join(b.get('tags', []))}"
        words = tokenize(text)
        for word in words:
            if len(word) < 2:
                continue
            if word not in index:
                index[word] = []
            if bid not in index[word]:
                index[word].append(bid)
    write_json_atomic(INVERTED_JSON, index)


def build_graph():
    bookmarks = read_jsonl(BOOKMARKS_JSONL)
    tags = read_jsonl(TAGS_JSONL)
    relations = read_jsonl(RELATIONS_JSONL)
    embeddings = {}
    if os.path.exists(EMBEDDINGS_JSON):
        with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
            embeddings = json.load(f)
    nodes = []
    edges = []
    tag_map = {}
    for t in tags:
        tag_map[t["name"]] = t["id"]
        nodes.append({"id": t["id"], "title": t["name"],
                     "type": "tag", "group": "tag"})
    for b in bookmarks:
        nodes.append({
            "id": b["id"], "title": b["title"][:40], "type": "bookmark",
            "group": b["category"], "url": b["url"], "summary": b.get("summary", "")
        })
    for r in relations:
        edges.append(
            {"source": r["from_node_id"], "target": r["to_node_id"], "label": r["relation_type"]})
    bookmark_list = list(embeddings.items())
    _SIMILARITY_THRESHOLD = 0.85
    _MAX_RELATED_PER_BOOKMARK = 5
    for i, (id1, emb1) in enumerate(bookmark_list):
        candidates = []
        for j, (id2, emb2) in enumerate(bookmark_list):
            if i >= j:
                continue
            if not emb1 or not emb2 or len(emb1) != len(emb2):
                continue
            sim = cosine_similarity(emb1, emb2)
            if sim > _SIMILARITY_THRESHOLD:
                candidates.append((sim, id2))
        candidates.sort(reverse=True)
        for sim, id2 in candidates[:_MAX_RELATED_PER_BOOKMARK]:
            edges.append({"source": id1, "target": id2,
                         "label": "related", "score": round(sim, 4)})
    graph_data = {"nodes": nodes, "edges": edges}
    write_json_atomic(GRAPH_NODES_JSON, graph_data)


def import_bookmarks(html_path):
    try:
        with open(html_path, "r", encoding="utf-8") as f:
            soup = BeautifulSoup(f.read(), "html.parser")
        bookmarks = []
        tag_map = {}
        existing_tags = read_jsonl(TAGS_JSONL)
        for t in existing_tags:
            tag_map[t["name"]] = t
        seen_urls = set()
        items = []
        for a in soup.find_all("a", href=True):
            url = a["href"]
            if url in seen_urls:
                continue
            seen_urls.add(url)
            title = a.get_text(strip=True) or url
            items.append((url, title))

        def fetch_with_retry(url, session, max_retries=2):
            last_result = None
            for attempt in range(max_retries):
                result = fetch_webpage(url, session)
                if result and result.get("error") != "all_methods_failed":
                    return result
                last_result = result
                if attempt < max_retries - 1:
                    sleep(1.0 * (2 ** attempt))
            return last_result

        def process_import_item(url, title):
            sleep(random.random() * 0.5)
            try:
                with requests.Session() as session:
                    webpage = fetch_with_retry(url, session)
                    is_dead_link = False
                    if webpage.get("error") == "all_methods_failed" or (not webpage["content"] and not webpage["title"]):
                        is_dead_link = True
                    if is_dead_link:
                        return None
                    ai_result = call_openclaw_ai_by_content(
                        webpage["title"], webpage["content"])
                embed = call_openclaw_embedding(
                    f"{webpage['title']} {webpage['content'][:3000]}")
                return {
                    "id": str(uuid.uuid4()),
                    "url": url,
                    "title": webpage["title"] or title,
                    "summary": ai_result["summary"],
                    "content": webpage["content"][:3000],
                    "category": ai_result["category"],
                    "tags": ai_result["tags"],
                    "score": ai_result["score"],
                    "is_read": False,
                    "is_dead_link": is_dead_link,
                    "created_at": int(datetime.now().timestamp() * 1000),
                    "updated_at": int(datetime.now().timestamp() * 1000),
                    "added_by": "import",
                    "embed": embed
                }
            except Exception:
                return None

        max_workers = 4
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            future_map = {executor.submit(process_import_item, url, title): (
                url, title) for url, title in items}
            completed = 0
            total = len(items)
            failed_count = 0
            for future in as_completed(future_map):
                url, title = future_map[future]
                try:
                    bookmark = future.result()
                    if bookmark is None:
                        failed_count += 1
                    else:
                        bookmarks.append(bookmark)
                except Exception:
                    failed_count += 1
                completed += 1

        if not bookmarks:
            return {"code": 200, "message": "导入完成，未获取到可用书签", "data": {"imported": 0}}

        for bookmark in bookmarks:
            append_jsonl(BOOKMARKS_JSONL, bookmark)
            for tag_name in bookmark["tags"]:
                if tag_name not in tag_map:
                    tag = {
                        "id": str(uuid.uuid4()), "name": tag_name, "slug": tag_name.lower(),
                        "description": "", "count": 1,
                        "created_at": int(datetime.now().timestamp() * 1000)
                    }
                    tag_map[tag_name] = tag
                else:
                    tag_map[tag_name]["count"] += 1
                relation = {
                    "id": str(uuid.uuid4()), "from_node_id": bookmark["id"], "from_type": "bookmark",
                    "to_node_id": tag_map[tag_name]["id"], "to_type": "tag",
                    "relation_type": "tagged", "score": 1.0,
                    "created_at": int(datetime.now().timestamp() * 1000)
                }
                append_jsonl(RELATIONS_JSONL, relation)

        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        for b in bookmarks:
            embeddings[b["id"]] = b["embed"]
        write_json_atomic(EMBEDDINGS_JSON, embeddings)
        write_jsonl(TAGS_JSONL, list(tag_map.values()))
        rebuild_indices()
        return {"code": 200, "message": "导入成功", "data": {"imported": len(bookmarks), "failed": failed_count}}
    except Exception as e:
        return {"code": 500, "message": f"导入失败: {str(e)}", "data": {}}


def add_bookmark(url):
    try:
        if not url:
            return {"code": 201, "message": "URL 不能为空", "data": {}}
        if not re.match(r"https?://[^\s/$.?#].[^\s/$.?#[^\s/$.?#]", url):
            return {"code": 202, "message": "URL 格式无效", "data": {}}

        existing_bookmarks = read_jsonl(BOOKMARKS_JSONL)
        existing_bookmark = None
        for b in existing_bookmarks:
            if b["url"] == url:
                existing_bookmark = b
                break

        webpage = fetch_webpage(url)

        if webpage["content"] == "" and webpage["title"] == "":
            return {"code": 203, "message": "获取网页内容失败，跳过处理", "data": {}}

        is_dead_link = False
        ai_result = call_openclaw_ai_by_content(
            webpage["title"], webpage["content"])
        embed = call_openclaw_embedding(
            f"{webpage['title']} {webpage['content'][:3000]}")
        if not ai_result["summary"] and not ai_result["tags"] and ai_result["category"] == "解析失败":
            is_dead_link = True

        if existing_bookmark:
            bookmark = existing_bookmark.copy()
            bookmark["title"] = webpage["title"]
            bookmark["summary"] = ai_result["summary"]
            bookmark["content"] = webpage["content"][:3000]
            bookmark["category"] = ai_result["category"]
            bookmark["tags"] = ai_result["tags"]
            bookmark["score"] = ai_result["score"]
            bookmark["updated_at"] = int(datetime.now().timestamp() * 1000)
            bookmark["embed"] = embed
            bookmark["is_dead_link"] = is_dead_link

            updated_bookmarks = []
            for b in existing_bookmarks:
                if b["id"] == bookmark["id"]:
                    updated_bookmarks.append(bookmark)
                else:
                    updated_bookmarks.append(b)
            write_jsonl(BOOKMARKS_JSONL, updated_bookmarks)

            relations = read_jsonl(RELATIONS_JSONL)
            updated_relations = []
            old_tag_ids = set()
            for r in relations:
                if r["from_node_id"] == bookmark["id"] and r["relation_type"] == "tagged":
                    old_tag_ids.add(r["to_node_id"])
                else:
                    updated_relations.append(r)

            existing_tags = read_jsonl(TAGS_JSONL)
            tag_map = {t["name"]: t for t in existing_tags}

            for t in existing_tags:
                if t["id"] in old_tag_ids:
                    t["count"] = max(0, t["count"] - 1)
            write_jsonl(TAGS_JSONL, existing_tags)

            for tag_name in ai_result["tags"]:
                if tag_name not in tag_map:
                    tag = {
                        "id": str(uuid.uuid4()), "name": tag_name, "slug": tag_name.lower(),
                        "description": "", "count": 1,
                        "created_at": int(datetime.now().timestamp() * 1000)
                    }
                    tag_map[tag_name] = tag
                else:
                    tag_map[tag_name]["count"] += 1
                relation = {
                    "id": str(uuid.uuid4()), "from_node_id": bookmark["id"], "from_type": "bookmark",
                    "to_node_id": tag_map[tag_name]["id"], "to_type": "tag",
                    "relation_type": "tagged", "score": 1.0,
                    "created_at": int(datetime.now().timestamp() * 1000)
                }
                updated_relations.append(relation)

            write_jsonl(TAGS_JSONL, list(tag_map.values()))
            write_jsonl(RELATIONS_JSONL, updated_relations)
        else:
            bookmark = {
                "id": str(uuid.uuid4()), "url": url, "title": webpage["title"],
                "summary": ai_result["summary"], "content": webpage["content"][:3000],
                "category": ai_result["category"], "tags": ai_result["tags"],
                "score": ai_result["score"], "is_read": False, "is_dead_link": is_dead_link,
                "created_at": int(datetime.now().timestamp() * 1000),
                "updated_at": int(datetime.now().timestamp() * 1000),
                "added_by": "manual", "embed": embed
            }
            append_jsonl(BOOKMARKS_JSONL, bookmark)

            existing_tags = read_jsonl(TAGS_JSONL)
            tag_map = {t["name"]: t for t in existing_tags}
            for tag_name in ai_result["tags"]:
                if tag_name not in tag_map:
                    tag = {
                        "id": str(uuid.uuid4()), "name": tag_name, "slug": tag_name.lower(),
                        "description": "", "count": 1,
                        "created_at": int(datetime.now().timestamp() * 1000)
                    }
                    tag_map[tag_name] = tag
                else:
                    tag_map[tag_name]["count"] += 1
                relation = {
                    "id": str(uuid.uuid4()), "from_node_id": bookmark["id"], "from_type": "bookmark",
                    "to_node_id": tag_map[tag_name]["id"], "to_type": "tag",
                    "relation_type": "tagged", "score": 1.0,
                    "created_at": int(datetime.now().timestamp() * 1000)
                }
                append_jsonl(RELATIONS_JSONL, relation)
            write_jsonl(TAGS_JSONL, list(tag_map.values()))

        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        embeddings[bookmark["id"]] = bookmark["embed"]
        write_json_atomic(EMBEDDINGS_JSON, embeddings)
        rebuild_indices()

        return {"code": 200, "message": "添加成功" if not existing_bookmark else "更新成功",
                "data": {"status": "ok", "bookmark": bookmark}}
    except Exception as e:
        return {"code": 500, "message": f"添加书签失败: {str(e)}", "data": {}}


def get_bookmark(bookmark_id):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        for b in bookmarks:
            if b["id"] == bookmark_id:
                return {"code": 200, "message": "查询成功", "data": b}
        return {"code": 200, "message": "Bookmark not found", "data": {}}
    except Exception as e:
        return {"code": 500, "message": f"查询失败: {str(e)}", "data": {}}


def list_bookmarks(search=None, tag=None, category=None, limit=-1):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        if not search and not tag and not category:
            bookmarks.sort(key=lambda x: -x["created_at"])
            result_list = bookmarks if limit == -1 else bookmarks[:limit]
            return {"code": 200, "message": "查询成功", "data": {"list": result_list, "total": len(bookmarks)}}

        results = bookmarks
        if tag:
            tag_list = read_jsonl(TAGS_JSONL)
            rels = read_jsonl(RELATIONS_JSONL)
            tag_obj = next((t for t in tag_list if t["name"] == tag), None)
            if tag_obj:
                ids = set()
                for r in rels:
                    if r["to_node_id"] == tag_obj["id"] and r["relation_type"] == "tagged":
                        ids.add(r["from_node_id"])
                results = [b for b in results if b["id"] in ids]

        if category:
            results = [b for b in results if b["category"] == category]

        if search and os.path.exists(INVERTED_JSON):
            with open(INVERTED_JSON, "r", encoding="utf-8") as f:
                index = json.load(f)
            words = tokenize(search)
            ids = None
            for w in words:
                if w in index:
                    if ids is None:
                        ids = set(index[w])
                    else:
                        ids &= set(index[w])
                else:
                    ids = set()
                    break
            if ids is not None:
                results = [b for b in results if b["id"] in ids]

        results.sort(key=lambda x: -x["created_at"])
        result_list = results if limit == -1 else results[:limit]
        return {"code": 200, "message": "查询成功", "data": {"list": result_list, "total": len(results)}}
    except Exception as e:
        return {"code": 500, "message": f"查询失败: {str(e)}", "data": {"list": [], "total": 0}}


def semantic_search(query, top_k=10):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        embeddings_path = os.path.join(DATA_DIR, "embeddings.json")
        if not os.path.exists(embeddings_path):
            return {"code": 500, "message": "语义搜索失败: embeddings.json 不存在", "data": {"list": [], "total": 0}}

        with open(embeddings_path, "r", encoding="utf-8") as f:
            embeddings = json.load(f)

        target_embed = call_openclaw_embedding(query)
        scored = []
        for b in bookmarks:
            emb = embeddings.get(b["id"])
            if not emb:
                continue
            sim = cosine_similarity(target_embed, emb)
            scored.append((-sim, b))

        scored.sort()
        results = [b for (_, b) in scored[:top_k]]
        return {"code": 200, "message": "语义搜索成功", "data": {"list": results, "total": len(results)}}
    except Exception as e:
        return {"code": 500, "message": f"语义搜索失败: {str(e)}", "data": {"list": [], "total": 0}}


def regenerate_embeddings():
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        embeddings = {}
        count = 0
        total = len(bookmarks)
        for i, b in enumerate(bookmarks):
            bookmark_id = b.get("id")
            if not bookmark_id:
                continue
            text = f"{b.get('title', '')} {b.get('summary', '')} {' '.join(b.get('tags', []))}"
            emb = call_openclaw_embedding(text)
            embeddings[bookmark_id] = emb
            b["embed"] = emb
            count += 1

        with open(EMBEDDINGS_JSON, "w", encoding="utf-8") as f:
            json.dump(embeddings, f, ensure_ascii=False)

        write_jsonl(BOOKMARKS_JSONL, bookmarks)
        return {"code": 200, "message": "重新生成成功", "data": {"count": count}}
    except Exception as e:
        return {"code": 500, "message": f"重新生成失败: {str(e)}", "data": {"count": 0}}


def update_bookmark(bookmark_id, data_str):
    try:
        try:
            data = json.loads(data_str)
        except (json.JSONDecodeError, TypeError):
            data = {}
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        found = False
        old_tags = []
        new_tags = []
        for i, b in enumerate(bookmarks):
            if b["id"] == bookmark_id:
                old_tags = b.get("tags", [])
                new_tags = data.get("tags", old_tags)
                bookmarks[i].update(data)
                bookmarks[i]["updated_at"] = int(
                    datetime.now().timestamp() * 1000)
                found = True
                break
        if found:
            write_jsonl(BOOKMARKS_JSONL, bookmarks)
            if set(old_tags) != set(new_tags):
                all_bookmarks = read_jsonl(BOOKMARKS_JSONL)
                all_relations = []
                tag_map = {}
                existing_tags = read_jsonl(TAGS_JSONL)
                for t in existing_tags:
                    tag_map[t["name"]] = t
                for b in all_bookmarks:
                    for tag_name in b.get("tags", []):
                        if tag_name not in tag_map:
                            tag = {
                                "id": str(uuid.uuid4()), "name": tag_name, "slug": tag_name.lower(),
                                "description": "", "count": 1,
                                "created_at": int(datetime.now().timestamp() * 1000)
                            }
                            tag_map[tag_name] = tag
                        else:
                            tag_map[tag_name]["count"] += 1
                        relation = {
                            "id": str(uuid.uuid4()), "from_node_id": b["id"], "from_type": "bookmark",
                            "to_node_id": tag_map[tag_name]["id"], "to_type": "tag",
                            "relation_type": "tagged", "score": 1.0,
                            "created_at": int(datetime.now().timestamp() * 1000)
                        }
                        all_relations.append(relation)
                write_jsonl(TAGS_JSONL, list(tag_map.values()))
                write_jsonl(RELATIONS_JSONL, all_relations)
                rebuild_indices()
            return {"code": 200, "message": "更新成功", "data": {"status": "ok"}}
        return {"code": 200, "message": "Bookmark not found", "data": {}}
    except Exception as e:
        return {"code": 500, "message": f"更新失败: {str(e)}", "data": {}}


def delete_bookmark(bookmark_id):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        bookmark_to_delete = next(
            (b for b in bookmarks if b["id"] == bookmark_id), None)
        if not bookmark_to_delete:
            return {"code": 200, "message": "Bookmark not found", "data": {}}

        tag_names_to_decrement = bookmark_to_delete.get("tags", [])
        new_list = [b for b in bookmarks if b["id"] != bookmark_id]
        if len(new_list) < len(bookmarks):
            write_jsonl(BOOKMARKS_JSONL, new_list)

        rels = read_jsonl(RELATIONS_JSONL)
        tag_count_delta = {}
        new_rels = []
        for r in rels:
            if r["from_node_id"] == bookmark_id and r["relation_type"] == "tagged":
                tag_count_delta[r["to_node_id"]] = tag_count_delta.get(
                    r["to_node_id"], 0) + 1
            elif r["from_node_id"] != bookmark_id and r["to_node_id"] != bookmark_id:
                new_rels.append(r)
        write_jsonl(RELATIONS_JSONL, new_rels)

        tags = read_jsonl(TAGS_JSONL)
        for t in tags:
            decrement = tag_count_delta.get(t["id"], 0)
            if decrement > 0:
                t["count"] = max(0, t.get("count", 0) - decrement)
        write_jsonl(TAGS_JSONL, tags)

        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        if bookmark_id in embeddings:
            del embeddings[bookmark_id]
            write_json_atomic(EMBEDDINGS_JSON, embeddings)

        rebuild_indices()
        return {"code": 200, "message": "删除成功", "data": {"status": "ok"}}
    except Exception as e:
        return {"code": 500, "message": f"删除失败: {str(e)}", "data": {}}


def delete_bookmark_by_url(url):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        bookmarks_to_delete = [b for b in bookmarks if b.get("url") == url]
        if not bookmarks_to_delete:
            return {"code": 200, "message": "未找到匹配的书签", "data": {"deleted_count": 0}}

        bookmark_ids_to_delete = {b["id"] for b in bookmarks_to_delete}
        all_tag_names_to_decrement = set()
        for b in bookmarks_to_delete:
            all_tag_names_to_decrement.update(b.get("tags", []))

        new_bookmarks = [b for b in bookmarks if b["id"]
                         not in bookmark_ids_to_delete]
        if len(new_bookmarks) < len(bookmarks):
            write_jsonl(BOOKMARKS_JSONL, new_bookmarks)

        rels = read_jsonl(RELATIONS_JSONL)
        tag_count_delta = {}
        new_rels = []
        for r in rels:
            if r["from_node_id"] in bookmark_ids_to_delete and r["relation_type"] == "tagged":
                tag_count_delta[r["to_node_id"]] = tag_count_delta.get(
                    r["to_node_id"], 0) + 1
            elif r["from_node_id"] not in bookmark_ids_to_delete and r["to_node_id"] not in bookmark_ids_to_delete:
                new_rels.append(r)
        write_jsonl(RELATIONS_JSONL, new_rels)

        tags = read_jsonl(TAGS_JSONL)
        for t in tags:
            decrement = tag_count_delta.get(t["id"], 0)
            if decrement > 0:
                t["count"] = max(0, t.get("count", 0) - decrement)
        write_jsonl(TAGS_JSONL, tags)

        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        for bid in bookmark_ids_to_delete:
            if bid in embeddings:
                del embeddings[bid]
        write_json_atomic(EMBEDDINGS_JSON, embeddings)

        rebuild_indices()
        return {"code": 200, "message": "删除成功",
                "data": {"deleted_count": len(bookmarks_to_delete), "deleted_ids": list(bookmark_ids_to_delete)}}
    except Exception as e:
        return {"code": 500, "message": f"删除失败: {str(e)}", "data": {}}


def get_random_bookmarks(count=10):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        if not bookmarks:
            return {"code": 200, "message": "获取成功，当前无书签记录", "data": []}
        weighted = []
        for b in bookmarks:
            w = 1.0
            if not b.get("is_read", False):
                w *= 3.0
            w *= b.get("score", 3)
            w *= random.random()
            weighted.append((-w, b))
        weighted.sort()
        results = [b for (_, b) in weighted[:count]]
        return {"code": 200, "message": "获取成功", "data": results}
    except Exception as e:
        return {"code": 500, "message": f"获取失败: {str(e)}", "data": []}


def get_related_bookmarks(bookmark_id):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        bookmark_exists = any(b["id"] == bookmark_id for b in bookmarks)
        if not bookmark_exists:
            return {"code": 200, "message": "Bookmark not found", "data": []}
        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        target_embed = embeddings.get(bookmark_id)
        if not target_embed:
            return {"code": 200, "message": "获取成功，当前无书签记录", "data": []}
        scored = []
        for b in bookmarks:
            if b["id"] == bookmark_id:
                continue
            emb = embeddings.get(b["id"])
            if not emb:
                continue
            sim = cosine_similarity(target_embed, emb)
            scored.append((-sim, b))
        scored.sort(key=lambda x: x[0])
        results = [b for (_, b) in scored[:10]]
        return {"code": 200, "message": "获取成功", "data": results}
    except Exception as e:
        return {"code": 500, "message": f"获取失败: {str(e)}", "data": []}


def list_tags():
    try:
        tags = read_jsonl(TAGS_JSONL)
        tags.sort(key=lambda x: -x.get("count", 0))
        return {"code": 200, "message": "获取成功", "data": tags}
    except Exception as e:
        return {"code": 500, "message": f"获取失败: {str(e)}", "data": []}


def regenerate_ai(bookmark_id):
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        found = None
        idx = -1
        old_tags = []
        for i, b in enumerate(bookmarks):
            if b["id"] == bookmark_id:
                found = b
                old_tags = b.get("tags", [])
                idx = i
                break
        if not found:
            return {"code": 200, "message": "当前无书签记录", "data": {}}
        ai_result = call_openclaw_ai_by_content(
            found["title"], found.get("content", ""))
        embed = call_openclaw_embedding(
            f"{found['title']} {found.get('content', '')[:3000]}")
        new_tags = ai_result["tags"]
        bookmarks[idx]["summary"] = ai_result["summary"]
        bookmarks[idx]["category"] = ai_result["category"]
        bookmarks[idx]["tags"] = new_tags
        bookmarks[idx]["score"] = ai_result["score"]
        bookmarks[idx]["embed"] = embed
        bookmarks[idx]["updated_at"] = int(datetime.now().timestamp() * 1000)
        write_jsonl(BOOKMARKS_JSONL, bookmarks)
        embeddings = {}
        if os.path.exists(EMBEDDINGS_JSON):
            with open(EMBEDDINGS_JSON, "r", encoding="utf-8") as f:
                embeddings = json.load(f)
        embeddings[bookmark_id] = embed
        write_json_atomic(EMBEDDINGS_JSON, embeddings)

        if set(old_tags) != set(new_tags):
            all_bookmarks = read_jsonl(BOOKMARKS_JSONL)
            all_relations = []
            tag_map = {}
            existing_tags = read_jsonl(TAGS_JSONL)
            for t in existing_tags:
                tag_map[t["name"]] = t
            for b in all_bookmarks:
                for tag_name in b.get("tags", []):
                    if tag_name not in tag_map:
                        tag = {
                            "id": str(uuid.uuid4()), "name": tag_name, "slug": tag_name.lower(),
                            "description": "", "count": 1,
                            "created_at": int(datetime.now().timestamp() * 1000)
                        }
                        tag_map[tag_name] = tag
                    else:
                        tag_map[tag_name]["count"] += 1
                    relation = {
                        "id": str(uuid.uuid4()), "from_node_id": b["id"], "from_type": "bookmark",
                        "to_node_id": tag_map[tag_name]["id"], "to_type": "tag",
                        "relation_type": "tagged", "score": 1.0,
                        "created_at": int(datetime.now().timestamp() * 1000)
                    }
                    all_relations.append(relation)
            write_jsonl(TAGS_JSONL, list(tag_map.values()))
            write_jsonl(RELATIONS_JSONL, all_relations)
            rebuild_indices()

        return {"code": 200, "message": "重新生成成功", "data": {"status": "ok", "bookmark": bookmarks[idx]}}
    except Exception as e:
        return {"code": 500, "message": f"重新生成失败: {str(e)}", "data": {}}


def check_dead_links():
    try:
        bookmarks = read_jsonl(BOOKMARKS_JSONL)
        dead = []
        for b in bookmarks:
            try:
                resp = requests.head(
                    b["url"], timeout=30, allow_redirects=True)
                if resp.status_code >= 400:
                    dead.append(b["id"])
            except requests.RequestException:
                dead.append(b["id"])
        return {"code": 200, "message": "检查完成", "data": {"dead": dead}}
    except Exception as e:
        return {"code": 500, "message": f"检查失败: {str(e)}", "data": {"dead": []}}


def get_graph_data():
    try:
        if os.path.exists(GRAPH_NODES_JSON):
            with open(GRAPH_NODES_JSON, "r", encoding="utf-8") as f:
                data = json.load(f)
            return {"code": 200, "message": "获取成功", "data": data}
        return {"code": 500, "message": "图谱数据文件不存在", "data": {"nodes": [], "edges": []}}
    except Exception as e:
        return {"code": 500, "message": f"读取图谱数据失败: {str(e)}", "data": {"nodes": [], "edges": []}}
