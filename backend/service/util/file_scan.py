import json
import logging
import math
import os
import sys
import time
from datetime import datetime
from service.config import PROJECT_ROOT

# 增量扫描缓存文件
_CACHE_SNAPSHOT_FILE = "index_file_scan_snapshot.json"

logger = logging.getLogger(__name__)

# ===================== 编译时构建的常量 =====================

# 文件类型映射字典（O(1) 查找）——9 大类
_EXT_TYPE_MAP = {
    # ===== 前端 =====
    'html': '前端', 'htm': '前端', 'xhtml': '前端',
    'css': '前端', 'scss': '前端', 'sass': '前端',
    'less': '前端', 'styl': '前端', 'pcss': '前端', 'postcss': '前端',
    'js': '前端', 'jsx': '前端', 'ts': '前端',
    'tsx': '前端', 'mjs': '前端', 'cjs': '前端',
    'jsm': '前端', 'tsm': '前端', 'mts': '前端', 'cts': '前端',
    'vue': '前端', 'svelte': '前端', 'astro': '前端',
    'swift': '前端', 'kt': '前端',
    # ===== 后端 =====
    'py': '后端', 'pyw': '后端',
    'java': '后端', 'kotlin': '后端', 'scala': '后端', 'groovy': '后端',
    'c': '后端', 'cpp': '后端', 'h': '后端', 'hpp': '后端', 'cs': '后端',
    'go': '后端',
    'rs': '后端', 'rlib': '后端', 'rmeta': '后端',
    'rb': '后端',
    'php': '后端',
    'dockerfile': '后端',
    'gradle': '后端', 'makefile': '后端', 'cmake': '后端',
    'proto': '后端', 'thrift': '后端', 'idl': '后端',
    'wasm': '后端', 'asm': '后端',
    # ===== 脚本 =====
    'sh': '脚本', 'bash': '脚本', 'zsh': '脚本',
    'bat': '脚本', 'cmd': '脚本', 'ps1': '脚本',
    # ===== 数据库 =====
    'sql': '数据库', 'mysql': '数据库', 'pgsql': '数据库', 'sqlite': '数据库',
    'db': '数据库', 'db3': '数据库', 'sqlite3': '数据库',
    'mdf': '数据库', 'ldf': '数据库', 'sqlitedb': '数据库',
    # ===== 文档 =====
    'md': '文档', 'markdown': '文档', 'rst': '文档', 'tex': '文档',
    'docx': '文档', 'doc': '文档', 'pdf': '文档',
    'ppt': '文档', 'odt': '文档', 'rtf': '文档', 'epub': '文档',
    'xlsx': '文档', 'xls': '文档', 'csv': '文档', 'tsv': '文档', 'ods': '文档',
    'json': '文档', 'jsonc': '文档',
    'xml': '文档', 'yaml': '文档', 'yml': '文档',
    'toml': '文档', 'ini': '文档', 'conf': '文档',
    'env': '文档', 'properties': '文档',
    'txt': '文档', 'text': '文档', 'log': '文档',
    'bak': '文档', 'tmp': '文档', 'cache': '文档', 'lock': '文档', 'map': '文档',
    'gitignore': '文档',
    # ===== 媒体 =====
    'png': '媒体', 'jpg': '媒体', 'jpeg': '媒体',
    'gif': '媒体', 'bmp': '媒体', 'svg': '媒体', 'webp': '媒体',
    'ico': '媒体', 'tiff': '媒体', 'tif': '媒体',
    'raw': '媒体', 'psd': '媒体', 'ai': '媒体', 'eps': '媒体', 'fig': '媒体',
    'mp4': '媒体', 'mov': '媒体', 'avi': '媒体',
    'flv': '媒体', 'wmv': '媒体', 'mkv': '媒体',
    'webm': '媒体', 'm4v': '媒体', 'mpg': '媒体', 'mpeg': '媒体', '3gp': '媒体',
    'mp3': '媒体', 'wav': '媒体', 'flac': '媒体',
    'aac': '媒体', 'ogg': '媒体', 'wma': '媒体', 'm4a': '媒体', 'opus': '媒体',
    'obj': '媒体', 'fbx': '媒体', 'dae': '媒体', 'stl': '媒体', '3ds': '媒体', 'blend': '媒体',
    # ===== 压缩文件 =====
    'zip': '压缩文件', 'rar': '压缩文件', '7z': '压缩文件',
    'tar': '压缩文件', 'gz': '压缩文件', 'bz2': '压缩文件', 'xz': '压缩文件', 'dmg': '压缩文件',
    # ===== 图表 =====
    'drawio': '图表', 'excalidraw': '图表',
    'sketch': '图表', 'xd': '图表', 'afdesign': '图表',
    'puml': '图表', 'plantuml': '图表', 'mmd': '图表',
    # ===== 字体 =====
    'ttf': '字体', 'otf': '字体', 'woff': '字体', 'woff2': '字体', 'eot': '字体',
    'glyphs': '字体', 'glyphs2': '字体',
}

