import json
import logging
import math
import os
import sys
import time
from datetime import datetime
from service.config import PROJECT_ROOT

# 增量扫描缓存快照文件名
_CACHE_SNAPSHOT_FILE = "index_file_scan_snapshot.json"

logger = logging.getLogger(__name__)

# ===================== 文件类型映射表（O(1) 查找，9 大类） =====================

_EXT_TYPE_MAP = {
    # 前端
    'html': '前端', 'htm': '前端', 'xhtml': '前端',
    'css': '前端', 'scss': '前端', 'sass': '前端',
    'less': '前端', 'styl': '前端', 'pcss': '前端', 'postcss': '前端',
    'js': '前端', 'jsx': '前端', 'ts': '前端',
    'tsx': '前端', 'mjs': '前端', 'cjs': '前端',
    'jsm': '前端', 'tsm': '前端', 'mts': '前端', 'cts': '前端',
    'vue': '前端', 'svelte': '前端', 'astro': '前端',
    'swift': '前端', 'kt': '前端',
    # 后端
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
    # 脚本
    'sh': '脚本', 'bash': '脚本', 'zsh': '脚本',
    'bat': '脚本', 'cmd': '脚本', 'ps1': '脚本',
    # 数据库
    'sql': '数据库', 'mysql': '数据库', 'pgsql': '数据库', 'sqlite': '数据库',
    'db': '数据库', 'db3': '数据库', 'sqlite3': '数据库',
    'mdf': '数据库', 'ldf': '数据库', 'sqlitedb': '数据库',
    # 文档
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
    # 媒体
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
    # 压缩文件
    'zip': '压缩文件', 'rar': '压缩文件', '7z': '压缩文件',
    'tar': '压缩文件', 'gz': '压缩文件', 'bz2': '压缩文件', 'xz': '压缩文件', 'dmg': '压缩文件',
    # 图表
    'drawio': '图表', 'excalidraw': '图表',
    'sketch': '图表', 'xd': '图表', 'afdesign': '图表',
    'puml': '图表', 'plantuml': '图表', 'mmd': '图表',
    # 字体
    'ttf': '字体', 'otf': '字体', 'woff': '字体', 'woff2': '字体', 'eot': '字体',
    'glyphs': '字体', 'glyphs2': '字体',
}

# 需要跳过的目录列表（编译为元组以加速前缀匹配）
_IGNORE_DIRS_TUPLE = tuple(
    os.path.normpath(d).replace("\\", "/") for d in [".", "$",
                                                     ".obsidian", ".venv", ".vscode", ".git", "assets",
                                                     ".trae", ".claude", ".idea",
                                                     ".mdgo",
                                                     "node_modules", "vendor", "dist", "build", "out", "target",
                                                     "__pycache__"
                                                     ]
)
_IGNORE_FILENAMES = (".", "$", ".bobconfig", ".itermexport",
                     ".gitignore", ".DS_Store", ".rayconfig")


def should_ignore_file(file_name):
    """检查文件名是否应该被跳过（如 .DS_Store、.gitignore）"""
    return file_name.startswith(_IGNORE_FILENAMES)


# 文件大小格式化常量
_SIZE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB']
_SIZE_DIVISORS = [1, 1024, 1024 ** 2, 1024 ** 3, 1024 ** 4]


def should_ignore_dir(relative_path):
    """判断目录是否在忽略列表中（前缀匹配），如 node_modules、.git"""
    normalized = relative_path.replace("\\", "/")
    if '__pycache__' in normalized:
        return True
    return normalized.startswith(_IGNORE_DIRS_TUPLE)


def format_size(bytes_size):
    """字节转可读大小（1024 进制查表法）"""
    if bytes_size <= 0:
        return '0 B'
    i = min(int(math.log(bytes_size, 1024)), len(_SIZE_UNITS) - 1)
    return f"{bytes_size / _SIZE_DIVISORS[i]:.2f} {_SIZE_UNITS[i]}"


def format_time(ts):
    """时间戳转日期字符串"""
    return datetime.fromtimestamp(ts).strftime('%Y-%m-%d %H:%M:%S')


def get_file_type(ext):
    """根据扩展名查表获取文件分类"""
    return _EXT_TYPE_MAP.get(ext.lower(), '其他')


