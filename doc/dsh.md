# DSH (DeepSeek Harness) 集成文档

## 概述

DSH 是 DeepSeek 的 AI 编程助手，运行在浏览器中（默认 `http://127.0.0.1:3080`）。本文档记录
Agent Deck Companion 与 DSH 的集成方案：session 切换的工作原理、功能键适配和遗留问题。

## Session 切换工作原理（简版）

DSH 的"当前选中 session"是浏览器客户端状态，服务端没有 select RPC，所以 companion 无法直接
调用 API 切换。解决方案是给页面一个"桌面侧可触达的信号"：

1. **信号**: Companion 向 DSH 的 `/api/deck/select` 发送 session ID；浏览器页面通过
   `/api/deck/next` 长轮询从宿主插件取得指令。
2. **消费**: 页面内的 `dsh-deck-bridge` 调用 DSH 内部导航 API（0.1.7+ 为
   `uiWorkspace.openSession(sessionId)`，旧版本为 `sessions.open(sessionId)`）并选中
   Chat 面板，再通过 `/api/deck/ack` 回执。只有收到回执才算切换成功。
3. **失败处理**: HTTP 无回执时 K1-K3 返回失败，不调用 AppleScript 或模拟鼠标。
4. **呈现**: 切换成功后把 DSH tab 带到前台（`focus_browser_tab`）：按 URL 前缀在浏览器窗口里
   找到 DSH 页签并切过去，而不是只 `open -a` 唤醒浏览器（那样只会停在最后活跃的无关页签）。

K1/K2/K3 对应 panel 1/2/3，session ID 由 panel 携带（来自 `~/.dsh/storages` 的文件读取）。

## 已完成功能

### 1. 进程检测 ✅
- **文件**: `src-tauri/src/dsh.rs`
- **机制**: 通过 `sysinfo` 扫描进程列表，匹配 `deepseek-harness` 进程名
- **Web 判定**: `web_url()` 探测 `http://127.0.0.1:<port>` 上的 DSH 服务

### 2. Session 状态读取（两层存储）✅
- **文件**: `src-tauri/src/dsh.rs` - `read_sessions()` / `read_status()`
- **数据源**:
  - `~/.dsh/storages/session_projcache/sessions/*.json` - 单独的 session 文件（**当前 DSH 版本的唯一实时来源**）
  - `~/.dsh/storages/session_projcache.json` - 聚合缓存（旧版本才写；缺失时不得让状态轮询停摆）
- **读取内容**: session ID（文件名）、标题、工作状态（working/idle/needs_input）、cwd、
  最后活动时间（lastPromptAt）；先读单独文件再补聚合缓存，HashSet 去重
- **状态判定**: `sessionStats.openStep` 为对象 → working；`goal.val.current.goal.phase == "blocked"`
  → needs_input；否则 idle。注意 `goal.val` 是投影状态 `{current, seenGoalIds, failure}`，
  目标对象在 `current.goal` 下（旧写法 `goal.val.goal` 会让 blocked 永远读不到）
- **Agent 级状态**: `read_status()` 取最近一个非空 session 的状态/标题/cwd；聚合缓存只作为兜底

### 3. Connection RPC 客户端 ✅
- **文件**: `src-tauri/src/dsh.rs` - `DshApiClient`
- **功能**: `POST /api/session/list` 一元 RPC；HMAC-SHA256 签名 Cookie 认证
  （密钥来自 `~/.dsh/.credentials.yaml`）
- **状态**: 已用本机 DSH 0.1.5-rc.2 实测（HTTP 200、标准 `server-response`）

> `session/list` 是 HTTP unary RPC，不是 WebSocket JSON-RPC；
> `/api/remote.mux` 只承载 `session/follow` 等 stream endpoint。

### 4. Session 列表同步与轮询 ✅
- **文件**: `src-tauri/src/monitor.rs` - `sync_dsh_panels()` / `poll_dsh_status()`
- **机制**: 每 5 秒读取 session 状态；按最后活动时间排序，严格投影最新三个到 slot 1/2/3
  （每轮先将 DshWeb panel 置 Offline 再投影，避免旧 label 残留；变更检测含 slot）