# 忽略的目录（编译为前缀元组，加速前缀匹配）
_IGNORE_DIRS_TUPLE = tuple(
    os.path.normpath(d).replace("\\", "/") for d in [".",
                                                     ".obsidian", ".venv", ".vscode", ".git", "assets",
                                                     ".trae", ".claude", ".idea",
                                                     ".mdgo",
                                                     "node_modules", "vendor", "dist", "build", "out", "target",
                                                     "__pycache__"
                                                     ]
)
_IGNORE_FILENAMES = (".", ".bobconfig", ".itermexport",
                     ".gitignore", ".DS_Store", ".rayconfig")


def should_ignore_file(file_name):
    return file_name.startswith(_IGNORE_FILENAMES)


# 大小格式化的单位常量
_SIZE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB']
_SIZE_DIVISORS = [1, 1024, 1024 ** 2, 1024 ** 3, 1024 ** 4]


def should_ignore_dir(relative_path):
    """检查目录是否应该被忽略（前缀匹配）"""
    normalized = relative_path.replace("\\", "/")
    if '__pycache__' in normalized:
        return True
    return normalized.startswith(_IGNORE_DIRS_TUPLE)


def format_size(bytes_size):
    """O(1) 查表法格式化文件大小"""
    if bytes_size <= 0:
        return '0 B'
    i = min(int(math.log(bytes_size, 1024)), len(_SIZE_UNITS) - 1)
    return f"{bytes_size / _SIZE_DIVISORS[i]:.2f} {_SIZE_UNITS[i]}"


def format_time(ts):
    return datetime.fromtimestamp(ts).strftime('%Y-%m-%d %H:%M:%S')


def get_file_type(ext):
    """O(1) 字典查找文件类型"""
    return _EXT_TYPE_MAP.get(ext.lower(), '其他')


def _get_birthtime(st):
    """
    跨平台获取文件创建时间（birth time）。

    平台差异：
      - macOS / BSD：  st_birthtime 是真正的创建时间，st_ctime 是 inode 变更时间
      - Linux：         部分内核/文件系统（ext4, btrfs）支持 st_birthtime；
                        否则回退 st_ctime（inode 变更时间，近似创建时间）
      - Windows：       st_ctime 就是创建时间，没有 st_birthtime 属性

    优先级：st_birthtime → st_ctime
    """
    if hasattr(st, 'st_birthtime'):
        return st.st_birthtime
    return st.st_ctime


