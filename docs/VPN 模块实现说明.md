# VPN 模块实现说明（mihomo · 无 TUN）

对应设计文档：[VPN 模块技术设计（mihomo · 无 TUN）](./VPN%20模块技术设计（mihomo%20·%20无%20TUN）.md)

本文件说明**当前实现**：代码位置、集成方式、运行前置、验证方法与已知边界。

---

## 1. 架构与集成（原生视图，非 iframe）

VPN 界面是 mdgo 主内容区里的一个**原生视图**，与日历、画布、全局搜索等模块一致：

```
main.html
  └── #vpn-container（普通 div，MAIN_VIEW_CONTAINERS 成员）
        └── css_js/modules/vpn.js
              window.vpnApp.mount(container) / destroy()
                 ├── window.__TAURI__.core.invoke("vpn_*")   → Rust commands/vpn.rs
                 └── window.__TAURI__.event.listen("vpn://*") ← VpnManager 事件
```

- 入口：仪表盘「VPN」按钮 → `toggleFile('vpn')` → `openVpnPage()`
- 打开：`openVpnPage()` → `switchToView(vpnContainer)` → `window.vpnApp.mount(vpnContainer)`
- 离开：`destroyEverything()` → `destroyVpnPage()` → `window.vpnApp.destroy()`（解绑事件、清定时器、清容器）
- 不再有 `vpn.html`、iframe、`host-bridge.js` / `vpn-bridge.js`，也没有 `postMessage` 握手。

### 命名与样式约定

| 层 | 约定 |
|---|---|
| JS 常量 | `VPN_*`（如 `VPN_TABS` / `VPN_EVENTS`） |
| JS 函数 | `vpn` 前缀（`vpnMount` / `vpnRenderProxies` / `vpnInvoke`…） |
| 对外 API | `window.vpnApp = { mount, destroy, isMounted }` |
| DOM 类名/ID | `vpn-*`（`vpn-card` / `vpn-node` / `vpn-set-mixed`…） |
| CSS | 作用域 `.vpn-view`；按钮复用 `.btn`（主操作实心蓝、次操作浅蓝、危险操作红色）、输入复用 `.setting-text/.setting-number`、弹窗复用 `.setting-modal*`、通知复用宿主 `showNotification`（右下角气泡）；VPN 布局与状态组件使用 `.vpn-*`，颜色/圆角/阴影取 `:root` 变量 |

---

## 2. 代码地图

### Rust（`tauri/src-tauri/src`）

| 文件 | 职责 |
|---|---|
| `core/vpn/mod.rs` | `VpnManager`：状态机、连接/断开时序、事件推送、崩溃自愈、网络监听、订阅调度、诊断、脱敏日志环 |
| `core/vpn/types.rs` | 前后端数据结构（`VpnState` / `CoreState` / `SystemProxyState` / `ConnectionState` / `Settings` / `Profile`…） |
| `core/vpn/paths.rs` | 数据目录、内核二进制解析、端口探测、原子写 |
| `core/vpn/settings.rs` | `settings.json` 读写、增量 patch、开机自启（Windows Run 键 / macOS LaunchAgent） |
| `core/vpn/config.rs` | 最小受管覆盖生成运行配置（强制端口/secret/监听/模式，删除 `tun`/`listeners` 与额外入站端口，DNS 默认透传）、`mihomo -t` |
| `core/vpn/process.rs` | 子进程 spawn（Windows 隐藏控制台）、stdout/stderr 异步读取、3s 超时强杀、日志文件滚动 |
| `core/vpn/api.rs` | mihomo REST 客户端（version/proxies/select/delay/group-delay/mode/rules/connections/kill + 流量/日志 JSON Lines 长连接） |
| `core/vpn/system_proxy.rs` | `SystemProxyGuard`：Windows 注册表 + `InternetSetOptionW`；macOS `networksetup` + `osascript` 管理员授权 |
| `core/vpn/profiles.rs` | 订阅下载（8MB/3 重定向/HTML 识别）、版本备份（保留 3 份）、节点数差异、失败退避 |
| `core/vpn/diagnostics.rs` | 五项诊断：内核 / 本地端口 / 系统代理 / 当前节点 / 外网连通 |
| `commands/vpn.rs` | 29 个 Tauri 命令门面（含 `vpn_pick_yaml` 本地文件选择） |

### 前端

