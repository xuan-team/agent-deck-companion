# Q&A / Troubleshooting

## Decoder (Knob) 不调节 Reasoning

### 症状

- 硬件上转 knob，硬件日志有 `CheckRotation currentDirection=X`，但没有 `Codex encoder ENC_CW step sent=X` 日志
- 或者 encoder 事件到达了 ChatGPT Desktop，但在 UI 组件间跳转而不是调节 reasoning

### 排查步骤

#### 1. 确认 Companion App 推送的 Agent 名称正确

**根因**：Companion 的 `agent_display_name()` 曾将 `"chatgpt"` 转为 `"Chatgpt"`（只大写首字母），但固件用 `strcmp(name, "ChatGPT")` 做精确匹配，导致 encoder callback 的 `isChatGPT` 检查失败，encoder 事件被静默丢弃。

**验证**：检查 `src-tauri/src/vendor_hid.rs` 中 `agent_display_name` 函数，确保 `"chatgpt"` 返回 `"ChatGPT"`。

**修复**：已修复。重新 build companion app 并重启，让 deck-view 把正确的 agent name 推送到硬件。

#### 2. 确认 ChatGPT Desktop 的 Knob 模式

**根因**：ChatGPT Desktop 有自己的 encoder 模式设置。如果模式不是 "Reasoning only"，encoder 事件会被当作 UI 导航（在组件间跳转）或其他行为。

**ChatGPT Desktop Knob 模式**：

| 模式 | 行为 |
|------|------|
| Reasoning only | 调节 reasoning effort（low/medium/high/extra high） |
| Composer navigation | 在 composer 控件间跳转 |
| Conversation scrolling | 滚动对话内容 |
| Custom assignments | 自定义每个手势 |

**修复**：打开 ChatGPT Desktop → Settings → Codex Micro → Knob → 选择 **"Reasoning only"**。

#### 3. 确认 `~/.codex/config.toml` 配置

Companion app 读取此配置来决定自己的 sync 策略：

```toml
[desktop.codex-micro-layout]
encoderMode = "reasoning"
```

如果缺失，companion app 会报 `wrong_mode` 状态。

> 注意：ChatGPT Desktop 自身的 knob 模式以 Settings UI 为准，config.toml 主要影响 companion app 的行为。

### 数据流

```
硬件 encoder 旋转
  → 固件 CheckRotation()
  → 固件 encoder callback
    → 条件: currentWorkMode == BLE_KEYBOARD_MODE && active agent name == "ChatGPT"
    → 通过: 发送 ENC_CW/ENC_CC via BLE Vendor HID (Report 0x06) → ChatGPT Desktop
    → 失败: 静默丢弃（无日志）
  → ChatGPT Desktop 根据 knob 模式处理事件
  → Companion app 从 SQLite 读回 reasoning 状态确认
```

## A1/A4/A5 Agent 映射不一致

### 问题描述

硬件 LCD 曾显示 `A1 -> DSH`、`A4 -> OpenCode`、`A5 -> Qwen`，而 Companion UI 期望 `A1 -> Qwen`、`A4 -> Claude`、`A5 -> DSH`。

硬件日志中的 `session[0] label="Qwen ..."` 只是当前 Agent 的第一个 session 标签，不能用于判断 A1~A5 的 Agent 槽位映射。

### Root cause

根因是 Companion 的本地持久化 profile 与 UI 期望配置不一致，而不是 ESP32 的 A 键编号转换错误。

实际配置文件为：

```text
~/Library/Application Support/Agent Deck Companion/profiles.json
```

原配置是 `A1=dsh`、`A4=opencode`、`A5=qwen`。Companion 会将该配置通过 DeckView 下发给 ESP32，因此硬件按照该配置显示。

ESP32 固件只保存当前 selected slot，不保存 Agent 槽位映射；启动时的 `ui_model_reset()` 名称只是 DeckView 到达前的 fallback。

### ESP32 固件判断

之前的固件槽位处理逻辑是正常的：A1~A5 映射为 0~4，日志中的 `A3 pressed`、`slot=2` 与此一致；`EvSelectedChanged slot=2` 已成功发送，后续 DeckView revision 也已成功应用。没有证据表明固件发生了 A1/A4/A5 索引错位。

因此，之前固件的通信和 slot 处理是 OK 的；问题在 Companion profile 配置。固件后来补充了正确的启动 fallback 名称和 A1~A5 名称日志，但这不是本次映射错误的根因修复。

### 修复

已将本机 `profiles.json` 修正为：

```text
A1 -> Qwen
A2 -> Herdr
A3 -> ChatGPT
A4 -> Claude
A5 -> DSH
```

完全退出并重新启动 Companion 后，它会重新向 ESP32 推送 DeckView。此次无需擦除 NVS/SPIFFS，也无需再次刷写固件。

**关键点**：物理 encoder 事件直接走 BLE HID → ChatGPT Desktop，不经过 companion app。Companion app 的 `vendor_hid.rs` 不处理 encoder rotation 事件。

### 硬件日志诊断