# ===================== 扫描文件（os.scandir 实现） =====================
def _scan_scandir(base_dir, on_file, on_dir=None):
    """
    使用 os.scandir 递归遍历目录。
    通过 on_file/on_dir 回调处理每条记录，避免中间 file_list 二次遍历。
    返回 (total_files, total_folders, total_size)
    """
    total_files = 0
    total_folders = 0
    total_size = 0
    ignored_dirs_total = 0

    stack = [(base_dir, '')]  # (absolute_path, relative_path)
    while stack:
        abs_root, rel_root = stack.pop()

        try:
            with os.scandir(abs_root) as it:
                entries = list(it)
        except PermissionError:
            continue

        dirs = []
        files = []
        for entry in entries:
            name = entry.name
            if should_ignore_file(name):
                continue
            try:
                is_dir = entry.is_dir(follow_symlinks=False)
            except OSError:
                continue

            if is_dir:
                child_rel = f"{rel_root}/{name}" if rel_root else name
                if should_ignore_dir(child_rel):
                    ignored_dirs_total += 1
                    continue
                dirs.append(entry)
            else:
                files.append(entry)

        for d_entry in dirs:
            d_rel = f"{rel_root}/{d_entry.name}" if rel_root else d_entry.name
            stack.append((d_entry.path, d_rel))
            if on_dir:
                on_dir(d_rel)
            total_folders += 1

        for f_entry in files:
            name = f_entry.name
            ext = (os.path.splitext(name)[1][1:]).lower()

            try:
                st = f_entry.stat(follow_symlinks=False)
            except OSError:
                continue

            size_bytes = st.st_size
            ctime_int = int(_get_birthtime(st))
            mtime_int = int(st.st_mtime)
            rel_path = f"{rel_root}/{name}" if rel_root else name
            file_type = get_file_type(ext)

            on_file(name, ext, file_type, size_bytes, ctime_int,
                    mtime_int, rel_path)

            total_files += 1
            total_size += size_bytes

    logger.info(f"📊 忽略目录数：{ignored_dirs_total}")
    return total_files, total_folders, total_size


def scan_files(base_dir):
    """
    使用 os.scandir + 回调模式扫描文件系统。
    边扫描边构建 folder_tree，避免二次遍历 file_list。
    """
    file_list = []
    folder_set = set()
    # 行中构建文件夹树（增量更新）
    tree_dict = {}

    def on_file(name, ext, file_type, size_bytes, ctime_int, mtime_int, rel_path):
        ctime_str = format_time(ctime_int)
        mtime_str = format_time(mtime_int)
        file_info = {
            "name": name,
            "ext": ext,
            "file_type": file_type,
            "size": size_bytes,
            "size_str": format_size(size_bytes),
            "ctime": ctime_int,
            "mtime": mtime_int,
            "ctime_str": ctime_str,
            "mtime_str": mtime_str,
            "ctime_date_str": ctime_str[:10],
            "mtime_date_str": mtime_str[:10],
            "path": rel_path,
        }
        file_list.append(file_info)

        # 边扫描边构建 folder_tree（增量更新）
        path_parts = rel_path.split('/')
        current = tree_dict
        for i in range(len(path_parts) - 1):
            part = path_parts[i]
            if part not in current:
                current[part] = {
                    'size': 0, 'ctime': ctime_int, 'mtime': mtime_int,
                }
            else:
                node = current[part]
                node['size'] += size_bytes
                if ctime_int < node['ctime']:
                    node['ctime'] = ctime_int
                if mtime_int > node['mtime']:
                    node['mtime'] = mtime_int
            current = current[part]

        # 将文件挂载到父目录节点的 __files__ 列表中
        if '__files__' not in current:
            current['__files__'] = []
        current['__files__'].append({
            'name': name,
            'size': size_bytes,
            'ctime': ctime_int,
            'mtime': mtime_int,
        })

    def on_dir(rel_path):
        folder_set.add(rel_path)

    total_files, total_folders, total_size = _scan_scandir(
        base_dir, on_file=on_file, on_dir=on_dir)

    # 将 tree_dict 转换为 ECharts 树图格式（仅目录，不含文件叶子节点）
    def _convert_tree_node(node):
        result = []
        for key in sorted(node):
            if key == '__files__':
                continue
            value = node[key]
            if isinstance(value, dict):
                children = _convert_tree_node(value)
                # 计算该目录下的直接子节点数（子目录 + 直接文件）
                sub_dirs = len(children)
                sub_files = len(value.get('__files__', []))
                folder_node = {
                    'name': key,
                    'file_num': sub_dirs + sub_files,
                    'size': value['size'],
                    'size_str': format_size(value['size']),
                    'ctime': value['ctime'],
                    'mtime': value['mtime'],
                }
                if children:
                    folder_node['children'] = children
                else:
                    folder_node['value'] = value['size']
                result.append(folder_node)
        return result

    # 构建根节点
    all_root_children = _convert_tree_node(tree_dict)

    # 计算根节点的 ctime（最早）和 mtime（最晚）
    root_ctime = min((f['ctime'] for f in file_list), default=0) if file_list else 0
    root_mtime = max((f['mtime'] for f in file_list), default=0) if file_list else 0

    folder_tree = {
        'name': os.path.basename(base_dir),
        'file_num': len(all_root_children),
        'size': total_size,
        'size_str': format_size(total_size),
        'ctime': root_ctime,
        'mtime': root_mtime,
        'children': all_root_children,
    }

    result = {
        "scan_time": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "base_dir": base_dir,
        "stats": {
            "total_files": total_files,
            "total_folders": len(folder_set),
            "total_size_bytes": total_size,
            "total_size_str": format_size(total_size),
        },
        "files": sorted(file_list, key=lambda f: f['path']),
        "folderTree": folder_tree,
    }

    logger.info(f"📊 扫描路径：{base_dir}")
    logger.info(
        f"📊 总文件：{total_files} | 总文件夹：{len(folder_set)} | 总大小：{format_size(total_size)}")
    return result


