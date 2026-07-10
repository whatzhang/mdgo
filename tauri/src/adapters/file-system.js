/**
 * Tauri 文件系统适配层
 *
 * 将浏览器 File System Access API (FileSystemFileHandle / FileSystemDirectoryHandle)
 * 替换为基于 Tauri invoke 的虚拟实现，保持接口兼容。
 *
 * 加载方式：在 index.html 中 <script src="src/adapters/file-system.js"></script>
 * 且必须在任何业务逻辑之前加载。
 */

(function () {
  // 仅在 Tauri 环境下生效
  if (typeof window.__TAURI__ === 'undefined') {
    console.log('[TauriFS] 非 Tauri 环境，跳过适配');
    return;
  }

  const { invoke } = window.__TAURI__.core;
  const { open, save } = window.__TAURI__.dialog;

  // =====================================================
  // 虚拟文件句柄 — 兼容 FileSystemFileHandle
  // =====================================================
  class TauriFileHandle {
    constructor(filePath) {
      this._path = filePath;
      this.name = filePath.split(/[/\\]/).pop();
      this.kind = 'file';
    }

    /** 获取 File 对象（模拟） */
    async getFile() {
      const meta = await invoke('get_file_meta', { path: this._path });
      const content = await invoke('read_file_binary', { path: this._path });
      const blob = new Blob([new Uint8Array(content)]);
      blob.name = this.name;
      blob.lastModified = meta.modified * 1000;
      return blob;
    }

    /** 只获取文件元数据，不读取文件内容（用于扫描等高性能场景） */
    async getFileMeta() {
      return await invoke('get_file_meta', { path: this._path });
    }

    /** 创建可写流 */
    async createWritable() {
      return new TauriWritableStream(this._path);
    }

    /** 查询权限 */
    async queryPermission() {
      return 'granted';
    }

    /** 请求权限 */
    async requestPermission() {
      return 'granted';
    }
  }

  // =====================================================
  // 虚拟可写流 — 兼容 FileSystemWritableFileStream
  // =====================================================
  class TauriWritableStream {
    constructor(filePath) {
      this._path = filePath;
      this._position = 0;
      this._buffer = null;
      this._isOpen = true;
    }

    async write(data) {
      if (!this._isOpen) throw new TypeError('Stream is closed');
      if (typeof data === 'string') {
        // 直接写字符串
        if (this._buffer === null) {
          this._buffer = data;
        } else {
          this._buffer += data;
        }
        this._position += data.length;
      } else if (data instanceof Blob) {
        const text = await data.text();
        return this.write(text);
      } else if (data instanceof Uint8Array) {
        // 暂不支持二进制分片写入，一次性写入
        if (this._buffer === null) {
          this._buffer = new TextDecoder().decode(data);
        }
      } else if (data && data.type === 'write') {
        // FileSystemWriteChunkType: { type: 'write', position, data }
        if (data.position !== undefined) this._position = data.position;
        return this.write(data.data);
      } else if (data && data.type === 'seek') {
        this._position = data.position;
      } else if (data && data.type === 'truncate') {
        if (this._buffer !== null && typeof this._buffer === 'string') {
          this._buffer = this._buffer.slice(0, data.size);
        }
      }
    }

    async close() {
      if (!this._isOpen) return;
      this._isOpen = false;
      if (this._buffer !== null && typeof this._buffer === 'string') {
        await invoke('write_file', { path: this._path, content: this._buffer });
      } else if (this._buffer !== null && this._buffer instanceof Uint8Array) {
        await invoke('write_file_binary', { path: this._path, content: Array.from(this._buffer) });
      }
    }

    async truncate(size) {
      if (this._buffer !== null && typeof this._buffer === 'string') {
        this._buffer = this._buffer.slice(0, size);
      }
    }

    async seek(position) {
      this._position = position;
    }
  }

  // =====================================================
  // 虚拟目录句柄 — 兼容 FileSystemDirectoryHandle
  // =====================================================
  class TauriDirectoryHandle {
    constructor(dirPath) {
      this._path = dirPath.replace(/[/\\]+$/, '');
      this.name = this._path.split(/[/\\]/).pop() || '/';
      this.kind = 'directory';
    }

    /** 物理路径 */
    get path() {
      return this._path;
    }

    /** 异步迭代器：遍历目录内容 */
    async *values() {
      const entries = await invoke('read_dir', { path: this._path });
      for (const entry of entries) {
        if (entry.kind === 'directory') {
          yield new TauriDirectoryHandle(entry.path);
        } else {
          yield new TauriFileHandle(entry.path);
        }
      }
    }

    /** 获取目录句柄 */
    async getDirectoryHandle(name, options) {
      const subPath = this._path + '/' + name;
      if (options && options.create) {
        await invoke('create_dir', { path: subPath });
      } else {
        const exists = await invoke('exists', { path: subPath });
        if (!exists) throw new TypeError(`目录不存在: ${name}`);
      }
      return new TauriDirectoryHandle(subPath);
    }

    /** 获取文件句柄 */
    async getFileHandle(name, options) {
      const filePath = this._path + '/' + name;
      if (options && options.create) {
        // 创建空文件
        const exists = await invoke('exists', { path: filePath });
        if (!exists) {
          await invoke('write_file', { path: filePath, content: '' });
        }
      } else {
        const exists = await invoke('exists', { path: filePath });
        if (!exists) throw new TypeError(`文件不存在: ${name}`);
      }
      return new TauriFileHandle(filePath);
    }

    /** 删除子目录/文件 */
    async removeEntry(name, options) {
      const targetPath = this._path + '/' + name;
      await invoke('delete', { path: targetPath });
    }

    /** 查询权限 */
    async queryPermission() {
      return 'granted';
    }

    /** 请求权限 */
    async requestPermission() {
      return 'granted';
    }

    /** values 的别名，兼容 for-await-of */
    [Symbol.asyncIterator]() {
      return this.values();
    }
  }

  // =====================================================
  // 替换全局 API
  // =====================================================

  /** 替换 showDirectoryPicker */
  window.showDirectoryPicker = async function (options) {
    const selected = await open({
      directory: true,
      multiple: false,
      title: '选择目录',
    });
    if (!selected) {
      throw new DOMException('用户取消了选择', 'AbortError');
    }
    return new TauriDirectoryHandle(selected);
  };

  /** 替换 showOpenFilePicker */
  window.showOpenFilePicker = async function (options) {
    const filters = [];
    if (options && options.types) {
      for (const t of options.types) {
        if (t.accept) {
          for (const [mime, exts] of Object.entries(t.accept)) {
            filters.push({
              name: t.description || mime,
              extensions: exts.map(e => e.replace(/^\./, '')),
            });
          }
        }
      }
    }
    const selected = await open({
      directory: false,
      multiple: options && options.multiple,
      filters: filters.length > 0 ? filters : undefined,
      title: '选择文件',
    });
    if (!selected) {
      throw new DOMException('用户取消了选择', 'AbortError');
    }
    const paths = Array.isArray(selected) ? selected : [selected];
    return paths.map(p => new TauriFileHandle(p));
  };

  /** 替换 showSaveFilePicker */
  window.showSaveFilePicker = async function (options) {
    const filters = [];
    if (options && options.types) {
      for (const t of options.types) {
        if (t.accept) {
          for (const [mime, exts] of Object.entries(t.accept)) {
            filters.push({
              name: t.description || mime,
              extensions: exts.map(e => e.replace(/^\./, '')),
            });
          }
        }
      }
    }
    const selected = await save({
      filters: filters.length > 0 ? filters : undefined,
      title: '保存文件',
    });
    if (!selected) {
      throw new DOMException('用户取消了保存', 'AbortError');
    }
    return new TauriFileHandle(selected);
  };

  /** 暴露 Tauri 句柄工厂，供其他适配器使用 */
  window.__tauriFs = {
    TauriFileHandle,
    TauriDirectoryHandle,
  };

  console.log('[TauriFS] 文件系统适配层已加载');
})();