| 日志 | 含义 |
|------|------|
| `CheckRotation currentDirection=X` | 硬件检测到旋转（callback 之前） |
| `Codex encoder ENC_CW step sent=1` | encoder 事件成功发送（BLE 模式 + ChatGPT） |
| 只有 CheckRotation，没有 Codex encoder | callback 内条件不满足（agent 名称不匹配或不在 BLE 模式） |

## Companion UI 与硬件 LCD 的 A1–A5 映射短暂不一致

### 症状

用户通过 **Configure Keys** 调整 Agent 槽位后，硬件 LCD 和物理按键按照新 profile 工作，例如：

```text
A1 -> ChatGPT
A3 -> DSH
A5 -> Qwen
```

但 Companion 的硬件模拟器可能显示旧的默认顺序，例如 `A1 -> Qwen`、`A3 -> ChatGPT`。这会造成物理 A1 激活 ChatGPT，而 Companion UI 看起来像激活了另一个槽位的错觉。

### 根因

后端没有丢失 `snapshot.agent_slots`。`AppRuntime::new()` 初始化 snapshot 时就会根据当前 profile 生成该字段，Tauri `get_state` 和 HTTP `/api/state` 也会返回完整槽位配置。

问题来自前端首次加载的异步时序：

1. 页面在 `getState()` 完成前将 `viewReady` 设为 `true`。
2. 此时 `snapshot` 仍为 `null`。
3. `agentSlots()` 使用硬编码的默认映射作为 fallback。
4. 页面因此在启动空窗期显示与持久化 profile 不一致的 A1–A5。

如果首次状态请求失败，这套错误的 fallback 还会持续显示，使用户误以为 Companion、LCD 和物理按键使用了三套映射。

### 修复

- 首次 `getState()` 完成后才展示主界面。
- 删除 `agentSlots()` 中硬编码的 Agent 默认映射。
- snapshot 尚未就绪时显示加载状态。
- 状态请求失败或确实缺少槽位数据时显示“Agent 槽位配置不可用”，不再伪造 A1–A5 配置。
- 成功取得 snapshot 后，Companion UI、LCD payload 和物理按键处理都以同一份 profile `agent_slots` 为准。

### 验证

```bash
npm run check
npm run build
```

两项检查均通过。运行中的 `/api/state` 也确认返回当前 profile 的完整 `agent_slots`。

## 接入动态字模后，切 Agent 时 LCD 刷新慢了 2.76 s

### 症状

中文字模在 LCD 上显示正确，但按 A1–A5 切 agent 时整屏更新明显变慢。
设备日志显示责任不在设备侧：

```text
[1788463] T3 DeckView applied, rev=74        ← 不含字模的一次推送：请求后 32 ms 落地（旧行为）
[1788474] T5 complete
   ⋮ 6.66 s（用户按下切换键）
[1795134] [GLYPH] built 8 glyphs, 1113 B blob, build=0   ← 设备组字体 0 ms
[1795135] [HIDMOCK] Glyph blob frames=21 free=20843      ← 设备已回 ACK，堆健康
[1797893] T3 DeckView applied, rev=75        ← 2.76 s 之后视图才被应用
```

21 帧 × 4 ms = 84 ms 才是这次传输应占的墙钟，超出约 33 倍全在主机侧。

### 根因

- **慢激活挡住了唯一的读线程**（主因，≈2.7 s）。字模的 ACK 只有 HID reader 线程读得到，
  而 reader 在 `handle_report` 里**同步**跑 `activate_agent`（`focus_host_window` + 4 段固定
  sleep + 多次 `osascript`/`open`，1–3 s）。这期间没有任何线程在读设备，ACK 躺在 OS 缓冲里，
  deck worker 卡在 `ack_rx.recv_timeout`，rev 也就一直不 bump。这条耦合是"先发字模才 bump rev"
  带出来的：改动前推视图只需写 6 帧，与"有没有人读设备"无关。
- **锁竞争吃掉节流**（≈0.5–1 s/事务）。reader 在持设备锁的状态下做 `read_timeout(50)`，而字模
  写路径当时是**逐帧取-写-放**，于是每帧都排在一个 50 ms 时间片后面；96 字满额（95 帧）可到 2.4–4.8 s。
- **100 ms 重试风暴回头喂第一条**。设备 `waitingForDeckView_` 下 100 ms 没等到视图就重发
  `REQUEST_DECK_VIEW`，而含字模的推送按设计不可能在 100 ms 内完成（纯节流 84 ms）。companion 对
  每次重发都会再跑一次激活并清掉去重 ⇒ 越慢越重试、越重试越慢。
- **A→B→A 每次重传整份字模**。epoch 描述"当前标签的码点集合"，而设备字体是整份替换语义，
  在两个 agent 间来回切每次都要重传对方那一整套。

### 修复

1. reader 只做解报文 + 投递：先发布选中 Agent 的快照并排队刷新，激活进一条**单消费者 FIFO**（`state.device_event_tx` + 专用 worker），
   屏先更新、窗口后到位。仍走同一个 `process_event`（串口与 HTTP mock
   同源）；必须串行，否则两次快连按的激活会交错跑完、`observed_pane` 复核误判失败。