| 文件 | 职责 |
|---|---|
| `css_js/modules/vpn.js` | VPN 视图逻辑：状态条、6 个 tab、弹窗、事件订阅、轮询、命令调用；对外 `window.vpnApp` |
| `css_js/modules/vpn.css` | VPN 视图样式（`vpn-*` 前缀，复用设计变量与 `.btn`/`.modal-overlay`） |
| `main.html` | dashboard「VPN」按钮、`#vpn-container`、`toggleFile('vpn')`、`openVpnPage/destroyVpnPage`、`destroyEverything` 清理、引入 CSS/JS |

### 内核二进制

不随仓库提交，也不放在 `bundle.resources`。`paths::resolve_core_bin` 查找顺序：

1. 设置里的「内核路径」（`settings.json` 的 `bin_path`）
2. 环境变量 `MDGO_MIHOMO_BIN`
3. 数据目录 `bin/mihomo-windows-x64.exe` / `bin/mihomo-darwin-arm64` / `bin/mihomo[.exe]`
4. `PATH` 中的 `mihomo`

数据目录：

| 平台 | 路径 |
|---|---|
| Windows | `%APPDATA%\mdgo\vpn` |
| macOS | `~/Library/Application Support/mdgo/vpn` |

目录内含 `settings.json`、`profiles.json`、`profiles/<id>/{raw.yaml,versions/}`、`config.yaml`、`cache/`、`mihomo.log`。

---

## 3. 关键实现约定

### 3.1 连接 / 断开时序

开启：
`生成配置 → 原子写入 → mihomo -t → spawn → 轮询 /version（最多 8s）→ 订阅 /traffic 与 /logs（JSON Lines 长连接）→ 快照并开启系统代理 → 校验 → 推送 connected`

关闭（顺序不可颠倒）：
`恢复系统代理 → 校验 → 停止流式任务 → terminate 子进程（3s 超时强杀）→ 推送 disconnected`

- 调试模式 Ctrl+C：`lib.rs` 注册 `tokio::signal::ctrl_c()`，先 `shutdown()`（恢复系统代理 + 停内核）再退出；`spawn` 后写 `mihomo.pid`，各停止路径清理。
- 孤儿兜底：启动时与每次连接前扫描「进程名含 mihomo 且命令行包含本数据目录 cache/config」的进程并强杀，避免上次 Ctrl+C/强杀遗留的内核与新内核同时存在。

崩溃自愈（每 3s 检查 pid）：
`检测到进程消失 → 先恢复系统代理 → intent=connected 且允许恢复 → 3s 后重启（连续 3 次；稳定运行 120s 才清零计数）`

### 3.2 配置合并

- 强制覆盖：`mixed-port`、`external-controller`、`secret`、`bind-address=127.0.0.1`、`allow-lan=false`、`mode`、`log-level`、`find-process-mode`
- 删除：`tun`、`listeners`、`port`、`socks-port`、`redir-port`、`tproxy-port`
- 补充：`profile.store-selected=true`、`profile.store-fake-ip=true`
- DNS：默认原样透传；只有 `dns_enable` 显式设置或 `dns_override=true` 时才改 `dns` 段
- 其余字段（`proxy-providers`、`rule-providers`、`sniffer`、`hosts`…）完全保留

### 3.3 安全

- mihomo 只监听 `127.0.0.1`，`secret` 随机生成（32 字节 base64），**只存在于 Rust 侧**
- 订阅 id 校验（字母数字/`_`/`-`）后再拼路径，防止 `..`/绝对路径穿越
- 订阅下载限制 8MB / 3 次重定向 / 20s 超时；HTML 登录页会被识别并拒绝；导入/更新前跑 `mihomo -t`
- 日志与导出统一脱敏（token/password/uuid/secret/auth/key，含 `access_token`/`apikey` 前缀）
- macOS 写系统代理：脚本写入固定临时路径，`osascript` 只执行 `/bin/sh <path>`，服务名来自 `networksetup` 白名单并用单引号转义
- Windows：注册表快照 5 个字段（含 `AutoConfigURL`/`AutoDetect` 原始二进制），PAC 存在时拒绝接管并提示
- 设置页「关于」只显示「内置 mihomo 内核」，不再附带许可证/源码/SHA-256 信息

---

### 3.4 UI 结构（本轮重设计）

