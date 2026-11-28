# VPN 模块技术设计（mihomo · 无 TUN）· v2

> 状态：**待评审（v2，已落实评审意见）**
> 参照：<https://github.com/clash-verge-rev/clash-verge-rev>（只取核心代理能力）
> 定位：**mdgo 内的轻量代理客户端**，不是完整的 Clash Verge 替代品
> 明确排除：TUN 虚拟网卡、系统级常驻 helper、媒体解锁检测、测试页、全局快捷键、WebDAV

---

## 1. 目标与定位

用户 90% 的使用路径只有 5 步：**打开代理 → 切换节点 → 看延迟 → 更新订阅 → 偶尔排查连接**。
设计围绕这 5 步展开；其余能力（规则、连接、日志）定位为**排错辅助**，不占据首屏。

核心原则（v2 新增）：

1. **内核状态 ≠ 系统代理状态 ≠ 真的能上网**，三者分开建模、分开展示；
2. **订阅配置尽量原样透传**，应用只强制覆盖极少量受管字段；
3. **系统代理的可靠恢复优先于内核自动重启**；
4. **DNS 不做独立编辑器**，默认继承订阅；
5. 前端只做展示与交互，**VPN 运行状态全部由 Rust 持有**。

## 2. 范围与优先级

| 页面 | 优先级 | 说明 |
|---|---|---|
| 代理 | **P0** | 唯一的高频页：策略组 + 节点网格 + 测速 + 模式 |
| 订阅 | **P0** | 导入/更新/切换/用量/到期 |
| 设置 | **P0** | 端口、系统代理、恢复策略、启动、订阅更新、高级（含 DNS） |
| 规则 | P1 | 只读排错 |
| 连接 | P1 | 实时监控 + 断开 |
| 日志 | P1 | 排错工具 |
| ~~DNS~~ | — | **取消一级 tab**，移入「设置 → 高级」 |
| 诊断 | P1（v2 新增） | 顶部按钮打开，5 项连通性检查 |
| 托盘 / 自启 | P1 | 扩展 `tray.rs`；自启默认关 |

Tab 数：**6 个**（代理 / 订阅 / 规则 / 连接 / 日志 / 设置）。

## 3. 状态模型（P0 重构）

### 3.1 数据结构

```rust
#[derive(Serialize, Clone)]
pub struct VpnState {
    pub core: CoreState,
    pub system_proxy: SystemProxyState,
    pub connection: ConnectionState,
    pub network: NetworkState,
    pub ports: Ports,                 // mixed_port, controller_port
    pub active_profile: Option<String>,
    pub last_diag: Option<Diagnostics>,
    pub intent: Intent,               // 用户意图：Connected / Disconnected（用于崩溃后是否自愈）
}

#[derive(Serialize, Clone)]
pub enum CoreState {
    Stopped,
    Starting,                          // 已 spawn，等待 /version 就绪
    Running { pid: u32, version: String },
    Stopping,
    Error { code: String, message: String },
}

#[derive(Serialize, Clone)]
pub enum SystemProxyState {
    Disabled,
    Enabling,
    Enabled { services: Vec<String>, detail: String },
    Restoring,
    Error { code: String, message: String },
}

#[derive(Serialize, Clone)]
pub enum ConnectionState {
    Disconnected,
    Connecting { step: String },       // 步骤名，前端直接展示
    Connected,
    Degraded { reasons: Vec<String> }, // 内核在跑但代理链路不完整
}

#[derive(Serialize, Clone)]
pub enum NetworkState { Online { detail: String }, Offline, Changed { detail: String } }
```

### 3.2 状态判定

`connection` 由 `core` + `system_proxy` + 最近一次诊断**推导**，不单独持久化：

| core | system_proxy | 诊断 | connection | 展示 |
|---|---|---|---|---|
| Stopped/Error | 任意 | — | Disconnected | 灰点「未开启」 |
| Starting | — | — | Connecting | 黄点「正在启动内核…」 |
| Running | Disabled | — | **Degraded** | 橙点「内核运行中 · 系统代理未开启」 |
| Running | Enabled | 失败 | **Degraded** | 橙点「已开启，但网络不通」+ 诊断建议 |
| Running | Enabled | 通过 | Connected | 绿点「代理已开启」 |

> UI 文案统一用「**代理已开启**」，不再使用「VPN 已连接」，避免与 TUN 型 VPN 混淆；
> 「规则/全局/直连」统一称「**路由模式**」。

### 3.3 开启流程（串行状态机）

```
用户点击「开启代理」
 → 加生命周期锁（只串行化生命周期操作）
 → 生成 config.yaml（第 7 节）
 → mihomo -t 配置测试 ──失败→ Error(配置非法) + 日志定位
 → 探测端口（7890/9090 起）
 → spawn 内核，异步读取 stdout/stderr
 → 轮询 /version（≤8s）──超时→ kill + Error(启动超时)
 → TCP 探测 mixed-port 可连接
 → SystemProxyGuard.snapshot_original()   （若之前无快照）
 → SystemProxyGuard.enable_proxy()        （按平台，见第 11 节）
 → SystemProxyGuard.verify_proxy()        ──失败→ rollback + Error(系统代理设置失败)
 → 触发一次连接诊断（第 13 节，异步，不阻塞）
 → Connected
```

每一步都发 `vpn://progress { step, label, ok }`，前端状态条据此展示「正在启动内核… / 正在开启系统代理…」。

### 3.4 关闭流程

```
用户点击「关闭代理」
 → 记录 intent = Disconnected
 → SystemProxyGuard.restore_original()
 → verify 系统代理已恢复 ──失败→ 重试一次；“仍失败”则保留快照并告警，但继续停内核
 → 停内核（优雅退出 → 超时强杀）
 → Disconnected
```

> **顺序不可颠倒**：先撤系统代理再停内核，否则中间有一段时间系统代理指向死端口。

### 3.5 崩溃 / 异常退出

| 事件 | 处理顺序 |
|---|---|
| 内核异常退出且 intent=Connected | ① **先恢复系统代理**（避免指向死端口）→ ② 30s 内最多重启 3 次（1/2/4s 退避）→ ③ 重启成功则重新开系统代理，失败则 `Error` + 告警 |
| 内核异常退出且 intent=Disconnected | 只恢复系统代理（幂等）+ 状态归零 |
| 应用崩溃/强杀 | 下次启动读快照 → 默认**自动恢复**系统代理，并在 UI 顶部提示「检测到上次异常退出，已恢复系统代理」 |
| 应用正常退出 | `RunEvent::ExitRequested` 里同步：恢复系统代理 → 停内核（有 3s 超时兜底） |

## 4. 模块分解（P1）

不使用覆盖所有操作的大 Mutex；按职责拆分，状态用 `RwLock`，生命周期用独立 `tokio::sync::Mutex` 串行：

```
VpnServices (app.manage)
├── CoreSupervisor      进程生命周期、健康检查、重启策略、日志读取
├── ConfigBuilder       配置生成、强制覆盖、mihomo -t 校验、原子替换
├── ProfileManager      订阅 CRUD、下载、版本备份、差异检测、定时任务
├── MihomoApiClient     /proxies /configs /connections /rules /logs /traffic（无状态）
├── SystemProxyGuard    快照/开启/校验/冲突检测/恢复/崩溃恢复
├── NetworkMonitor      网络变化与休眠恢复（第 12 节）
├── Diagnostics         5 项连通性检查（第 13 节）
└── EventPublisher      节流后统一 emit
```

