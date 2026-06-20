from __future__ import annotations

import strawberry
from typing import Optional, List
from service.system_service import scan_file_info as svc_scan_file_info


# --- GraphQL 类型定义 ---

@strawberry.type
class ScanStats:
    """扫描统计"""
    total_files: int
    total_folders: int
    total_size_bytes: int
    total_size_str: str


@strawberry.type
class ScanFile:
    """单个文件信息"""
    name: str
    ext: str
    file_type: str
    size: int
    size_str: str
    ctime: int
    mtime: int
    ctime_str: str
    mtime_str: str
    ctime_date_str: str
    mtime_date_str: str
    path: str


@strawberry.type
class FolderTreeNode:
    """文件夹树节点"""
    name: str
    file_num: int
    size: int
    size_str: str
    ctime: int
    mtime: int
    children: List[FolderTreeNode]


@strawberry.type
class ScanResult:
    """扫描结果汇总"""
    scan_time: str
    base_dir: str
    stats: ScanStats
    files: List[ScanFile]
    folder_tree: FolderTreeNode


def _build_folder_tree(node: dict) -> FolderTreeNode:
    """递归构建 FolderTreeNode 树结构"""
    return FolderTreeNode(
        name=node["name"],
        file_num=node["file_num"],
        size=node["size"],
        size_str=node["size_str"],
        ctime=node["ctime"],
        mtime=node["mtime"],
        children=[_build_folder_tree(child) for child in node.get("children", [])],
    )


@strawberry.type
class Query:
    """GraphQL 查询入口"""
    @strawberry.field
    def health(self) -> str:
        """健康检查"""
        return "OK"

    @strawberry.field
    def scan_file_info(self, dir: Optional[str] = None) -> ScanResult:
        """扫描指定目录的文件信息"""
        result = svc_scan_file_info(dir_path=dir)
        data = result["data"]
        return ScanResult(
            scan_time=data["scan_time"],
            base_dir=data["base_dir"],
            stats=ScanStats(
                total_files=data["stats"]["total_files"],
                total_folders=data["stats"]["total_folders"],
                total_size_bytes=data["stats"]["total_size_bytes"],
                total_size_str=data["stats"]["total_size_str"],
            ),
            files=[ScanFile(**f) for f in data["files"]],
            folder_tree=_build_folder_tree(data["folderTree"]),
        )


schema = strawberry.Schema(query=Query)