# ===================== 增量扫描缓存 =====================


def _get_cache_paths(base_dir):
    """获取扫描结果文件和缓存快照文件的路径"""
    output_file = os.path.join(base_dir, '.mdgo', 'data', 'index_file_scan_data.json')
    cache_file = os.path.join(base_dir, '.mdgo', 'data', _CACHE_SNAPSHOT_FILE)
    return output_file, cache_file


def _build_mtime_snapshot(base_dir):
    """
    快速遍历文件系统，仅收集 {rel_path: mtime} 和 {dir_path: True}。
    使用 os.scandir 避免 os.stat 开销。
    返回 (file_snapshot, dir_snapshot, total_size)
    """
    file_snapshot = {}
    dir_snapshot = {}
    total_size = 0
    stack = [(base_dir, '')]

    while stack:
        abs_root, rel_root = stack.pop()
        try:
            with os.scandir(abs_root) as it:
                entries = list(it)
        except PermissionError:
            continue

        dirs = []
        for entry in entries:
            name = entry.name
            if should_ignore_file(name):
                continue
            try:
                is_dir = entry.is_dir(follow_symlinks=False)
            except OSError:
                continue

            if is_dir:
                child_rel = f"{rel_root}/{name}" if rel_root else name
                if should_ignore_dir(child_rel):
                    continue
                dir_snapshot[child_rel] = True
                dirs.append(entry)
            else:
                rel_path = f"{rel_root}/{name}" if rel_root else name
                try:
                    st = entry.stat(follow_symlinks=False)
                except OSError:
                    continue
                file_snapshot[rel_path] = st.st_mtime
                total_size += st.st_size

        for d_entry in dirs:
            d_rel = f"{rel_root}/{d_entry.name}" if rel_root else d_entry.name
            stack.append((d_entry.path, d_rel))

    return file_snapshot, dir_snapshot, total_size


def _load_snapshot(cache_file):
    """从缓存文件加载 mtime 快照"""
    try:
        with open(cache_file, 'r', encoding='utf-8') as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def _load_cached_result(output_file):
    """从缓存的结果文件加载完整扫描数据"""
    try:
        with open(output_file, 'r', encoding='utf-8') as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def _save_cache(cache_file, snapshot):
    """原子写入缓存快照"""
    tmp = cache_file + '.tmp'
    with open(tmp, 'w', encoding='utf-8') as f:
        json.dump(snapshot, f, ensure_ascii=False)
    os.replace(tmp, cache_file)