- 生命周期操作（start/stop/restart/switch-profile）加锁串行；
- 测速、日志读取、订阅下载、诊断**不参与**该锁，避免互相阻塞；
- 所有状态变更统一走 `VpnServices::update_state(fn)`，由 `EventPublisher` 推送 `vpn://state`。

## 5. 架构总览

```
┌──────────────────────────────────────────────────────────────┐
│ mdgo 主窗口 (main.html)                                       │
│  #vpn-container（原生视图容器）                                  │
│     css_js/modules/vpn.js：window.vpnApp.mount / destroy       │
│     ↕ invoke / listen（不再经 postMessage 桥）                  │
│     ↕ invoke / listen                                         │
│ Rust: commands/vpn.rs → VpnServices                           │
└──────────────┬───────────────────────────────────────────────┘
               │ HTTP 127.0.0.1:<controller> + Bearer secret
        ┌──────▼──────┐
        │ mihomo.exe  │  mixed-port 监听（无 TUN）
        └─────────────┘
```

## 6. 配置管线（P0 重构）

### 6.1 处理原则：原始订阅 + 最小受管覆盖

**不再使用「base 整体覆盖 + DNS 特殊处理」的通用合并策略**。运行时配置 = **订阅原文** + 一张极短的强制覆盖表：

| 字段 | 处理 | 原因 |
|---|---|---|
| `mixed-port` | 强制 = 应用端口 | 端口由应用管理 |
| `external-controller` | 强制 = `127.0.0.1:<port>` | 只允许本机 |
| `secret` | 强制 = 应用生成 | 安全 |
| `bind-address` | 强制 = `127.0.0.1` | 禁止局域网 |
| `allow-lan` | 强制 = `false` | 同上 |
| `mode` | 强制 = 用户选择（rule/global/direct） | 路由模式由 UI 控制 |
| `log-level` | 强制 = 设置项 | 日志页依赖 |
| `tun` | **删除整块** | 明确不做 TUN，防止订阅偷偷开 |
| `listeners` | **删除整块** | v1 不开放自定义入站端口（安全） |
| `profile.store-selected` | 强制 = `true` | 记住手动选中的节点 |
| 其余一切（`proxies` `proxy-groups` `rules` `proxy-providers` `rule-providers` `dns` `hosts` `sniffer` `geodata*` `ntp` `experimental` …） | **原样保留** | 不改变订阅行为 |

> 实现方式：`serde_yaml::Value` 上做**定点赋值/删除**，不递归合并、不注入默认值。
> 生成前若存在 `tun`/`listeners`，记录一条「已按安全策略移除」的日志（用户可在日志页看到）。

### 6.2 校验与生效流程

```
下载/导入 → 保存原始 YAML（raw）
 → 解析 YAML（失败：语法错误行号）
 → 结构校验（存在 proxies / proxy-groups / rules 之一；未知顶层键记录告警但不拒绝）
 → 生成运行配置（6.1 覆盖表）
 → mihomo -t -f <tmp>   ← 以 Mihomo 自身为准，不只看 YAML 能不能解析
     ├─ 退出码 0 → 通过
     └─ 非 0 → 解析 stderr，指出错误行/字段，保留旧配置
 → 原子替换 config.yaml（同目录临时文件 + rename）
 → 热重载：PUT /configs?force=true；失败则优雅重启内核
```

- `mihomo -t` 的 stderr 会格式化后展示在订阅卡片「查看详情」里（截断 4KB）；
- 生成与测试都在临时目录完成，**不触碰正在运行的 config.yaml**。

### 6.3 配置生成的约束

- 控制器只监听 `127.0.0.1`（mihomo 默认 0.0.0.0，必须显式覆盖）；
- secret 每次安装随机生成，存 `settings.json`（0600），**永不出现在前端**；
- 运行配置写盘后 chmod 0600（含节点密码）。

## 7. 订阅管理（P0/P1 补充）

### 7.1 下载硬约束

| 约束 | 值 |
|---|---|
| 连接/总超时 | connect 8s / total 20s |
| 最大重定向 | 3 |
| 最大响应体 | 8 MB（超出立即中断） |
| TLS | 强制校验，不允许跳过 |
| Content-Type | `yaml`/`text/*`/`octet-stream` 允许；`text/html` 直接拒绝 |
| HTML/登录页识别 | 首 512B 内出现 `<html`/`<!doctype`/「登录」「过期」等特征 → 判为失败 |
| 状态码 | 仅 2xx 视为成功；403/429 给出明确文案 |

### 7.2 数据与版本

```
profiles/
  index.json                # 列表 + 激活项 + 每项 stats
  <id>/
    raw.yaml                # 当前使用的原始订阅（只读参考）
    meta.json               # 上次更新、节点数、组数、规则数、差异、错误
    versions/<ts>.yaml      # 最近 3 个版本（可回滚）
```

- 更新失败：**保留旧 raw.yaml 与运行状态**，卡片显示失败原因 + 「重试」；
- **差异检测**：更新前后比较 `proxies` 数量、`proxy-groups` 数量、`rules` 数量；
  - 节点数变为 0 → 红色警告「本次订阅内容异常（0 个节点），已保留旧配置」；
  - 变化超过 80% → 黄色提示「节点数变化较大：86 → 12」；
- **到期提醒**：`expire` 7 天内每天提示一次；已过期在卡片与顶部横幅提示；
- **流量不足**：剩余 < 10% 时卡片显示黄色提示；
- **URL 脱敏**：UI 默认 `token=••••`，设置里可临时显示；
- **定时更新**：后台每 5 分钟检查，单并发，失败只记录不阻塞；
- 统计信息（节点数/组数/规则数/更新时间/下次更新时间）直接展示在卡片主行。

## 8. mihomo API 代理层

### 8.1 命令（`#[tauri::command]`，薄门面 → VpnServices）

| 命令 | 入参 | 返回 |
|---|---|---|
| `vpn_state` | — | `VpnState` |
| `vpn_connect` / `vpn_disconnect` | — | `VpnState` |
| `vpn_restart_core` | — | `VpnState` |
| `vpn_diag_run` | — | `Diagnostics` |
| `vpn_proxies` | — | `Vec<ProxyGroup>`（含 `tested_at`、`delay_state`） |
| `vpn_select_proxy` | `{group, name}` | `()` |
| `vpn_delay` / `vpn_group_delay` | `{name}` / `{group}` | `{name: DelayState}` |
| `vpn_mode_set` | `{mode}` | `VpnState` |
| `vpn_rules` | — | `Vec<Rule>`（含 `index`、`source`、`provider`、`is_final`） |
| `vpn_connections` | — | `{connections, upload, download}`（含 `protocol`、`process`） |
| `vpn_kill_connection` / `vpn_kill_all` | `{id}` / — | `()` |
| `vpn_logs` | `{level, limit}` | `Vec<LogEntry>`（脱敏后） |
| `vpn_logs_clear` / `vpn_logs_export` | — | `()` / `{path}` |
| `vpn_profiles` / `vpn_profile_add` / `_update` / `_remove` / `_activate` | 见前版 | `Profile`/`Vec<Profile>` |
| `vpn_profile_versions` / `vpn_profile_rollback` | `{id}` / `{id, ts}` | `Vec<Version>` / `()` |
| `vpn_settings_get` / `_set` | `{...}` | `Settings` |
| `vpn_sysproxy_get` / `vpn_sysproxy_set` / `vpn_sysproxy_services` | `{on}` / — | 状态 / 服务列表 |
| `vpn_network_state` | — | `NetworkState` |