def _get_birthtime(st):
    """
    跨平台获取文件创建时间。

    macOS/BSD 用 st_birthtime（真正创建时间），
    Linux 优先 st_birthtime（部分 FS 支持），否则回退 st_ctime，
    Windows 的 st_ctime 就是创建时间。
    """
    if hasattr(st, 'st_birthtime'):
        return st.st_birthtime
    return st.st_ctime


# ===================== 文件扫描核心（os.scandir） =====================

def _scan_scandir(base_dir, on_file, on_dir=None):
    """
    用 os.scandir 递归遍历目录，通过回调处理每个文件/目录。
    避免先收集完整列表再二次遍历，提升性能。
    返回 (total_files, total_folders, total_size)
    """
    total_files = 0
    total_folders = 0
    total_size = 0
    ignored_dirs_total = 0

    stack = [(base_dir, '')]  # (绝对路径, 相对路径)
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
                    mtime_int, rel_path, rel_root)

            total_files += 1
            total_size += size_bytes

    logger.info(f"📊 忽略目录数：{ignored_dirs_total}")
    return total_files, total_folders, total_size


def scan_files(base_dir):
    """
    全量扫描：用 os.scandir + 回调模式遍历目录，
    边扫描边构建 folder_tree，避免二次遍历。
    """
    folder_set = set()
    # 运行时构建文件夹树（增量更新）
    tree_dict = {}
    fileListMap = {}  # 用于存储文件列表的映射，键为路径，值为文件信息列表

    def on_file(name, ext, file_type, size_bytes, ctime_int, mtime_int, rel_path, rel_root):
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
        fileListMap.setdefault(rel_root, []).append(
            file_info)  # 将文件信息添加到对应路径的列表中

        # 逐层构建目录树（按路径分段）
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

        # 文件挂载到父目录
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
        # 将空目录也注入 tree_dict，确保 folderTree 能包含它
        path_parts = rel_path.split('/')
        current = tree_dict
        for part in path_parts:
            if part not in current:
                current[part] = {'size': 0, 'ctime': 0, 'mtime': 0}
            current = current[part]

    total_files, total_folders, total_size = _scan_scandir(
        base_dir, on_file=on_file, on_dir=on_dir)

    # 将 tree_dict 转成 ECharts 树图格式（目录节点，不含文件叶子）
    def _convert_tree_node(node):
        result = []
        for key in sorted(node):
            if key == '__files__':
                continue
            value = node[key]
            if isinstance(value, dict):
                children = _convert_tree_node(value)
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

    all_root_children = _convert_tree_node(tree_dict)

    folder_tree = {
        'name': os.path.basename(base_dir),
        'file_num': len(all_root_children),
        'size': total_size,
        'size_str': format_size(total_size),
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
        "files": fileListMap,
        "folderTree": folder_tree,
    }

    logger.info(f"📊 扫描路径：{base_dir}")
    logger.info(
        f"📊 总文件：{total_files} | 总文件夹：{len(folder_set)} | 总大小：{format_size(total_size)}")
    return result


# ===================== 增量扫描缓存 =====================


def _get_cache_paths(base_dir):
    """获取扫描结果和缓存快照的文件路径"""
    output_file = os.path.join(
        base_dir, '.mdgo', 'data', 'index_file_scan_data.json')
    cache_file = os.path.join(base_dir, '.mdgo', 'data', _CACHE_SNAPSHOT_FILE)
    return output_file, cache_file


def scan_file_info(base_dir=None, force=False):
    """
    文件扫描入口，支持增量模式。

    增量逻辑：
      1. 快速构建当前 mtime 快照（只拿修改时间，不构建完整数据）
      2. 与缓存快照对比，找出新增/修改/删除的文件
      3. 无变更 → 直接返回缓存（毫秒级）
      4. 有变更 → 只扫描变更的文件，合并到缓存中
    """
    output_file = os.path.join(
        base_dir, '.mdgo', 'data', 'index_file_scan_data.json')
    logger.info("📊 执行全量扫描..." if force else "📊 首次扫描，执行全量扫描...")
    result = scan_files(base_dir)
    os.makedirs(os.path.dirname(output_file), exist_ok=True)
    with open(output_file, 'w', encoding='utf-8') as f:
        json.dump(result, f, ensure_ascii=False)
    logger.info(f"✅ 全量扫描完成！数据已保存到 {output_file}")

    return result


if __name__ == '__main__':
    if len(sys.argv) > 1:
        base_dir = os.path.abspath(sys.argv[1])
        force = '--force' in sys.argv or '-f' in sys.argv
        scan_file_info(base_dir=base_dir, force=force)