def scan_file_info(base_dir=None, force=False):
    """
    扫描文件信息，支持增量模式。

    参数：
        base_dir: 扫描根目录
        force: 是否强制全量扫描（忽略缓存）

    增量逻辑：
        1. 构建当前 mtime 快照（快速 scandir，只拿 mtime，不构建完整数据）
        2. 与缓存快照对比，计算变更的文件路径
        3. 若没有变更且缓存存在，直接返回缓存结果（毫秒级）
        4. 若有变更，仅扫描变更的文件重新采数，合并到缓存结果中
    """

    output_file, cache_file = _get_cache_paths(base_dir)

    if not force:
        # 加载缓存快照和结果
        cached_snapshot = _load_snapshot(cache_file)
        cached_result = _load_cached_result(output_file)

        if cached_snapshot and cached_result:
            # 快速构建当前快照
            current_files, current_dirs, current_total_size = _build_mtime_snapshot(
                base_dir)

            old_files = cached_snapshot.get('files', {})
            old_dirs = cached_snapshot.get('dirs', {})
            old_total_size = cached_snapshot.get('total_size', 0)

            # 判断是否有变化：文件新增/删除/mtime变化、目录变化、总大小变化
            has_changes = False

            # 检查文件变更
            if len(current_files) != len(old_files):
                has_changes = True
            else:
                for rel_path, mtime in current_files.items():
                    old_mtime = old_files.get(rel_path)
                    if old_mtime is None or abs(old_mtime - mtime) > 0.001:
                        has_changes = True
                        break

            # 检查目录变更（数量变化或新目录）
            if not has_changes and len(current_dirs) != len(old_dirs):
                has_changes = True

            # 检查总大小变化
            if not has_changes and abs(current_total_size - old_total_size) > 0:
                has_changes = True

            if not has_changes:
                logger.info("📊 无文件变更，使用缓存数据（增量扫描命中）")
                return cached_result

            logger.info(f"📊 检测到文件变更，执行增量扫描...")
            # 识别新增/修改/删除的文件
            new_file_list = []
            for rel_path, mtime in current_files.items():
                old_mtime = old_files.get(rel_path)
                if old_mtime is None or abs(old_mtime - mtime) > 0.001:
                    new_file_list.append(rel_path)

            deleted_paths = set(old_files.keys()) - set(current_files.keys())

            if new_file_list or deleted_paths:
                logger.info(
                    f"📊 变更文件数：{len(new_file_list)} | 删除文件数：{len(deleted_paths)}")

            # 增量更新：只重新扫描变更的文件
            if new_file_list:
                _update_files_from_snapshot(
                    new_file_list, base_dir, cached_result)

            # 删除不复存在的文件
            if deleted_paths:
                _remove_deleted_files(
                    deleted_paths, cached_result, current_total_size)

            # 更新 stats
            stats = cached_result['stats']
            stats['total_files'] = len(current_files)
            stats['total_folders'] = len(current_dirs)
            stats['total_size_bytes'] = current_total_size
            stats['total_size_str'] = format_size(current_total_size)

            # 重建 folderTree
            # 确保文件顺序确定性（按路径排序）
            cached_result['files'].sort(key=lambda f: f['path'])
            tree_children = _convert_file_list_to_tree(cached_result['files'])
            # 计算根节点统计
            files_list = cached_result['files']
            root_ctime = min((f['ctime'] for f in files_list), default=0) if files_list else 0
            root_mtime = max((f['mtime'] for f in files_list), default=0) if files_list else 0
            cached_result['folderTree'] = {
                'name': os.path.basename(base_dir),
                'file_num': len(tree_children),
                'size': current_total_size,
                'size_str': format_size(current_total_size),
                'ctime': root_ctime,
                'mtime': root_mtime,
                'children': tree_children,
            }
            cached_result['scan_time'] = datetime.now().strftime(
                "%Y-%m-%d %H:%M:%S")

            # 保存更新后的缓存快照
            new_snapshot = {
                'files': current_files,
                'dirs': current_dirs,
                'total_size': current_total_size,
                'updated_at': time.time(),
            }
            _save_cache(cache_file, new_snapshot)

            # 保存结果
            os.makedirs(os.path.dirname(output_file), exist_ok=True)
            with open(output_file, 'w', encoding='utf-8') as f:
                json.dump(cached_result, f, ensure_ascii=False)

            logger.info(
                f"✅ 增量扫描完成！数据已保存到 {output_file}")
            return cached_result

    # 全量扫描
    logger.info("📊 执行全量扫描..." if force else "📊 首次扫描，执行全量扫描...")
    result = scan_files(base_dir)

    # 构建并保存缓存快照
    current_files, current_dirs, _ = _build_mtime_snapshot(base_dir)
    snapshot = {
        'files': current_files,
        'dirs': current_dirs,
        'total_size': result['stats']['total_size_bytes'],
        'updated_at': time.time(),
    }
    os.makedirs(os.path.dirname(cache_file), exist_ok=True)
    _save_cache(cache_file, snapshot)

    # 保存结果
    os.makedirs(os.path.dirname(output_file), exist_ok=True)
    with open(output_file, 'w', encoding='utf-8') as f:
        json.dump(result, f, ensure_ascii=False)
    logger.info(f"✅ 全量扫描完成！数据已保存到 {output_file}")

    return result