### 8.2 事件（全部带 `v` 版本号）

| 事件 | 频率 | 负载 |
|---|---|---|
| `vpn://state` | 变化时 | `VpnState` |
| `vpn://progress` | 每步 | `{step, label, ok, error?}` |
| `vpn://traffic` | 1s（页面隐藏 5s） | `{up, down, upTotal, downTotal}` |
| `vpn://connections` | 2s（连接页不可见 10s） | `{connections, upload, download}` |
| `vpn://log` | ≤5 条/200ms | `LogEntry`（已脱敏） |
| `vpn://profiles` | 变化时 | `{profiles, activeId}` |
| `vpn://diag` | 诊断完成 | `Diagnostics` |
| `vpn://network` | 变化时 | `NetworkState` |

## 9. 前端集成：原生容器模块（非 iframe）

> 早期设计曾用「独立 vpn.html + iframe + postMessage 通用桥」，实际落地后因为跨文档时序、双份资源解析、
> 桥维护成本而改为**原生视图模块**，与 mdgo 其它业务视图（日历/画布/搜索等）保持一致。

### 9.1 形态

```
main.html
  #vpn-container（普通 div，MAIN_VIEW_CONTAINERS 成员）
      └── css_js/modules/vpn.js  window.vpnApp.mount(container)
              ├── 直接 window.__TAURI__.core.invoke("vpn_*")
              └── 直接 window.__TAURI__.event.listen("vpn://*")
                     ↕ Rust: commands/vpn.rs → VpnManager
```

### 9.2 约定

1. **两个文件**：`css_js/modules/vpn.js`（逻辑）+ `css_js/modules/vpn.css`（仅 VPN 特有布局），
   JS 常量 `VPN_*`、函数 `vpnXxx`、DOM 类名/ID `vpn-*`，与其它模块一致；
2. **样式复用**：颜色/圆角/阴影全部取 `:root` 变量；按钮复用系统 `.btn`（蓝 `.btn-primary` / 绿 `.btn-success` / 红 `.btn-danger`），输入框复用 `.setting-text/.setting-number`（弹窗用 `.input-modal-input`），弹窗复用 `.setting-modal*`，通知复用宿主 `showNotification`（右下角气泡）；VPN 专有的电源状态组（含 Switch）、标签、节点行、表格皮肤在 `.vpn-view` 作用域内用 `.vpn-*`，不污染其它模块；
3. **生命周期**：`mount(container)` 建 DOM、拉取状态、订阅事件、启动轮询；`destroy()` 解绑事件、清定时器、清容器；
   离开视图由 `destroyEverything()` 统一调用；
4. **后端无关**：视图只消费 `vpn_state` / `vpn_*` 命令与 `vpn://*` 事件，不持有内核/系统代理状态；
5. **Tauri 适配层是异步注入的**（`window.__tauriInitPromise`），模块内所有调用先等待 `window.__TAURI__` 就绪，
   不再依赖页面加载顺序。

### 9.3 为什么不用 iframe / 通用桥

- 同源 iframe 并不构成安全边界（仍可访问父页面 API），桥只能算接口约束；
- 需要额外的握手/订阅/超时/来源校验胶水代码，且 `type="module"` 适配层与 classic 脚本存在加载时序差；
- 独立页面带来第二份 CSS/JS 解析与构建入口，打开变慢；
- 若未来确实需要隔离第三方内容，再引入独立 origin + `sandbox`（届时才需要消息桥）。
## 10. 系统代理（P0 强化）

### 10.1 `SystemProxyGuard`（独立模块）

```rust
SystemProxyGuard
├── snapshot_original()      // 首次开启前拍快照，落盘 settings.json.sysProxySnapshot
├── enable_proxy(port)       // 按平台写入，随后 verify
├── verify_proxy()           // 回读系统设置，与期望值比对
├── detect_external_change() // 开启期间若发现被第三方改动 → 告警/重写
├── restore_original()       // 只恢复本程序改过的项
└── recover_after_crash()    // 启动时检测「快照存在但上次未正常退出」
```

快照结构：

```jsonc
{
  "platform": "windows|macos",
  "owned": true,
  "session": "uuid",            // 每次开启生成，防止误恢复他人的修改
  "services": ["Wi-Fi"],        // macOS
  "original": { /* 平台原始值 */ },
  "applied":  { /* 本程序写入的值（用于冲突检测） */ },
  "createdAt": 1750000000
}
```

### 10.2 Windows

- 位置：`HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`
- 快照字段：`ProxyEnable`、`ProxyServer`、`ProxyOverride`、`AutoConfigURL`、`AutoDetect`
- 开启：
  1. 若 `AutoConfigURL` 非空 → 视为 PAC 环境，**默认不接管**，提示用户（可强制覆盖）；
  2. 写入 `ProxyEnable=1`、`ProxyServer=127.0.0.1:<port>`、`ProxyOverride=localhost;127.*;<local>`；
  3. `InternetSetOption(INTERNET_OPTION_SETTINGS_CHANGED)` + `…REFRESH` 让 WinINet 立即生效；
  4. `verify`：回读三个值比对，不一致 → 回滚并报错。
- 冲突检测：开启期间每 30s 回读一次，发现 `ProxyServer` 被改 → `vpn://state` 标 `Degraded`，UI 提示「系统代理被其他程序修改」。
- 恢复：写回快照三个字段 + 通知刷新；`AutoConfigURL/AutoDetect` 只恢复曾改动的项。
- **明确写入文档**：这里设置的是 **WinINet（IE/Edge/系统组件）** 代理；使用 WinHTTP 的应用（部分服务、命令行工具）不受影响。需要 WinHTTP 支持时再通过 `netsh winhttp`（需管理员）作为 P2 选项，不在 v1 范围。

### 10.3 macOS

- 枚举：`networksetup -listallnetworkservices`（首行是提示，跳过；行首 `*` 表示已停用）；
- 识别当前活动服务：`route -n get default` → 拿 `interface` → `networksetup -listnetworkserviceorder` 里把 `Device: en0` 映射回服务名；识别不到时让用户手选（设置页）；
- 逐服务快照：`-getwebproxy` / `-getsecurewebproxy` / `-getsocksfirewallproxy` 的 `Enabled/Server/Port` 三项；
- 开启：对**当前服务**分别设置 HTTP、HTTPS（SOCKS 默认不开，设置里可开），然后 `-setwebproxystate on` 等；
- 权限：通过 `osascript -e 'do shell script "…" with administrator privileges'` 触发系统授权弹窗；
  - **安全**：不使用字符串拼接；服务名与参数经**单引号转义**（`'` → `'\''`）后再拼入 `do shell script`，且服务名先与 `-listallnetworkservices` 的结果做白名单比对，杜绝命令注入；
- 恢复：只对快照里记录的服务恢复其**原本**的 Enabled/Server/Port；若某服务已被删除则跳过并记录；
- **重启/换网注意**：macOS 切换网络服务时系统会重置代理设置，`NetworkMonitor` 检测到后**重新应用**（若 intent=Connected）。

### 10.4 恢复策略