2. 旧版 Report 6 写路径改粗粒度锁：一次逻辑突发 = 取放锁一次，4 ms 节流在锁内睡完，**放锁之后才等 ACK**。
   reader 的 50 ms 读超时不动（不无故回退 CPU 优化）。
3. `CMD_REQUEST_DECK_VIEW` 是设备重发、不是用户按键 ⇒ 只在设备槽位与 `active_agent` 不一致时
   （`follow_slot`）才投激活事件；固件侧首次预算 100 ms → **600 ms** 并指数翻倍到 5 s 封顶，
   在真正发出 `EV_SELECTED_CHANGED` 时（`flushPendingEvents()`，固件 `HidMockProtocol.cpp:741`）重置。
4. 字形集合改成**累加并集**（`state.glyph_installed`）：`needed ⊆ installed` ⇒ 零帧；要传时送
   `installed ∪ needed`，超 96 字退回只送 `needed`。线上字节格式一字未动。
5. 每次推送打一条分段耗时，把 `ack_wait`（读线程是否被挡）与 `transfer`（锁竞争）分开：
   `[vendor_hid] deck push rev=… total=… rasterize=… transfer=… ack_wait=… deck=…`

### 验证

```bash
cargo test  --manifest-path src-tauri/Cargo.toml      # 116 通过（含新增 plan 子集/并集用例）
cargo check --manifest-path src-tauri/Cargo.toml --all-targets
idf.py -B build-n4r2-nimble-release size              # IRAM 仍 16383/16384
```

上述是初期问题与修法。后续 BLE 快路径及最终真机结果见下一节；先前的
`[T2]→[T3] < 200 ms`（含 21 帧字模）不是已达成指标。

字节级红线不变量：289 B DeckView 布局一字未动；纯 ASCII 输入报文与改动前逐字节相同；
字模路径任何失败仍收敛到 ASCII 口径而非空白标签。协议契约见
`esp32-keyboard/ai_work_doc/glyph_font_protocol.md` §4.3、§5.1、§5.2、§3.4。

### 后续 BLE 优化与最终测量（2026-09）

Companion 的 Vendor HID 设备选择限定为 BLE，不再用 USB Vendor HID 作为默认链路。固件在
Hello/Status 的能力位声明 BLE Feature Report 7；companion 优先以命令 22 一帧发送**原样
289 B** DeckView，收到固件应用后的 ACK 才算完成，失败则本次连接回退到 Report 6 六帧。
同一个 Report 7 的命令 23 每帧带 285 B 字模数据，最终 ACK 后才允许发 Unicode 视图；
blob 超限或快路径失败仍走原有小分片事务。设备只在完整校验后发布新字体，flash 中不放字库。
真机已验证中文、6 帧大报文字模构建成功及单帧 DeckView ACK；其他语种和失败回退仍需专项验证。

切换路径另做了三处去重与调度修复：

1. 收到 `EV_SELECTED_CHANGED` 后先更新内存中的 `active_agent`，立即排队屏幕刷新；窗口激活在
   独立 worker 执行。重复的 `REQUEST_DECK_VIEW` 不再反复激活或清掉已 ACK 视图的去重键。
   去重键忽略仅随时钟增长的使用量 `age_seconds`；30 秒全量刷新仍会更新它。
2. 固件的 `EV_SELECTED_CHANGED` 与 `REQUEST_DECK_VIEW` 共用事件序号。companion 现在对两者共用
   一个水位；原来的 `event gap: 18 -> 20` 是漏计中间请求 19 的误报。
3. `push_deck_view` 入口原本先取一次**无写入优先标记**的设备锁检查连接，可能被持锁做
   `read_timeout(50)` 的 reader 连续插队，且这段等待发生在 `deck push total` 计时之外。
   改用同步维护的 `snapshot.device.connected` 判断；真正写入仍用 `DeviceWriteIntent` 优先取锁。

| 场景 | ESP32 T0→T3 | companion 指标 |
| --- | ---: | --- |
| 最后一处入口取锁修复前，字形已缓存的 A1 | 1.447–1.567 s | DeckView 快路径仅 259–331 ms；其余在推送前 |
| 修复后，A2 与 A1 往返 | **335 ms / 345 ms** | 选槽事件到推送完成 316 ms / 314 ms，DeckView 写入 285 ms / 279 ms |

`deck push queue delay` 记录的是**下一条**排队触发等待 worker 的时间；若其后没有新的
`deck push rev=`，说明那条触发被去重，并未再次写入 BLE。`deck rev=… after selected event
seq=… observed_for=…` 是主机收到选槽事件到该视图推送完成的耗时。对比它与设备 T0→T3，
可区分 BLE 事件送达延迟和主机排队延迟。上述往返日志中没有超时重试；不同标签触发新字模时，
首次切换仍需等待完整字模事务，不能把 335–345 ms 当作冷启动保证。
