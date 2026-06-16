from __future__ import annotations

import strawberry
from typing import Optional, List
from backend.service.system_service import scan_file_info as svc_scan_file_info


@strawberry.type
class ScanStats:
    total_files: int
    total_folders: int
    total_size_bytes: int
    total_size_str: str


@strawberry.type
class ScanFile:
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
    name: str
    file_num: int
    size: int
    size_str: str
    ctime: int
    mtime: int
    children: List[FolderTreeNode]


@strawberry.type
class ScanResult:
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
    @strawberry.field
    def health(self) -> str:
        return "OK"

    @strawberry.field
    def scan_file_info(self, dir: Optional[str] = None) -> ScanResult:
        result = svc_scan_file_info(dir_path=dir)
        return ScanResult(
            scan_time=result["scan_time"],
            base_dir=result["base_dir"],
            stats=ScanStats(
                total_files=result["stats"]["total_files"],
                total_folders=result["stats"]["total_folders"],
                total_size_bytes=result["stats"]["total_size_bytes"],
                total_size_str=result["stats"]["total_size_str"],
            ),
            files=[ScanFile(**f) for f in result["files"]],
            folder_tree=_build_folder_tree(result["folderTree"]),
        )


schema = strawberry.Schema(query=Query)