- **顶栏状态组**：`代理` 开关（28×16 Switch）+ 绿色「已开启」+ 内核/系统代理两个彩色状态；三段在同一容器内，悬停显示详情浮层（代理状态、内核版本/PID/端口、系统代理方式/地址）。
- **顶栏右侧**：只保留 ↑/↓ 速率，**已移除波形图**；模式改为「规则 / 全局 / 直连」分段选择。
- **标签**：统一 `.vpn-tag`（高 20 / 圆角 6 / 11px）；成功、警告、危险、信息四类语义色，状态类带 5px 圆点，类型/只读标签不带点。
- **按钮**：统一使用系统 `.btn` 样式与三种语义色：蓝色 `.btn-primary`（常规操作）、绿色 `.btn-success`（导入 / 使用 / 保存等确认动作）、红色 `.btn-danger`（删除 / 断开）；不再自定义 VPN 按钮皮肤。仅补 `line-height:1` / 纵向 padding 0，修正系统按钮文本略偏上的视觉问题。
- **设置页**：双列卡片；系统代理协议行新增文案「仅影响系统代理条目」+ 说明「内核 mixed-port 始终同时支持 HTTP/HTTPS/SOCKS；SOCKS 部分应用可能不读取」。
- **弹窗输入**：复用系统 `.input-modal-input`；弹窗容器 `overflow:hidden`，四角圆角完整。
- **诊断弹窗**：408px 宽、约 300px 高；检查项改为单行键值（左名称/右详情），6px 状态圆点，结论压缩为两行，超长详情用 `title`。
- **当前节点选择器**：顶栏「当前节点」悬停展开面板；按当前策略组展示全部节点，节点按延迟升序（未测速/超时置底），可切换策略组（级联）后点击节点直接切换；每组列表首行提供「测速本组」，测速后原地刷新面板延迟，不整页重绘，级联菜单保持展开。
- **托盘当前节点**：托盘菜单「当前节点：xxx」悬停展开级联子菜单（策略组 → 节点，按延迟升序、超时/未测速置底）；点击节点直接切换；随 `vpn_proxies` 与后台周期任务同步。
- **托盘路由模式**：`规则/全局/直连` 使用 `CheckMenuItem`，当前模式带勾选标记，托盘与主界面状态同步。
- **代理页**：过滤内核自带的 `GLOBAL` 策略组；节点名开头的国旗 emoji 解析为**方形 ISO 代码标识** + 国名（Windows 上旗帜 emoji 会退化成 GB 字母，方框标识保证可见）。
- **Tab**：浅蓝胶囊选中态；无订阅时仅订阅/设置可用，其余置灰。
- **CSS**：所有新增样式限定在 `.vpn-view` 作用域，避免影响主应用其它区域。

设计评审稿与截图：`docs/vpn-redesign/`（评审用，可在合入后按需删除）。
## 4. 验证方法

```powershell
# Rust 编译与单测
cd tauri/src-tauri
cargo check --lib                 # 期望 0 error / 0 warning
cargo test --lib vpn              # 27 passed / 3 ignored
# 真实内核联调（需要本机 mihomo 二进制，只验 API 解析）
cargo test --lib real_core_api_smoke -- --ignored --nocapture
# 孤儿内核清理回归（模拟 Ctrl+C 遗留进程，需要本机 mihomo）
cargo test --lib stale_core_process_is_killed -- --ignored --nocapture
# 流量/日志流解析回归（mihomo JSON Lines，需要本机 mihomo）
cargo test --lib real_core_traffic_stream_yields_events -- --ignored --nocapture

# 前端语法
node --check css_js/modules/vpn.js
node <inline-script-checker> main.html
```