- **不依赖聚合缓存**: Agent 级状态取不到时也要继续同步 panel，否则删掉最后一个 session 后
  旧行会一直留到下一次 60 秒的进程 reconcile

### 5. K1-K3 Session 切换 ✅
- **主路径**: `dsh.rs` - `request_session_selection()` + `dsh-deck-bridge/` 的 HTTP 长轮询/ACK 通道
  （原理见上文"Session 切换工作原理"）
- **权限边界**: K1-K3 不使用 Apple Events、辅助功能树或模拟鼠标；页面内插件调用 DSH 自身服务
- **入口**: `activation.rs` - `activate_panel()`，transport 为
  `PanelTransport::DshWeb { session_id }`
- **成功标志**: `acked=true`

### 6. K4-K10 功能键（Web surface）✅
- **配置**: `plugins/builtin/dsh/plugin.json`（运行时优先）+ `agent_actions.rs`
  `DSH_WEB_BINDINGS`（两处必须一致）
- **映射**: K4 OK(`y`+Enter) / K5 NO(`n`+Enter) / K6 COMPACT(`/compact`) /
  K7 PLAN(`/plan`) / K8 EXPORT(`/export`) / K9 SEND(Enter) / K10 禁用
- **执行路径**: `activation.rs` - fn_key 先聚焦 DSH browser tab，再调用
  `dsh.rs` - `focus_chat_composer()`（AppleScript Accessibility 语义点击输入框：优先底部
  dock 坐标，命中非 `AXTextArea` 或空白 session 的居中 hero composer 时回退到语义
  `AXTextArea` 聚焦），然后通过 `macos_input.rs` 逐字符 CGEvent 输入 + Enter。
  动作本身只是"打字 + 回车"的 `ActionPlan::TextInput`，与 Qwen/Claude 终端路径同构
- **状态**: 斜杠命令依赖 DSH 前端命令名；`/export` 若命令不存在需调整字符串

### 7. Surface 检测与自动激活 ✅
- **文件**: `activation.rs` - `resolved_surface()`；`lib.rs`
- DSH 即使在 Herdr pane 内运行也优先解析为 `Web`（只要有 DSH web 服务在跑）；
  启动时自动激活第一个运行中的 agent（qwen > herdr > dsh > codex > chatgpt > claude）

### 8. RwLock poison 恢复 ✅
- 所有 `.expect("snapshot poisoned")` 改为 `into_inner()` 恢复，单线程 panic 不拖垮全局

### 9. 插件主动上报（0.4.0 起）✅
- **方向**: `dsh-deck-bridge` 宿主插件 → `POST 127.0.0.1:34115/api/dsh/push`
- **触发**: 订阅 DSH 自己的 `api-session/added|removed|status|activity|error`（浏览器侧边栏渲染用的同一份事件）
- **语义**: 只发"去看一眼"的 `{reason, sessions}`，不带状态；companion 用同一个读取器重新采样，
  DSH 语义只有一份实现，丢一次推送只损失延迟
- **鉴权**: `x-agent-deck-token`，令牌来自 companion 写的 `~/.agent-deck-companion/hook-token`（0600）
- **节流**: 400ms 合并；单次超时 800ms；连不上静默降级，不影响 DSH
- **轮询兜底**: 收到推送后 30 秒轮询兜底，推送停止后自动回到 5 秒
- **可见性**: `dsh_diagnostics` 返回 `pushes` / `last_push_at_ms`，设置页显示「已收到 N 次推送」

## 技术架构