| 场景 | 行为 |
|---|---|
| 用户点「关闭代理」 | 恢复 → 校验 |
| 内核崩溃 | **先恢复**，再决定是否重启内核 |
| 应用正常退出 | 恢复 + 3s 超时 |
| 应用崩溃/强杀 | 下次启动自动恢复（快照 `owned=true` 且 `session` 未被正常关闭） |
| 恢复失败 | 保留快照、置 `Error`、UI 常驻横幅「系统代理未能自动恢复，请手动检查」+「重试」按钮 |

## 11. 网络监听与休眠恢复（P0 新增）

`NetworkMonitor` 后台任务（5s 轮询，轻量）：

- 采集：本地 IP 列表 + 默认路由接口（跨平台读取；无新依赖时用 `sysinfo` + 系统命令兜底）；
- 判定：接口/地址变化 → `NetworkChanged`；全部失效 → `Offline`；
- **休眠恢复**：检测到两次 tick 的时间差 > 60s → 视为 `SystemResumed`；
- 事件触发后的动作（不盲目重启内核）：
  1. `GET /version` 健康检查；
  2. 重探 mixed-port；
  3. 校验系统代理是否仍指向本地端口（macOS 换网常被重置）；
  4. 若 `intent=Connected` 且某项失效 → 按最小动作修复（重开系统代理 / 重启内核）；
  5. 推送 `vpn://network` 与 `vpn://state`。

## 12. 连接诊断（P1 新增）

`vpn_diag_run` 依次执行，全部只读、≤10s：

| # | 检查 | 方式 | 失败提示 |
|---|---|---|---|
| ① | 内核运行 | `GET /version` | 内核未运行，点击「开启代理」 |
| ② | 本地端口 | TCP connect `127.0.0.1:<mixed-port>` | 端口未监听，重启内核 |
| ③ | 系统代理 | 回读并与期望比对 | 系统代理未开启/被修改，重新开启 |
| ④ | 当前节点 | `GET /proxies/<当前组选中节点>/delay` | 节点不可用，切换节点 |
| ⑤ | 外网连通 | 经 mixed-port 请求 `generate_204`（5s） | 外网不通，检查节点或规则 |

结果：`{ ok, checks: [{id, ok, latencyMs?, message}], conclusion, suggestions[] }`，UI 用面板展示（第 13 节）。

## 13. UI 规格（v2 重构）

### 13.1 顶部状态条

```
[⏻ 代理 已开启 | ● 内核 运行中 | ● 系统代理 已启用]  [规则|全局|直连]  当前节点 香港 01 · 42ms >   ↑ 1.2 MB/s  ↓ 8.4 MB/s  [诊断]
```

- 代理使用**真实 Switch**（28×16）+「代理」+ 绿色「已开启」，整块可点击；明确是开关而非状态文字；
- **内核 / 系统代理**是两个独立状态项：状态值 + 圆点用颜色表达（绿/橙/红），标签弱化；
- 悬停状态组显示详情浮层：代理状态、内核版本/PID/端口、系统代理方式/地址；使用自定义浮层避免与原生 `title` 双提示；
- 「当前节点」悬停展开节点选择器：按当前策略组列出全部节点、按延迟升序，可先切换策略组（级联）再点击节点直接切换；每组列表首行提供「测速本组」，测速后原地刷新延迟并保持展开；
- 托盘「当前节点：xxx」悬停展开级联子菜单（策略组 → 节点，按延迟升序、超时/未测速置底），点击即切换；节点数据随 `vpn_proxies` 与后台同步；
- 托盘「路由模式」的三项使用勾选菜单项，当前模式带标记；
- 「当前节点」可点击 → 跳转代理页并定位选中项；
- 版本号、端口、PID 不在顶部常驻，放在设置页 / hover 浮层；
- **移除流量曲线**，只保留 ↑/↓ 速率，减少无意义装饰。

### 13.2 代理页（重做）

**左栏（策略组）＋ 右栏（节点网格）**：

```
┌ 策略组 ───────────┬ 节点 (12/86)  [搜索] [按延迟排序] [全部测速] ┐
│ 🚀 节点选择  86  › │  ┌──────────┐ ┌──────────┐ ┌──────────┐              │
│ ♻️ 自动选择   12   │  │香港 01 ✓ │ │香港 02   │ │日本 01   │              │
│ 🎯 全球直连    3   │  │SS · 42ms │ │SS · 51ms │ │VM · 88ms │              │
│ 🛑 广告拦截    2   │  └──────────┘ └──────────┘ └──────────┘              │
│ 📲 电报消息    4   │  …（网格，自适应列宽）                                  │
└───────────────────┴────────────────────────────────────────────────────────┘
```

- 不展示内核自带的 `GLOBAL` 策略组（与订阅节点重复）；
- 节点名开头的国旗 emoji 转为方形 ISO 代码标识 + 国名，避免 Windows 上旗帜渲染成 GB 字母；
- 节点延迟状态区分：`未测速`（灰）/`测速中`（转圈）/`42ms`（绿）/`180ms`（黄）/`超时`（红）/`失败`（红）；
- 显示「最近测速时间」；组内「单独测速」与全局「全部测速」；
- 节点多时不一次性渲染全量：虚拟滚动或分页（v1 用「显示前 60 个 + 加载更多」）。

### 13.3 订阅页

- 卡片主行：名称、`使用中`、节点数、规则数、更新时间、下次更新时间；
- 用量条 + 到期日；剩余 <10% 黄色预警；到期 ≤7 天提醒；
- 更新失败：红色状态 + 原因 + 「重试」；
- 差异异常：红色横幅「本次更新 0 个节点，已保留旧配置」；
- 「导入订阅」为**弹窗**（名称 / URL / 更新周期 / 高级展开）；
- 提供「版本回滚」（最近 3 个版本）。

### 13.4 规则页

只读提示；列为：`序号 / 类型 / 内容 / 策略 / 来源(订阅名) / Provider`；`MATCH` 行标记「兜底」徽标并固定置底展示。

### 13.5 连接页

列：主机 / 协议(TCP/UDP) / 规则与出口链 / 进程 / 上行 / 下行 / 时长 / 断开；顶部：搜索域名、`只看当前节点`、`全部断开`（**二次确认**）。

### 13.6 日志页

级别筛选、关键字过滤、暂停/继续、自动滚动、复制、导出；错误行仅用颜色标识，不提供单独诊断入口（统一使用顶部「诊断」）；所有日志在 Rust 侧脱敏（URL query、密码、UUID、token）。

### 13.7 设置页（分组）

| 分组 | 内容 |
|---|---|
| 代理端口 | mixed-port、控制器端口（自动探测提示） |
| 系统代理 | 开关、当前网络服务（macOS 可选）、系统代理协议 HTTP/HTTPS/SOCKS（附说明：仅影响写入 OS 的条目，不限制内核协议） |
| **系统代理恢复** | 退出时恢复、异常退出自动恢复、当前快照状态、立即恢复按钮 |
| 启动 | 开机自启、启动时自动连接 |
| **订阅与更新** | 默认更新周期、更新失败通知、自动连接上次节点 |
| 高级 | 测速 URL/超时、**DNS（基础开关 + 可选上游覆盖 + 只读生效预览 + 来源说明）**、日志级别、数据目录 |
| 内核 | 版本、PID、重启、打开数据目录、查看内核日志 |
| 关于 | 仅显示「内置 mihomo 内核」，不展示许可证/源码/SHA-256 |

**DNS 在设置中的形态**（不再有独立页）：

