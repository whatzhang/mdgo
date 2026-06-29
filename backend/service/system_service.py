import os
import subprocess
import tempfile
import time


def get_openresty_conf(conf_path):
    """读取 OpenResty 配置文件内容"""
    try:
        with open(conf_path, "r", encoding="utf-8") as f:
            return {"success": True, "conf": f.read(), "path": conf_path, "code": 200}
    except FileNotFoundError:
        return {"success": False, "message": f"配置文件不存在: {conf_path}", "code": 500}
    except PermissionError as e:
        return {"success": False, "message": f"无权限读取配置文件: {e}", "code": 500}
    except Exception as e:
        return {"success": False, "message": str(e), "code": 500}


def reload_openresty_conf(conf_path, conf_content):
    """
    写入配置并重载 OpenResty。
    写入前备份原配置，重载失败时自动回滚备份。
    """
    if not conf_path:
        return {"success": False, "message": "配置文件路径不能为空", "code": 500}
    if not conf_content:
        return {"success": False, "message": "配置内容为空", "code": 500}

    backup_path = None

    # 写入前备份原有配置
    try:
        if os.path.exists(conf_path):
            with open(conf_path, "r", encoding="utf-8") as f:
                original_content = f.read()
            backup_path = os.path.join(
                tempfile.gettempdir(), f"index.conf.backup.{int(time.time())}")
            try:
                with open(backup_path, "w", encoding="utf-8") as f:
                    f.write(original_content)
            except OSError:
                backup_path = None

        with open(conf_path, "w", encoding="utf-8") as f:
            f.write(conf_content)
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

        # reload 失败，回滚备份配置
        if backup_path and os.path.exists(backup_path):
            try:
                with open(backup_path, "r", encoding="utf-8") as f:
                    restore_content = f.read()
                with open(conf_path, "w", encoding="utf-8") as f:
                    f.write(restore_content)
                subprocess.run([
                    "openresty", "-s", "reload"
                ], capture_output=True, text=True, timeout=10)
            except Exception:
                pass

        stderr = reload_result.stderr.strip() if reload_result.stderr else "unknown error"
        return {"success": False, "message": f"重载失败: {stderr}", "code": 500}
    except FileNotFoundError:
        return {"success": False, "message": "openresty 命令未找到，请确认已安装", "code": 500}
    except subprocess.TimeoutExpired:
        return {"success": False, "message": "openresty 重载超时", "code": 500}
    except Exception as e:
        # 异常情况下也尝试回滚
        if backup_path and os.path.exists(backup_path):
            try:
                with open(backup_path, "r", encoding="utf-8") as f:
                    restore_content = f.read()
                with open(conf_path, "w", encoding="utf-8") as f:
                    f.write(restore_content)
                subprocess.run([
                    "openresty", "-s", "reload"
                ], capture_output=True, text=True, timeout=10)
            except Exception:
                pass
        return {"success": False, "message": f"重载失败: {e}", "code": 500}