```
DSH (浏览器, http://127.0.0.1:3080)
  ├─ Unary RPC: POST /api/session/list (HMAC Cookie)
  ├─ Stream mux: ws /api/remote.mux (session/follow)
  └─ dsh-deck-bridge 插件（HTTP 长轮询/ACK → uiWorkspace.openSession）
        │  ▲ /api/deck/select                  api-session/* → POST /api/dsh/push
        │  │                                        │
Agent Deck Companion (Rust)                          │
  dsh.rs        read_sessions / read_status / web_url │
  │             request_session_selection（K1-K3 唯一切换路径）
  │             focus_chat_composer (功能键输入前聚焦)
  monitor.rs    poll_dsh_status (5s；收到推送后 30s 兜底) ◀──┘
  │             sync_dsh_panels（推送与轮询共用）
  activation.rs resolved_surface (DSH→Web)
  │             K1-K3: activate_panel → HTTP 长轮询/ACK
  │             K4-K9: focus tab → composer → macos_input::post(TextInput/Shortcut)
  http_api.rs   POST /api/dsh/push（token 鉴权，仅触发重采样）
  plugin.rs     plugins/builtin/dsh/plugin.json keymap（UI 标签 + 运行时优先）
  agent_actions DSH_WEB_BINDINGS（adapter 默认值）
```

## dsh-deck-bridge 插件：安装与验证

#### 插件结构（`dsh-deck-bridge/`）

| 文件 | 作用 |
| --- | --- |
| `package.json` | `dsh.bundle.patch` 指向 patch；`dsh.client` 声明 web 平台注入 |
| `cordis.patch.yml` | 向 profile 的 loader 树 `insert` 模块 `deck-bridge` |
| `lib/index.js` | 宿主 HTTP 路由：select、next、tabs、ack/nack |
| `lib/client.js` | 页面内实现：注入 `sessions` 与 `layout`，长轮询取指令，按版本调用 `uiWorkspace.openSession` / `sessions.open` 切换 session 与 Chat 面板后回 ACK |

旧页面仍可识别 `#deck-<nonce>-<session-id>`，但 Companion 的 K1-K3 已不发送 hash。
新路径用 `/api/deck/next` 和 `/api/deck/ack`；失败时页面回 `/api/deck/nack`。

#### DSH 0.2 兼容与权限

- `dsh-deck-bridge` 0.3.2 的 peerDependencies 支持 DSH `0.2.0-rc.2`；旧版插件仅声明
  0.1.x 时，DSH 插件管理器会拒绝安装。无需使用 `allow-version --accept-risk`。
- `/api/deck/select` 返回 **404** 表示桥接路由未加载，不是 macOS 权限错误。先检查
  web profile 的依赖和 bundles，再重启 DSH 并刷新页面。
- K1-K3 先通过 HTTP 切换并等待 ACK，成功后尽力聚焦标签页；切换不依赖 Input Monitoring、
  辅助功能或浏览器自动化授权。聚焦已有浏览器标签页仍使用 Apple Events，功能键输入仍使用辅助功能。

#### 安装步骤

**更简单的方式**:Companion 主窗口 → **Hooks** → 标题为 `DeepSeek Harness 插件`（带 `DSH 插件` 标记）的那一行 →「安装插件」。
Companion 会替你跑下面同一条 `npx` 命令,并把 profile、版本、待重启状态显示在该行下方。
需要 DSH 正在运行(否则无法确定装到哪个 profile);`npx` 不在 PATH 时可用 `AGENT_DECK_NPX` 指定。

手工安装:

```sh
# 1. 安装到 web profile（dsh 默认不在 PATH，统一走 npx 入口；前缀用 link: 不要用 file:）
npx @deepseek-ai/dsh plugin --profile web add "link:$PWD/dsh-deck-bridge"

# 2. 重启 DSH Web（bundle 列表在服务启动时解析）
npx @deepseek-ai/dsh web

# 3. 刷新浏览器中的 DSH 页面（Cmd+R）
```

#### 验证 / 卸载

```sh
npx @deepseek-ai/dsh plugin --profile web list      # 确认已注册
# 或直接按 K1/K2/K3，观察 companion 日志
#   [dsh] http-select session=... acked=true       ⇒ 正常
#   [dsh] deck/select unacked: reason=...          ⇒ 查看浏览器 Console 和插件加载状态

npx @deepseek-ai/dsh plugin --profile web remove dsh-deck-bridge   # 卸载
```

`link:` 建软链，改插件源码后重启 DSH 即生效；`file:` 只在首次安装时拷贝一次，之后
`node_modules` 里一直是旧文件（详见 `dsh-deck-bridge/README.md`）。

