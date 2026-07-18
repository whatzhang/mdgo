import os
import subprocess
import tempfile
import time


def _read_file(path):
    """安全读取文件内容，异常时返回 None"""
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except Exception:
        return None


def _write_file(path, content):
    """安全写入文件，异常时抛出"""
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)


def _rollback(conf_path, backup_path):
    """回滚配置文件并重载（静默，不抛异常）"""
    if not backup_path or not os.path.exists(backup_path):
        return
    try:
        content = _read_file(backup_path)
        if content is not None:
            _write_file(conf_path, content)
            subprocess.run(
                ["openresty", "-s", "reload"],
                capture_output=True, text=True, timeout=10,
            )
    except Exception:
        pass


def get_openresty_conf(conf_path):
    """读取 OpenResty 配置文件内容"""
    if not conf_path:
        return {"success": False, "message": "配置文件路径不能为空", "code": 500}
    content = _read_file(conf_path)
    if content is None:
        return {"success": False, "message": f"文件不存在或无法读取: {conf_path}", "code": 500}
    return {"success": True, "conf": content, "path": conf_path, "code": 200}


def reload_openresty_conf(conf_path, conf_content):
    """
    写入配置并重载 OpenResty。
    写入前备份原配置，重载失败时自动回滚备份（回滚后也尝试重载）。
    """
    if not conf_path:
        return {"success": False, "message": "配置文件路径不能为空", "code": 500}
    if not conf_content:
        return {"success": False, "message": "配置内容为空", "code": 500}

    # 备份原始配置
    backup_path = None
    original_content = _read_file(conf_path)
    if original_content is not None:
        try:
            backup_path = os.path.join(
                tempfile.gettempdir(), f"index.conf.backup.{int(time.time())}")
            _write_file(backup_path, original_content)
        except Exception:
            backup_path = None

    try:
        _write_file(conf_path, conf_content)
    except PermissionError as e:
        return {"success": False, "message": f"无权限写入配置文件: {e}", "code": 500}
    except OSError as e:
        return {"success": False, "message": f"无法写入配置文件: {e}", "code": 500}

    if os.name == 'nt':
        return {
            "success": False,
            "message": "Windows 平台不支持 openresty 重载，请在 Linux/macOS 环境或使用适配的 Web 服务器。",
            "code": 500
        }

    # 执行 reload
    try:
        reload_result = subprocess.run(
            ["openresty", "-s", "reload"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if reload_result.returncode == 0:
            return {"success": True, "message": "配置已生效", "backup_path": backup_path, "code": 200}

        # reload 失败时回滚
        stderr = reload_result.stderr.strip() if reload_result.stderr else "unknown error"
        _rollback(conf_path, backup_path)
        return {"success": False, "message": f"重载失败: {stderr}", "code": 500}
    except FileNotFoundError:
        return {"success": False, "message": "openresty 命令未找到，请确认已安装", "code": 500}
    except subprocess.TimeoutExpired:
        _rollback(conf_path, backup_path)
        return {"success": False, "message": "openresty 重载超时", "code": 500}
    except Exception as e:
        _rollback(conf_path, backup_path)
        return {"success": False, "message": f"重载失败: {e}", "code": 500}