- 默认：`继承订阅配置`（只读展示「DNS 段来自订阅：8 个 nameserver / fake-ip 开」）；
- 可选：`启用 DNS 增强` 开关 → 仅写 `dns.enable`；
- 可选：`覆盖上游 DNS` 开关 + 一个多行输入框（写 `dns.nameserver`）；
- 折叠的「查看生效配置」：只读 YAML 片段；
- 明确提示：**纯系统代理模式下 DNS 不保证被全部接管**，修改可能对部分应用无效。

## 14. 安全与合规

- 仅 `127.0.0.1`；`allow-lan=false`；`bind-address=127.0.0.1`；剥离 `tun`/`listeners`；
- secret 32 字节随机，0600，只在 Rust 内使用；
- 订阅 URL/密码/UUID/token 脱敏（日志、UI、导出日志）；
- 系统代理快照与恢复是**最高优先级安全路径**，失败必须可见可重试；
- `osascript` 参数白名单 + 单引号转义，禁止拼接任意用户字符串；
- 设置页「关于」只显示「内置 mihomo 内核」，不展示许可证与源码链接（按产品要求精简）。

## 15. 与 mdgo 集成

1. `DASHBOARD_BUTTON` 增加 `{ key: '🌐 VPN', value: 'toggleFile("vpn")', display: true, type: 'C' }`；
2. `#vpn-container`（普通 div，加入 `MAIN_VIEW_CONTAINERS`），由 `css_js/modules/vpn.js` 挂载；
3. `toggleFile('vpn')` → `openVpnPage()`（`window.vpnApp.mount`）；`destroyEverything()` → `destroyVpnPage()`（`destroy()` 解绑事件/清定时器，**不停内核/不动系统代理**）；
4. 不再需要 iframe 与 postMessage 桥。
5. Rust：`commands/vpn.rs` 注册；`setup` 初始化 `VpnServices` + 崩溃恢复检测；`tray.rs` 增加 VPN 菜单；`RunEvent::ExitRequested` 收尾。

## 16. 错误处理

| 场景 | 处理 |
|---|---|
| 配置非法 | `mihomo -t` stderr 摘要 + 「查看详情」；保留旧配置 |
| 端口占用 | 自动换端口；全被占 → 明确列出端口 |
| 内核启动超时 | kill + `Error`，指向日志页（过滤 error） |
| 系统代理设置/恢复失败 | 回滚 + 顶部常驻横幅 + 重试 |
| API 401 | secret 失配 → 重启内核 |
| 订阅 403/429/HTML | 分类文案 + 保留旧配置 |
| 网络断开/换网 | `NetworkMonitor` 修复流程，不重启内核优先 |

## 17. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| **M0** | 设计稿评审（本轮） | 设计确认 |
| M1 | 状态模型 + CoreSupervisor + ConfigBuilder + SystemProxyGuard + 顶部状态条 + 原生容器视图 | 状态可区分「内核运行/系统代理/连通」；崩溃后系统代理自动恢复 |
| M2 | 代理页（策略组+节点网格+测速）+ 订阅页（导入/更新/差异/回滚） | 常规订阅可完整使用 |
| M3 | 规则 / 连接 / 日志 | 排错链路可用 |
| M4 | 设置（含 DNS 高级）、诊断面板、NetworkMonitor、托盘、自启 | 换网/休眠后自动修复；诊断能定位「内核好但网络不通」 |
| M5 | 打包（内核二进制按需放置）、macOS 授权联调、兼容性测试样本 | 双端安装包跑通全流程 |

## 18. 测试计划

- **订阅兼容性样本**（放 `tests/fixtures/vpn/`）：普通 Clash YAML、Mihomo 全量字段、含 `proxy-providers`、含 `rule-providers`、复杂 `dns`、多 `proxy-groups`、300+ 节点、含未知字段、过期/无效订阅、返回 HTML 的订阅、8MB+ 超大响应；
- 每个样本断言：生成配置 → `mihomo -t` 通过 → 启动后 `/proxies` 可读；
- **系统代理**：Windows 三字段快照/恢复、PAC 场景、外部改动检测；macOS 多服务、换网重置、恢复后原值一致；
- **崩溃注入**：kill -9 内核 → 断言系统代理先恢复；kill -9 应用 → 下次启动自动恢复；
- **网络切换**：断网→恢复、切 Wi-Fi、休眠唤醒（模拟 tick 间隔）；
- **桥**：白名单拒绝、超时、重复 id、页面卸载清理。

## 19. 风险

| 风险 | 缓解 |
|---|---|
| 系统代理恢复失败导致断网 | 快照 + 校验 + 启动自动恢复 + 常驻告警 |
| 订阅格式千差万别 | 原样透传 + `mihomo -t` 兜底 + 样本回归 |
| macOS 权限弹窗频繁 | 仅在开/关各弹一次；记住服务；文档说明 |
| 内置二进制体积 | 只内置目标平台；后续可改首启下载 |
| 节点过多卡顿 | 分页/虚拟滚动 + 测速并发限制 |

## 20. 评审意见落实对照表

| 评审意见 | 处理 |
|---|---|
| P0 系统代理与内核状态分离 | ✅ 第 3 节三态模型 + 第 3.3/3.4 流程；崩溃先恢复代理 |
| P0 订阅合并策略有风险 | ✅ 第 6 节改为「原文 + 最小覆盖表」，移除通用 merge；DNS 不再被覆盖 |
| P0 DNS 能力重新界定 | ✅ 取消一级 tab；默认透传；仅保留开关 + 可选上游；明确系统代理下 DNS 局限 |
| P0 系统代理跨平台加强 | ✅ 第 10 节：快照全字段、PAC/AutoDetect 保护、冲突检测、回滚、校验、macOS 多服务与注入防护、WinInet 说明 |
| P1 进程管理补充 | ✅ 第 4/3.5 节：异步读管道、PID 身份校验、启动前配置测试、优雅退出、串行化、模块拆分 |
| P1 订阅功能补充 | ✅ 第 7 节：差异检测、失败保留、到期/流量提醒、大小限制、脱敏、版本回滚、HTML 识别 |
| 页面缩减 6 tab | ✅ 第 2/13 节：DNS 移入设置 |
| 网络连通性诊断 | ✅ 第 12 节 + 顶部「诊断」入口 |
| 桥安全与通用性 | ✅ 第 9 节：白名单、超时、重复 id、订阅管理、版本、payload 校验、命名空间注册 |
| 视图不控制 VPN 生命周期 | ✅ 第 9.2 节 |
| 顶部状态条调整 | ✅ 第 13.1 节：代理 Switch + 彩色内核/系统代理状态 + hover 详情 + 当前节点 + 仅保留速率（无曲线） |
| 代理页重做 | ✅ 第 13.2 节：左组右网格、搜索、排序、测速状态、分页 |
| 订阅/规则/连接/日志/设置优化 | ✅ 第 13.3–13.7 节 |
| 网络切换与休眠恢复 | ✅ 第 11 节 |
| 订阅兼容性测试 | ✅ 第 18 节 |

## 21. 附录

### 21.1 端口默认

| 项 | 默认 | 可改 |
|---|---|---|
| mixed-port | 7890 | ✅ 自动探测 |
| external-controller | 127.0.0.1:9090 | ✅ 自动探测 |
| 测速 URL | `https://www.gstatic.com/generate_204` | ✅（高级） |
| 诊断 URL | 同上 | 固定 |

### 21.2 数据目录