## 已知限制

1. **切换依赖插件已加载**: 页面未加载桥接模块时 K1-K3 报错；插件按版本选择打开 session 的
   内部服务名（0.1.7+ 用 `uiWorkspace.openSession`，更早版本用 `sessions.open`），DSH 大版本
   升级仍可能变动
2. **状态更新延迟**: 文件轮询，最多 5 秒；未来可改用 `session/follow` WebSocket 订阅
3. **单实例**: 进程检测只返回第一个匹配的 DSH 实例
4. **功能键命令依赖前端**: `/compact`、`/plan`、`/export` 是 DSH 前端斜杠命令，命令改名
   时需同步 `plugin.json` 与 `agent_actions.rs`

## 后续 TODO

- [ ] **插件主动上报**已实现（0.4.0，见上文第 9 节）：订阅 DSH 自己的 `api-session/*` 事件，
  变化时 POST companion 的 `/api/dsh/push`，轮询降为兜底。更细的 `agent/status` / `session/event`
  暂不订阅（每个 turn / 每个 token 都发，且重采样时投影文件已更新）
- [ ] `session/follow` 实时状态推送（另一条路：companion 直接订阅 `/api/remote.mux`，无需插件）
- [ ] 通过 unary RPC 创建/归档 session（需 DSH 提供 RPC）
- [ ] 多实例支持
- [ ] 插件侧扩展更多动作（新建 session、重命名、回传当前选中项）；若 DSH 提供官方客户端
      导航 API 则替换 hash 信号通道

### 方案对比（决策记录）

| 特性 | 旧 Accessibility 方案 | 当前 HTTP 长轮询/ACK 插件 |
|------|-------------|---------|
| **可靠性** | ⚠️ 依赖 AX 语义与实时坐标 | ✅ 按 session ID 精确切换 |
| **维护性** | ⚠️ AX 角色变化需调整 | ⚠️ 依赖 cordis 内部服务名 `sessions` |
| **安装复杂度** | ✅ 无需安装 | ⚠️ 需安装插件并重启 DSH Web |
| **后台能力** | ❌ 需 tab 前台+真实点击 | ✅ 后台 tab 直接切换 |

## 调试指南

```bash
npm run tauri:dev    # 启动应用

# 观察日志
tail -f /tmp/agent_deck.log | grep -E "\[dsh\]|\[fn_key\]|\[activate\]|focus_session"

# 关键日志:
#   [dsh] http-select session=... acked=true       主路径成功
#   [dsh] deck/select unacked: reason=... detail=...   页面拒绝原因
#   [fn_key] → post plan=TextInput {...}          功能键已发送
#   [activate] agent=dsh ... surface=Web          surface 解析正确

cat ~/.dsh/storages/session_projcache.json | jq .   # session 数据
ps aux | grep dsh                                   # DSH 进程
```

DSH API 手动测试：主窗口 **设置 → Agent 监控 → DSH 集成** 的「重新检测」按钮
（`commands.rs::dsh_diagnostics` → `dsh.rs::diagnostics`），一次列出 DSH 目录、凭据、
进程、Web 地址、本地会话数、Connection RPC 结果和 deck 桥接的连接页面数。同一处可以
开关 DSH 监控：关闭后不再轮询 DSH 存储与状态，其会话行也会从列表移除。

## 相关文件

- `src-tauri/src/dsh.rs` - 进程检测、session 读取、RPC 客户端、HTTP 长轮询/ACK 切换、composer 聚焦、DSH 页签切换（`focus_browser_tab` + Chrome/Safari AppleScript）
- `src-tauri/src/monitor.rs` - session 同步和状态轮询
- `src-tauri/src/activation.rs` - K1-K3/K4-K10 按键处理、surface 检测
- `src-tauri/src/agent_actions.rs` - `DSH_WEB_BINDINGS` adapter 默认 keymap
- `src-tauri/src/macos_input.rs` - CGEvent 键盘注入（TextInput/Shortcut 执行）
- `src-tauri/src/commands.rs` - Tauri 命令（dsh_diagnostics）
- `plugins/builtin/dsh/plugin.json` - DSH 插件配置（运行时 keymap + UI 标签）
- `dsh-deck-bridge/` - HTTP 长轮询/ACK 桥接 web 插件（保留旧 SSE 路由供旧页面过渡）
- `src/routes/+page.svelte` / `src/lib/api.ts` - UI 与 API 封装