完整回归矩阵与合并门槛见设计文档 [§23 回归要求](./VPN%20模块技术设计（mihomo%20·%20无%20TUN）.md#23-回归要求全量闭环可验收)：
所有 29 个命令、6 个 tab、系统代理/订阅/崩溃自愈/退出清理都必须走“成功 + 失败 + 恢复 + 清理”闭环。

**无内核时的界面验证**：在仓库根目录临时放一个静态宿主页（加载 `css_js/main.css` + `vpn.css` + `vpn.js`，
stub 一个 `window.__TAURI__` 返回示例数据），`window.vpnApp.mount(容器, { tab })` 后逐 tab 截图；
URL 参数 `?tab=proxies|profiles|rules|connections|logs|settings`、`?diag=1` 可复现各页面。
该宿主页只用于本地联调，不进入仓库。

---

## 5. 已知边界

1. **仅系统代理**：不做 TUN；Windows 的 WinHTTP 类应用、部分不读系统代理的程序不会被接管（设置页已提示）。
2. **macOS 未实机验证**：`networksetup` / `osascript` / LaunchAgent 分支仅静态审查；系统代理仍是单网络服务模型，
   Wi-Fi ↔ 有线切换后需要重新开启（网络变化监听会尝试验证并重开，但新服务未纳入快照）。
3. **macOS 未处理 PAC**（Windows 会检测到 `AutoConfigURL` 并拒绝开启）。
4. **仅 Windows x64 / macOS arm64** 二进制（`paths.rs` 也支持 `mihomo-darwin-amd64` 命名，需自行放置）。
5. **0 节点订阅会被拒绝覆盖**（保留旧配置）；节点数骤减只提示，不阻止。
6. **更新失败通知**为应用内 toast（`notify_on_update_fail`），不弹系统通知。
7. **内核 SHA-256 未做启动前强校验**。
8. **调试 Ctrl+C 依赖信号处理**：正常 Ctrl+C 会走 `ctrl_c` → `shutdown()`；`taskkill /F`、断电等强杀场景由下次启动的孤儿扫描兜底（无法保证强杀瞬间恢复系统代理）。

---

## 6. 本轮架构调整记录（相对 iframe 方案）

| 项 | 旧实现 | 现实现 |
|---|---|---|
| 承载方式 | `vpn.html` + iframe | `#vpn-container` + `css_js/modules/vpn.js` |
| 通信 | `host-bridge.js` + `vpn-bridge.js` + postMessage 握手 | 直接 `invoke` / `event.listen`，等待 `window.__TAURI__` 就绪 |
| 样式 | 页面内独立一套 `.btn/.card/.pill` | 复用 `:root` 变量 + 系统 `.btn`（蓝 `.btn-primary` / 绿 `.btn-success` / 红 `.btn-danger`）+ `.setting-text/.setting-number` + `.setting-modal*` + `showNotification`；VPN 专用 `.vpn-status-group/.vpn-switch/.vpn-tag/.vpn-node/...` 全部限定在 `.vpn-view` 作用域 |
| 加载 | 每次进入重载 iframe、两份资源解析 | 随主文档加载一次，`mount/destroy` 切换 |
| 设计稿 | `docs/vpn-mockup/`（HTML + 截图） | 已删除，仅保留本目录两份 Markdown 文档 |
| 许可证 | `resources/vpn/LICENSE.mihomo.txt` + README | 已删除；设置页只显示「内置 mihomo 内核」 |

本轮 UI 重设计（2026-11）：

- 顶栏三段状态重组（开关 + 彩色状态 + hover 详情），移除速率波形图；
- 操作按钮统一为系统 `.btn` 三色（蓝/绿/红），标签、开关、tab、表格、设置双列、空态统一新规范；
- 诊断弹窗压缩到 408px 宽 / 约 300px 高；
- 设置页「系统代理协议」补充“仅影响系统代理条目”的说明文案。
保留的修复（来自上一轮代码审查）：订阅 id 路径穿越防护、崩溃恢复快照保留、导入/更新执行 `mihomo -t`、
节点名转义、配置截断按字符、macOS 服务解析、退出有界等待、崩溃计数与退避、控制类事件始终下发、
日志增量渲染、DNS 继承、端口探测排除冲突等。

本轮新增修复：

- **调试 Ctrl+C 遗留 mihomo**：`ctrl_c` 收尾 + `mihomo.pid` 记录 + 启动/连接前扫描并强杀本数据目录的遗留内核；
- **流量/日志流解析**：mihomo `/traffic`、`/logs` 是 JSON Lines（不是 SSE `data:` + 空行），原解析器等不到事件导致顶部速率恒 0、日志页空白；改为按行读取并兼容 `data:` 前缀，长连接改用无总超时 client；新增 `real_core_traffic_stream_yields_events` 回归测试；
- **速率曲线**：改为按窗口峰值归一化，避免固定 KB/s 比例在真实流量下全部触顶变为横线；
- **find-process-mode 生效方式**：由热重载改为重启内核，确保“连接页显示进程”开关真正生效；
- 回归测试：`stale_core_process_is_killed`（ignored，需本机 mihomo），并纳入设计文档 §23 回归矩阵。