| 平台 | 路径 |
|---|---|
| macOS | `~/Library/Application Support/mdgo/vpn/` |
| Windows | `%APPDATA%\mdgo\vpn\` |

### 21.3 相关文档

- 文档：`docs/VPN 模块实现说明.md`（实现与集成说明）
- 设计文档：本文件


---

## 22. 实现状态（已落地）

设计到此已全部实现，代码结构与验证方式见 [VPN 模块实现说明](./VPN%20模块实现说明.md)。

| 设计项 | 状态 | 实现位置 |
|---|---|---|
| 三态状态模型 + 事件推送 | ✅ | `core/vpn/mod.rs`（`VpnState` / `publish`） |
| 最小受管配置 + `mihomo -t` | ✅ | `core/vpn/config.rs` |
| 进程管理（异步读管道 / 3s 超时强杀 / 崩溃自愈） | ✅ | `core/vpn/process.rs`、`mod.rs` |
| 系统代理守卫（Windows 注册表 / macOS networksetup） | ✅ | `core/vpn/system_proxy.rs` |
| 订阅下载 / 版本 / 差异 / 定时更新 | ✅ | `core/vpn/profiles.rs` |
| mihomo API（proxies/rules/connections/logs/traffic） | ✅ | `core/vpn/api.rs` |
| 五项诊断 | ✅ | `core/vpn/diagnostics.rs` |
| 网络变化检测与最小修复 | ✅ | `mod.rs`（`spawn_network_monitor`） |
| 孤儿内核清理 + Ctrl+C 收尾 | ✅ | `mod.rs`（`kill_stale_cores`/`cleanup_stale_cores`）、`lib.rs`（`ctrl_c`） |
| 前端原生视图（CSS + JS 两个文件） | ✅ | `css_js/modules/vpn.css` + `css_js/modules/vpn.js` |
| 6 tab 界面 + 弹窗 | ✅ | 同上 |
| dashboard 入口 + 容器挂载/清理 | ✅ | `main.html`（`toggleFile('vpn')`、`openVpnPage/destroyVpnPage`） |
| 托盘菜单（状态 / 开关 / 路由模式） | ✅ | `tray.rs` |
| 开机自启（Run 键 / LaunchAgent） | ✅ | `core/vpn/settings.rs`、`lib.rs`（`--autostart` 静默进托盘） |
| 关于信息 | ✅ | 设置页仅显示「内置 mihomo 内核」 |

**未实现（明确超出本期范围）**：内核二进制自动下载/更新、SHA-256 强校验、Windows PAC 改写、TUN。

## 23. 回归要求：全量、闭环、可验收

> 本章是**强制要求**，不是建议。任何代码改动合并前，必须证明「所有功能仍闭环」：
> 入口 → 正常路径 → 异常/边界 → 恢复或重试 → 资源清理（进程 / 系统代理 / 文件 / 监听 / 定时器 / 事件订阅）。

### 23.1 闭环判定标准

一个功能只有同时满足以下五点才算“完整”：

1. **有入口**：UI 入口或命令/事件入口明确，且状态可见；
2. **有成功路径**：返回明确结果，状态机推进到终态，持久化正确；
3. **有失败路径**：超时、端口占用、配置非法、下载失败、权限拒绝、内核崩溃等均有可读错误，不 panic、不静默失败；
4. **有恢复**：失败后可重试/回滚/自动修复；崩溃后能自愈；退出后无残留；
5. **有清理**：进程、系统代理、临时文件、监听、定时器、事件订阅全部释放。

### 23.2 全量功能闭环矩阵

| # | 功能 | 成功路径 | 失败 / 边界 | 恢复 / 清理 | 自动化 |
|---|---|---|---|---|---|
| 1 | 连接 | 配置生成 → `-t` → spawn → `/version` 就绪 → 系统代理 → `connected` | 端口占用（自动换端口）、配置非法、二进制缺失、启动超时、系统代理失败 | 回滚系统代理、停止内核、`failed` 可重试 | `cargo test --lib vpn` + 手工 |
| 2 | 断开 | 恢复代理 → 停 SSE → 停内核 → `disconnected` | 恢复失败保留快照、内核已死 | 幂等、超时强杀、清 pid 记录 | 手工 + 单元 |
| 3 | 崩溃自愈 | 3s 检测到 pid 消失 → 恢复代理 → 自动重启 | 连续 3 次失败停止；稳定 120s 清零计数 | 重启后回到 `connected` | 手工杀进程 |
| 4 | Ctrl+C / 强杀 | 信号 → 恢复代理 → 停内核 → 退出 | 信号丢失/强杀 → 下次启动清理孤儿 | pid 记录 + 启动扫描清理 | `stale_core_process_is_killed`（ignored） |
| 5 | 系统代理 | Windows 注册表 / macOS networksetup 开启并校验 | PAC 拒绝、macOS 授权取消、服务名变化 | 快照 + `clean_exit` + 崩溃恢复 | 手工矩阵 |
| 6 | 订阅导入 | URL/本地 YAML → 校验 → `-t` → 保存 | 8MB/重定向/HTML/0 节点/重复 id/路径穿越 | 失败保留旧配置、退避重试 | 单元 + 手工 |
| 7 | 订阅更新 | 下载 → 校验 → 版本备份（3 份） | 网络失败、节点骤减、版本回滚 | 失败不覆盖、`fail_count` 退避 | 单元 + 集成 |
| 8 | 代理页 | 策略组/节点、切换、测速、定位、搜索排序 | API 超时、组为空、节点重名 | 保留旧数据、错误提示 | 手工 |
| 9 | 规则/连接/日志 | 分页读取、搜索、断开、清空、复制 | 内核未运行、字段缺失、XSS 注入 | 转义、脱敏、增量渲染、暂停 | 单元（脱敏/转义）+ 手工 |
| 10 | 诊断 | 5 项检查 + 结论 + 建议 | 每项超时、部分失败 | 可重复执行，不改变状态 | 手工 |
| 11 | 设置 | patch 语义、端口/DNS/进程列/自启 | 非法端口、`dns_enable_clear`、自启失败 | 失败回滚提示、重启内核 | 单元 + 手工 |
| 12 | 托盘 / 自启 | 菜单状态同步、`--autostart` 静默 | 托盘创建失败降级、Run 键失败 | 无托盘模式、错误提示 | 手工 |
| 13 | 前端视图 | mount → 订阅 → 轮询 → 6 tab | Tauri 未就绪、接口失败、空订阅 | `destroy()` 解绑/清定时器、重试 | `node --check` + 浏览器联调 |
| 14 | 退出 | 有界等待、恢复代理、停内核 | 锁超时跳过恢复（快照保留） | 不阻塞主线程、无孤儿 | 手工 Ctrl+C |

### 23.2.1 命令级回归清单（29 个）

> 每个命令至少覆盖：正常返回、参数非法/内核未运行、超时/断开、清理或状态回滚。
> `vpn_delay`、`vpn_network_state` 目前无 UI 入口，仍需在集成测试中覆盖。

| # | 命令 | 正常路径 | 失败/边界 |
|---|---|---|---|
| 1 | `vpn_state` | 返回完整三态 + 端口/网络 | 内核未启动时字段完整、不 panic |
| 2 | `vpn_connect` | 完整连接时序 | 无订阅/无内核/端口占用/`-t` 失败 |
| 3 | `vpn_disconnect` | 恢复代理 + 停内核 | 重复断开幂等、内核已死 |
| 4 | `vpn_restart_core` | 重启并恢复 connected | 未连接时仅启动、失败回滚 |
| 5 | `vpn_diag_run` | 5 项检查 + 结论 | 单项超时/部分失败 |
| 6 | `vpn_proxies` | 策略组/节点/延迟 | 内核未运行、空组 |
| 7 | `vpn_select_proxy` | 切换并持久化 | 组/节点不存在、内核拒绝 |
| 8 | `vpn_delay` | 单节点延迟 | 超时、节点不存在 |
| 9 | `vpn_group_delay` | 整组延迟 | 组不存在、部分超时 |
| 10 | `vpn_mode_set` | rule/global/direct | 非法模式、内核拒绝 |
| 11 | `vpn_rules` | 规则列表（含 MATCH） | 空规则、解析失败 |
| 12 | `vpn_connections` | 连接快照 + 进程列 | 内核未运行、字段缺失 |
| 13 | `vpn_kill_connection` | 断开指定连接 | id 不存在、内核拒绝 |
| 14 | `vpn_kill_all` | 全部断开 | 无连接、内核拒绝 |
| 15 | `vpn_logs` | 级别/limit 过滤 | 非法 level、limit 0/超大 |
| 16 | `vpn_logs_clear` | 清空环 | 重复清空 |
| 17 | `vpn_profiles` | 列表 + active_id | 空目录、坏 profile 跳过 |
| 18 | `vpn_profile_add` | URL/本地导入 + 校验 | 非法 URL/路径、0 节点、重复 |
| 19 | `vpn_profile_update` | 下载 + 版本备份 | 断网/HTML/超限/退避 |
| 20 | `vpn_profile_remove` | 删除文件 + 历史 | 不存在、路径穿越拒绝 |
| 21 | `vpn_profile_activate` | 切换并重载 | 配置非法、内核重启失败 |
| 22 | `vpn_profile_versions` | 版本列表 | 无版本、坏元数据 |
| 23 | `vpn_profile_rollback` | 回滚到指定版本 | 版本不存在、校验失败 |
| 24 | `vpn_settings_get` | 返回设置 + 快照状态 | 文件损坏用默认值 |
| 25 | `vpn_settings_set` | patch 合并 + 必要时重启 | 非法端口、`dns_enable_clear`、权限失败 |
| 26 | `vpn_sysproxy_set` | 开启/关闭 + 校验 | PAC/授权取消/服务名变化 |
| 27 | `vpn_network_state` | online/offline | 网络切换、无默认路由 |
| 28 | `vpn_open_data_dir` | 打开目录 | 目录不存在时创建 |
| 29 | `vpn_pick_yaml` | 选择本地 YAML | 取消选择返回 None |

### 23.3 回归层级与命令

```powershell
# 1) Rust 编译（0 error / 0 warning）
cd tauri/src-tauri
cargo check --lib