## 参考资料

- [DSH GitHub 仓库](https://github.com/deepseek-ai/deepseek-harness)
- [DSH 官方文档](https://deepseek-harness.github.io/deepseek-harness/)
- [Cordis 框架文档](https://cordisjs.com/)
- [dsh-sess 插件](https://github.com/deepseek-ai/deepseek-harness/tree/main/plugins/dsh-sess) - 参考实现

## 变更记录

### 2026-10-16
- ✅ 修复「DSH 只看得到 pid、看不到会话状态」：`read_status()` 原先只读
  `storages/session_projcache.json`，而当前 DSH 版本不写这个聚合文件（只写
  `session_projcache/sessions/*.json`），于是 `poll_dsh_status()` 一开始就 `return false`——
  5 秒轮询既不更新 Agent 状态，也不同步 session 行，`task`/`workspace` 永远是 null。
  现在 Agent 级状态取最近一个 session 文件，聚合缓存降级为兜底；panel 同步与状态解耦，
  每轮都执行
- ✅ 修复 goal 判定路径：`goal.val` 是投影状态 `{current, seenGoalIds, failure}`，
  目标在 `current.goal` 下；原实现读 `goal.val.goal.phase`，导致 `blocked` 永远读不到、
  needs_input 从不触发（三处重复实现改为共用 `current_goal()`）
- ✅ `parse_session_file` 与聚合缓存共用 `derive_session_status`，不再各自维护一份状态判定
- ✅ 把依赖本机 `~/.dsh` 会话标题的测试改为「存在就解析、不存在就跳过」，不再因某台机器的
  数据而长期红灯

### 2026-09-29
- ✅ 修复 DSH focus 只唤醒浏览器、不停留在 DSH 页签的问题：原先 `open -a "Google Chrome"`
  只激活应用，前台仍是最后活跃的无关页签。新增 `dsh.rs::focus_browser_tab`，按 `web_url()`
  前缀在窗口里定位 DSH 页签并切过去，找不到才退回只激活应用
- ⚠️ AppleScript 的 `tab`/`URL` 词表只在字面量 `tell application "Google Chrome"`（或
  `"Safari"`）块内可编译；`tell application <变量>` 会退化成泛化解释器并报语法错误，因此
  浏览器名必须写进脚本而非 argv，Chrome 与 Safari 各用一份模板（切页动词不同：
  `active tab index` vs `set current tab … to tab …`）

### 2026-09-28
- ✅ 修复 DSH 0.1.7 K1-K3 切换失败（`sessions.open is not a function`）：0.1.7 把 session
  导航从 `sessions` 移到 `uiWorkspace.openSession()`，插件改为运行时探测两者并兼容旧版本
- ✅ `layout.selectPanel(null)` 移入 `finally` 且独立捕获，面板清理失败不再误报 nack

### 2026-09-21
- ✅ DSH Web K6-K9 调整为 COMPACT/PLAN/EXPORT/SEND；移除 PAUSE/RESUME
- ✅ 新增 `dsh-deck-bridge/` web 插件，K1-K3 切换改为 hash 桥接主路径，
  Accessibility 点击降级为兜底；Chrome 实测通过
- ✅ 文档整理：移除已废弃的 `dsh-agent-deck-plugin/` 探索原型记录

### 2026-09-18
- ✅ 两层存储读取、Connection unary RPC（HMAC Cookie）、K1-K3 Accessibility 切换
- ✅ 自动激活第一个运行 agent；RwLock poison 修复；DSH surface 优先解析为 Web
- ✅ 修复 DSH slot 残留导致 vendor_hid 显示旧 session labels

### 2026-09-16
- ✅ 基础进程检测、session 文件轮询、列表同步