def _convert_file_list_to_tree(file_list):
    """将 file_list 转换为 ECharts 树图格式（用于增量更新重建 folderTree）"""
    tree = {}
    for file_info in file_list:
        path_parts = file_info['path'].split('/')
        file_size = file_info['size']
        file_ctime = file_info['ctime']
        file_mtime = file_info['mtime']

        current = tree
        for i in range(len(path_parts) - 1):
            part = path_parts[i]
            if part not in current:
                current[part] = {
                    'size': 0, 'ctime': file_ctime, 'mtime': file_mtime,
                }
            else:
                node = current[part]
                node['size'] += file_size
                if file_ctime < node['ctime']:
                    node['ctime'] = file_ctime
                if file_mtime > node['mtime']:
                    node['mtime'] = file_mtime
            current = current[part]

        # 将文件挂载到父目录节点的 __files__ 列表中
        if '__files__' not in current:
            current['__files__'] = []
        current['__files__'].append({
            'name': file_info['name'],
            'size': file_size,
            'ctime': file_ctime,
            'mtime': file_mtime,
        })

    def _convert(node):
        result = []
        for key in sorted(node):
            if key == '__files__':
                continue
            value = node[key]
            if isinstance(value, dict):
                children = _convert(value)
                sub_dirs = len(children)
                sub_files = len(value.get('__files__', []))
                folder_node = {
                    'name': key,
                    'file_num': sub_dirs + sub_files,
                    'size': value['size'],
                    'size_str': format_size(value['size']),
                    'ctime': value['ctime'],
                    'mtime': value['mtime'],
                }
                if children:
                    folder_node['children'] = children
                else:
                    folder_node['value'] = value['size']
                result.append(folder_node)
        return result

    return _convert(tree)


def _update_files_from_snapshot(changed_paths, base_dir, cached_result):
    """
    从已变更的文件路径列表中重新采集文件信息，更新到 cached_result 中。
    changed_paths 是相对路径列表。
    """
    for rel_path in changed_paths:
        abs_path = os.path.join(base_dir, rel_path)
        try:
            st = os.stat(abs_path)
        except OSError:
            continue

        name = os.path.basename(rel_path)
        ext = (os.path.splitext(name)[1][1:]).lower()
        size_bytes = st.st_size
        ctime_int = int(_get_birthtime(st))
        mtime_int = int(st.st_mtime)
        ctime_str = format_time(ctime_int)
        mtime_str = format_time(mtime_int)
        file_type = get_file_type(ext)

        new_info = {
            "name": name,
            "ext": ext,
            "file_type": file_type,
            "size": size_bytes,
            "size_str": format_size(size_bytes),
            "ctime": ctime_int,
            "mtime": mtime_int,
            "ctime_str": ctime_str,
            "mtime_str": mtime_str,
            "ctime_date_str": ctime_str[:10],
            "mtime_date_str": mtime_str[:10],
            "path": rel_path,
        }

        # 查找并替换（或追加）
        found = False
        for i, existing in enumerate(cached_result['files']):
            if existing['path'] == rel_path:
                cached_result['files'][i] = new_info
                found = True
                break
        if not found:
            cached_result['files'].append(new_info)


def _remove_deleted_files(deleted_paths, cached_result, current_total_size):
    """从 cached_result 中移除已删除的文件"""
    old_files = cached_result['files']
    cached_result['files'] = [
        f for f in old_files if f['path'] not in deleted_paths
    ]


if __name__ == '__main__':
    if len(sys.argv) > 1:
        base_dir = os.path.abspath(sys.argv[1])
        # 支持 --force 参数强制全量扫描
        force = '--force' in sys.argv or '-f' in sys.argv
        scan_file_info(base_dir=base_dir, force=force)