# 2) 单元测试（27 个，覆盖配置/路径/脱敏/流解析/版本/退避等）
cargo test --lib vpn

# 3) 真实内核 API 冒烟（需本机 mihomo，Windows 必跑）
cargo test --lib real_core_api_smoke -- --ignored --nocapture

# 4) 孤儿内核清理回归（需本机 mihomo）
cargo test --lib stale_core_process_is_killed -- --ignored --nocapture

# 5) 流量/日志流解析回归（需本机 mihomo）
cargo test --lib real_core_traffic_stream_yields_events -- --ignored --nocapture

# 6) 前端语法
node --check css_js/modules/vpn.js
node <inline-script-checker> main.html

# 7) 构建（dist 必须包含 vpn.css/vpn.js，且无 vpn.html）
cd tauri
npm run build
```

### 23.4 Windows 手工回归（每次发版必做）

1. 空订阅首启 → 显示空态 → 导入 URL → 自动激活 → 代理页有节点；
2. 开启代理 → 系统代理生效（浏览器可访问）→ 切换节点 / 测速 / 模式切换；
3. 任务管理器杀掉 mihomo → 3s 内自动重启并恢复系统代理；
4. **调试模式 Ctrl+C 中断 → 任务管理器确认无 mihomo → 重新启动 → 只有一个 mihomo**；
5. 断开代理 → 系统代理恢复原值；退出应用 → 无残留 mihomo；
6. 订阅更新失败（断网）→ 旧配置仍可用、提示失败、退避生效；
7. 规则/连接/日志页搜索、断开单条/全部、复制/清空日志；
8. 诊断全部通过 / 人为断网后诊断给出建议；
9. 设置改端口 → 自动重启内核；开启“连接页显示进程” → 连接页出现进程列；
10. 托盘开关/模式与主界面状态一致；开机自启静默进托盘。

### 23.5 合并门槛

- `cargo check` 0 warning；`cargo test --lib vpn` 全绿；3 个 ignored 测试本机通过；
- 前端两个静态检查 + `npm run build` 通过；
- 23.2 矩阵中每行至少验证一次，失败路径必须有截图/日志证据；
- 不允许新增“只做成功路径”的功能；不允许遗留孤儿进程、未恢复的系统代理、未清理的定时器/事件。

---

## 24. 高可用设计

### 24.1 故障检测与恢复矩阵

| 故障 | 检测方式 | 恢复动作 | 用户可见结果 |
|---|---|---|---|
| 内核崩溃/被杀 | 3s 轮询 pid | 先恢复系统代理，再按 `intent` 自动重启（3 次上限） | 顶部“内核异常” → 自动恢复或提示 |
| 调试 Ctrl+C / 强杀 | 启动时 pid 记录 + 命令行扫描 | 杀掉指向本数据目录的遗留 mihomo | 下次启动只有一个内核 |
| 系统代理被外部改掉 | 每次状态刷新 + 网络变化事件 | 校验失败 → 标记 `degraded` → 一键/自动重开 | 顶部黄条 + 重开按钮 |
| 端口冲突 | 连接前探测 | 自动选择空闲端口并持久化 | 无需用户处理 |
| 配置写入中断 | 临时文件 + 原子 rename | 旧配置不被破坏 | 不出现半截 YAML |
| 配置非法 | `mihomo -t` | 拒绝激活/导入，返回 stderr 摘要 | 可读错误，旧配置保留 |
| 订阅下载失败 | 状态码/大小/HTML 检测 | 退避（`fail_count` + `last_attempt_at`） | 保留旧配置 + 失败提示 |
| 系统代理无法恢复 | 恢复后校验 | 快照保留，下次启动 `recover_after_crash` | 不丢原始配置 |
| 退出卡死 | 生命周期锁 3s 超时 | 跳过恢复（快照保留）后继续退出 | 应用能退出 |
| 前端接口失败 | invoke 超时/异常 | 保留旧数据 + toast + 重试 | 页面不白屏 |

### 24.2 高可用约束

1. **状态唯一事实源在 Rust**：前端只是投影，重开视图不丢状态；
2. **所有写操作幂等**：重复 connect/disconnect/update 不产生双进程、双快照；
3. **先恢复系统代理，再停内核**：任何路径不得颠倒；
4. **退出有界**：最多 3s 等锁、3s 停内核，绝不阻塞主线程；
5. **崩溃恢复可关闭**：`recover_on_crash` / `restore_on_exit` 由用户控制；
6. **孤儿清理只认本数据目录**：进程名含 `mihomo` 且命令行包含本目录 `cache/` 或 `config.yaml`，避免误杀其它客户端。

---

## 25. 性能与高并发设计

### 25.1 任务与线程模型

- 所有网络/进程/文件 IO 走 `tokio` 异步任务，Tauri 主线程只做命令转发与事件 emit；
- 内核 stdout/stderr 由独立 task 读取 → 有界日志环（最多 800 条）→ 批量/增量推送到前端；
- `/traffic`、`/logs` 使用 JSON Lines 长连接（mihomo 并非标准 SSE `data:` 流），断线 2s 重连；`/connections` 由前端 2s 轮询（仅当前 tab）；
- 订阅下载与更新在后台任务串行执行，UI 不阻塞；
- 系统代理、配置生成、`mihomo -t` 为短时阻塞操作，放在 `spawn_blocking`/独立任务，不占用异步 worker 过久。

### 25.2 性能预算（目标值）

| 场景 | 目标 |
|---|---|
| 打开 VPN 视图（本地数据） | < 150ms 首屏，状态加载 < 1s |
| tab 切换 | < 50ms（本地渲染） |
| 连接端到端 | 常规 < 3s，含 `-t` 校验 < 8s |
| 断开 | < 2s（含系统代理恢复） |
| 日志推送到界面 | 单条 < 16ms，批量合并，最多保留 800 条 |
| 连接页轮询 | 2s 一次，窗口不可见时暂停 |
| 代理列表刷新 | 15s 一次，仅代理 tab 且内核运行 |
| 大配置（10MB / 5000 节点） | 解析不 OOM，UI 分页/限量（首屏 120 节点） |

### 25.3 并发安全清单

1. `lifecycle` 锁串行化 connect/disconnect/restart，避免双内核；
2. 配置 reload 使用 `reload_lock`，同一时刻只有一个重载；
3. `std::sync::Mutex` 临界区不跨 `.await`；需要异步时先取快照再释放；
4. `generation` 代际号让过期的流/日志/请求结果自动失效；
5. `shutting_down` 原子标志保证退出一致性，二次调用立即返回；
6. 日志通道有界 + 前端二次限流（`logsPaused`、级别过滤、最多 600 行 DOM）；
7. 事件订阅只注册一次，`destroy()` 必须 unlisten，避免重复渲染；
8. 所有 `await` 外部调用带超时（invoke 8–90s、HTTP 15s、进程停止 3s）。

---

## 26. SOLID 对照

| 原则 | 落地方式 | 证据 |
|---|---|---|
| **S** 单一职责 | 配置生成 / 进程 / API / 订阅 / 系统代理 / 诊断 / 设置 / 路径各自独立模块；`commands/vpn.rs` 只做门面 | `core/vpn/*.rs` |
| **O** 开闭 | 系统代理按平台分支；诊断检查项可扩展；事件类型集中注册；新增 tab 只加渲染函数 | `system_proxy.rs`、`diagnostics.rs`、`VPN_TABS` |
| **L** 里氏替换 | 平台实现返回统一的 `SystemProxyState`；Windows/macOS 行为差异不改变调用方契约 | `types.rs` |
| **I** 接口隔离 | 29 个命令按功能拆分，前端只依赖用到的命令；不暴露内核 secret | `commands/vpn.rs` |
| **D** 依赖倒置 | 前端 → 命令门面 → `VpnManager` → 平台模块；核心不依赖前端；平台差异收敛在 `system_proxy` | 依赖单向 |

### 26.1 依赖方向

```
vpn.js（视图）
   ↓ invoke / listen
commands/vpn.rs（门面，参数校验 + 错误映射）
   ↓
core/vpn/mod.rs（状态机 / 编排）
   ├── config.rs / process.rs / api.rs
   ├── profiles.rs / diagnostics.rs / settings.rs / paths.rs
   └── system_proxy.rs（平台实现）
```

禁止反向依赖：`core` 不引用 `commands` / 前端；`api.rs` 不直接改状态。

### 26.2 扩展点

| 扩展 | 做法 |
|---|---|
| 新增平台系统代理 | 在 `system_proxy.rs` 增加 cfg 分支，保持 `enable/disable/verify` 契约 |
| 新增诊断项 | 在 `diagnostics.rs` 的检查列表追加，前端无需改动 |
| 新增 tab | `VPN_TABS` + `vpnRenderXxx` + 数据加载函数 |
| 新增设置项 | `Settings` 字段 + `patch` 合并 + 设置页 `.setting-row` |
| 新增事件 | `vpn://*` 注册 + 前端 `vpnOnEvent` 分支 |

---

## 27. 缺陷回归记录

### 27.1 调试模式 Ctrl+C 遗留 mihomo

**现象**：调试模式 `Ctrl+C` 中断后，后台残留 mihomo；再次启动出现两个 mihomo。

**根因**：
1. `Ctrl+C` 直接终止调试进程，`RunEvent::ExitRequested` 的收尾逻辑不执行，子进程未 `terminate`；
2. 原实现没有启动时孤儿检测，旧进程继续占用配置端口/控制器；
3. 新实例启动时只会新建内核，不会感知旧内核。

**修复**：
1. `VpnManager::cleanup_stale_cores()`：启动时 + 每次连接前扫描进程，匹配“进程名含 mihomo 且命令行包含本数据目录 cache/config”后强杀；
2. spawn 成功后写 `mihomo.pid`（pid/exe/config），所有停止/崩溃路径清空；
3. `lib.rs` 注册 `tokio::signal::ctrl_c()`：先 `shutdown()`（恢复系统代理 + 停内核）再退出；
4. `find-process-mode` 改动改为重启内核，避免热重载不生效；
5. 回归测试：`stale_core_process_is_killed`（ignored，需要本机 mihomo）。

**验收**：
- 调试模式 `Ctrl+C` → 任务管理器无 mihomo；
- 再次启动 → 仅一个 mihomo，系统代理状态正确；
- `cargo test --lib stale_core_process_is_killed -- --ignored` 通过。


### 27.2 `/traffic`、`/logs` 流解析错误（速率恒 0、曲线横线）

**现象**：顶部上传/下载始终 `0 B/s`，速率曲线是一条横线；日志页也可能一直空白。

**根因（两层）**：mihomo 的 `/traffic` 与 `/logs` 返回的是 **JSON Lines**（每行一个 JSON，例如
`{"up":150,"down":1866,"upTotal":150,"downTotal":1866}`），既没有 SSE 的 `data:` 前缀，也没有空行分隔。
旧实现按标准 SSE 找 `\n\n` 分隔符再取 `data:`，因此永远拼不出完整事件，SSE 任务空转。

第二层：即使收到事件，旧曲线用固定 `KB/s × 2` 映射到 20px 高度，真实流量（>9KB/s）全部触顶，仍然是一条横线。

**修复**：
1. 新增 `take_line`：按 `\n` 切行，兼容 `\r\n`，同时接受可选的 `data:` 前缀；
2. `/traffic`、`/logs` 都改为逐行解析 JSON，不再依赖空行；
3. 长连接使用独立的“无总超时”client（普通请求仍有超时），避免 SSE 被周期性掐断；
4. 曲线按窗口峰值归一化（后续 UI 重设计已移除曲线，仅保留速率）；
5. 保留断线 2s 重连与代际检查。

**验证**：
- 单元测试 `take_line_accepts_json_lines_and_sse`（JSON Lines + SSE 两种格式）；
- 真实内核回归 `real_core_traffic_stream_yields_events`（ignored，需要本机 mihomo）：启动内核后 6s 内必须收到流量事件；
- 手工：浏览器/下载产生流量时顶部速率变化，日志页有实时日志（后续 UI 重设计已移除曲线）。
