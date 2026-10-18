# K4-K10 功能键统一方案

> 最后更新：2026-09-01

---

## 背景问题

当前 5 个 active agent（A1-A5）+ Herdr，每个 agent 有独立的 K4-K10 映射（定义在各自的 `plugin.json` 中）。
导致用户在切换 agent 时，同一个物理键的功能和标签都不同，增加心智负担。

**目标**：统一 K4-K10 标签，减少用户心智负担。默认配置作为最佳实践，软件架构上不阻止用户自行覆盖。

---

## 现状：各 agent K4-K10 映射（来自 plugin.json）

| 键 | ChatGPT (Desktop) | Codex (Terminal) | Qwen (Terminal) | Claude (Terminal) | Herdr (HerdrPane) | DSH (Web) |
|----|-------------------|------------------|-----------------|-------------------|-------------------|-----------|
| K4 | FORK `chatgpt_fork` | FORK `/fork` | COMMAND `/` | COMMAND `/` | SPLIT→ `prefix+v` | COMMAND `/` |
| K5 | SEND `Enter` | SEND `Enter` | STOP `Esc` | STOP `Ctrl+C` | SPLIT↓ `prefix+-` | STOP `/stop` |
| K6 | PREV `⌘⇧[` | PREV `↑` | PREV `↑` | PREV `↑` | PREV TAB `prefix+p` | COMPACT `/compact` |
| K7 | NEXT `⌘⇧]` | NEXT `↓` | NEXT `↓` | NEXT `↓` | NEXT TAB `prefix+n` | PAUSE `/goal pause` |
| K8 | SIDEBAR `⌘B` | COMMAND `Ctrl+K` | TRANSCRIPT `Ctrl+O` | TRANSCRIPT `Ctrl+O` | SIDEBAR `prefix+b` | RESUME `/goal resume` |
| K9 | FIND `⌘F` | TRANSCRIPT `Ctrl+O` | MODE `Ctrl+R` | MODE `Shift+Tab` | ZOOM `prefix+z` | PLAN `/plan` |
| K10 | VOICE `voice_hold` | HELP `?` | KEYS `?` | HELP `/help` | HELP `prefix+?` | KEYS `?` |

**观察**：
- K6/K7 (PREV/NEXT) 大部分 agent 语义一致，但 DSH 用了 COMPACT/PAUSE
- K5 在 ChatGPT/Codex 是 SEND，其他是 STOP — 语义不同
- K8-K10 各 agent 差异最大

---

## 各 Agent 实际功能调研

> 基于 plugin.json、README.md、agent_actions.rs 调研各 agent **实际支持的功能**，而非当前映射。

| 功能 | ChatGPT | Codex | Qwen | Claude | Herdr | DSH |
|------|:-------:|:-----:|:----:|:------:|:-----:|:---:|
| **Sidebar** | ✅ ⌘B | ❌ | ❌ | ❌ | ✅ prefix+b | ❌ |
| **Find/Search** | ✅ ⌘F (聊天内) | ❌ | ⚠️ Ctrl+R (历史, 标签歧义) | ❌ | ❌ | ❌ |
| **Fork/Branch** | ✅ forkThread | ✅ /fork | ✅ /branch (未映射) | ❌ | ❌ | ❌ |
| **Send/Submit** | ✅ Enter | ✅ Enter | ✅ Enter | ✅ Enter | N/A | ✅ Enter |
| **Stop/Cancel** | ✅ ⌘. (未映射) | ✅ Ctrl+C (未映射) | ✅ Esc | ✅ Ctrl+C | N/A | ✅ /stop |
| **Prev/Next** | ✅ ⌘⇧[/] (会话) | ✅ ↑/↓ (历史) | ✅ ↑/↓ (历史) | ✅ ↑/↓ (历史) | ✅ prefix+p/n (标签) | ✅ ↑/↓ (历史, 未映射) |
| **Transcript/View** | ❌ | ✅ Ctrl+O | ✅ Ctrl+O | ✅ Ctrl+O | ❌ | ❌ |
| **Command Palette** | ✅ ⌘K (未映射) | ✅ Ctrl+K | ✅ / | ✅ / | ❌ | ✅ / |
| **Mode Switch** | ❌ | ❌ | ✅ Ctrl+R (标签歧义) | ✅ Shift+Tab | ❌ | ⚠️ 通过 /plan, /goal |
| **Compact** | ❌ | ❌ | ❌ | ✅ /compact (未映射) | ❌ | ✅ /compact |
| **Help/Keys** | ❌ | ✅ ? | ✅ ? | ✅ /help | ✅ prefix+? | ✅ ? |
| **Voice** | ✅ Ctrl+Shift+D | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Zoom** | ❌ | ❌ | ❌ | ❌ | ✅ prefix+z | ❌ |
| **Split** | ❌ | ❌ | ❌ | ❌ | ✅ prefix+v/- | ❌ |
| **Goal/Plan** | ❌ | ❌ | ❌ | ❌ | ❌ | ✅ /goal, /plan |
| **New Tab/Session** | ✅ ⌘N (未映射) | ❌ | ✅ /clear (未映射) | ✅ /clear (未映射) | ✅ prefix+c (未映射) | ✅ /goal (未映射) |

### 关键发现

1. **Qwen Ctrl+R 是历史搜索**，不是聊天内容搜索 — 与 ChatGPT ⌘F 语义不同
2. **Prev/Next 语义差异大**：ChatGPT 是会话间导航，Terminal agents 是输入历史，Herdr 是标签页
3. **Codex 没有配置 Stop** — plugin.json 中未映射（虽然 Ctrl+C 可用）
4. **Fork 只有 3 个 agent 支持**：ChatGPT、Codex、Qwen
5. **Sidebar 只有 2 个 agent 支持**：ChatGPT、Herdr
6. **Transcript/View 有 3 个 terminal agent 支持**：Codex、Qwen、Claude（都是 Ctrl+O）

### README vs plugin.json 差异

> **重要**：README 记录了 agent 具备但未在当前 plugin.json 中映射的功能。plugin.json 是实际部署的数据源。

| Agent | README 记录但 plugin.json 未映射的功能 |
|-------|---------------------------------------|
| ChatGPT | K1=NEW(⌘N), K2=QUICK(⌘⌥N), K3=TEMP(⌘⇧N), K4=COMMAND(⌘K), K5=STOP(⌘.) |
| Qwen | K1=NEW(/clear), K2=QUICK(/btw), K3=BRANCH(/branch) |
| Claude | K1=CLEAR(/clear), K2=COMPACT(/compact), K3=RESUME(/resume) |
| Codex | Stop (Ctrl+C) |
| DSH | K6=PREV(↑), K7=NEXT(↓) |
| Herdr | K1=NEW TAB(prefix+c), K2=WORKSPACE(prefix+w), K3=GOTO(prefix+g) |

**结论**：这些功能是 agent 实际支持的，只是当前 plugin.json 没有暴露。统一 keymap 时可以考虑是否需要补充映射。

---

## 数据源架构

```
plugin.json (主) ──→ snapshot.function_keys ──→ LCD label (deck-view v2)
                     ──→ execute_function_key() ──→ CGEvent / text_input / herdr_prefix

agent_actions.rs (备) ──→ 仅在 plugin 未注册时生效（fallback）
```

- **plugin.json** 是实际生效的数据源
- **agent_actions.rs** 的 adapter 表是 fallback，主要用于测试
- `FunctionKeyState.label` 是 LCD 上显示的标签（7 字节，UTF-8）
- `FunctionKeyState.supported` 决定键是否可用

---

## 方案讨论

### 方案 A：严格 ChatGPT 映射

| 键 | 统一标签 | ChatGPT | Codex | Qwen | Claude | Herdr | DSH |
|----|---------|---------|-------|------|--------|-------|-----|
| K4 | FORK | ✅ | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 |
| K5 | SEND | ✅ | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 |
| K6 | PREV | ✅ | ✅ ↑ | ✅ ↑ | ✅ ↑ | ✅ prefix+p | ❌ 空 |
| K7 | NEXT | ✅ | ✅ ↓ | ✅ ↓ | ✅ ↓ | ✅ prefix+n | ❌ 空 |
| K8 | SIDEBAR | ✅ | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 |
| K9 | FIND | ✅ | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 |
| K10 | VOICE | ✅ | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 | ❌ 空 |

- 优点：完全统一
- 缺点：7 个键中大部分 agent 大部分键是空的，利用率极低

### 方案 B：ChatGPT 标签为默认 + 语义等价覆盖

基于实际功能调研，重新评估各键的统一可能性：

| 键 | 统一标签 | ChatGPT | Codex | Qwen | Claude | Herdr | DSH | 备注 |
|----|---------|---------|-------|------|--------|-------|-----|------|
| K4 | FORK | ✅ fork | ✅ /fork | ✅ /branch | ❌ | ❌ | ❌ | 3/6 支持 |
| K5 | SEND/STOP | ✅ Enter | ✅ Enter | ✅ Esc | ✅ Ctrl+C | ❌ | ✅ /stop | 语义不同但实用 |
| K6 | PREV | ✅ ⌘⇧[ | ✅ ↑ | ✅ ↑ | ✅ ↑ | ✅ prefix+p | ✅ ↑ | 全部支持，语义不同 |
| K7 | NEXT | ✅ ⌘⇧] | ✅ ↓ | ✅ ↓ | ✅ ↓ | ✅ prefix+n | ✅ ↓ | 全部支持，语义不同 |
| K8 | SIDEBAR/VIEW | ✅ ⌘B | ✅ Ctrl+O | ✅ Ctrl+O | ✅ Ctrl+O | ✅ prefix+b | ❌ | 5/6 有等价功能 |
| K9 | FIND/MODE | ✅ ⌘F | ❌ | ⚠️ Ctrl+R | ❌ | ❌ | ❌ | 仅 2/6 有 |
| K10 | VOICE/HELP | ✅ voice | ✅ ? | ✅ ? | ✅ /help | ✅ prefix+? | ✅ ? | 5/6 有 HELP |

**修正后的分析**：
- K6/K7：全部 agent 都有 PREV/NEXT 等价功能（虽然语义不同：会话/历史/标签）
- K8：5/6 agent 有 SIDEBAR 或 VIEW/TRANSCRIPT — 可以统一标签为 `VIEW`
- K10：5/6 agent 有 HELP — 可以统一
- K4：只有 3/6 有 FORK — ChatGPT/Codex/Qwen 统一，其他留空
- K5：全部有 SEND 或 STOP — 保留各自语义
- K9：只有 ChatGPT 有真正的 FIND — 其他留空或保留各自功能

**建议最终映射**：

| 键 | 统一标签 | ChatGPT | Codex | Qwen | Claude | Herdr | DSH |
|----|---------|---------|-------|------|--------|-------|-----|
| K4 | FORK | `chatgpt_fork` | `/fork` | `/branch` | — | — | — |
| K5 | SEND/STOP | `Enter` | `Enter` | `Esc` | `Ctrl+C` | — | `/stop` |
| K6 | PREV | `⌘⇧[` | `↑` | `↑` | `↑` | `prefix+p` | `↑` |
| K7 | NEXT | `⌘⇧]` | `↓` | `↓` | `↓` | `prefix+n` | `↓` |
| K8 | VIEW | `⌘B` | `Ctrl+O` | `Ctrl+O` | `Ctrl+O` | `prefix+b` | — |
| K9 | FIND | `⌘F` | — | `Ctrl+R` | — | — | — |
| K10 | HELP | `voice_hold` | `?` | `?` | `/help` | `prefix+?` | `?` |

**变化点**：
- K8 标签从 SIDEBAR 改为 VIEW — 涵盖 Sidebar 和 Transcript 两种语义
- K10 标签保持 HELP — ChatGPT 保留 VOICE（特例）
- K4 FORK 只有 3 个 agent 支持，其他留空
- K9 FIND 只有 ChatGPT 和 Qwen 支持，其他留空

### 方案 C：（未来）用户自定义覆盖

架构已支持：`profile.rs` 中 `agent_keys: BTreeMap<agent_id, BTreeMap<surface, BTreeMap<key, ConfiguredAction>>>` 允许用户为每个 agent/surface/key 组合自定义绑定。

---

## 决定

**采用方案 B（部分统一）**：ChatGPT 标签为默认基线，允许语义等价覆盖，DSH/Herdr 保留特殊功能。

### 最终决策

1. **K8 标签**：`VIEW` — 更通用，涵盖 Sidebar 和 Transcript 两种语义
2. **K10 标签**：`VOICE` — 严格跟随 ChatGPT，其他 agent 留空（不用 HELP）
3. **DSH 功能**：保留 COMPACT/PAUSE/RESUME/PLAN 在 K6-K9 — DSH 特殊处理，不统一标签
4. **Herdr Split**：保留 SPLIT→/SPLIT↓ 在 K4-K5 — Herdr 特殊处理，不统一标签

### 统一后的默认映射（最终版）

| 键 | 标签 | ChatGPT | Codex | Qwen | Claude | Herdr | DSH |
|----|------|---------|-------|------|--------|-------|-----|
| K4 | OK | `chatgpt_approve` | `y` | Enter | `y` | Enter 选中 | `y` |
| K5 | NO | `chatgpt_reject` | `n` | Esc | `n` | Esc 退出 | `n` |
| K6 | VIEW | `⌘B` | `Ctrl+O` | `Ctrl+O` | `Ctrl+O` | `prefix+b` | COMPACT `/compact` |
| K7 | STOP | `⌘.` | `Ctrl+C` | `Esc` | `Ctrl+C` | VSP `prefix+v` | PAUSE `/goal pause` |
| K8 | SEND | `Enter` | `Enter` | `Enter` | `Enter` | HSP `prefix+-` | RESUME `/goal resume` |
| K9 | FORK | `chatgpt_fork` | `/fork` | `/branch` | — | — | PLAN `/plan` |
| K10 | VOICE | `voice_hold` | — | — | — | SESSION `prefix+g` | — |

**Herdr 方向键由 knob decoder 处理**：顺时针旋转 = ↑，逆时针旋转 = ↓，不占用 K8/K9 按键。

**统一程度**：
- K4/K5：全部 agent 统一为 OK/NO，用于批准/拒绝 tool use
- K6：5/6 agent 统一为 VIEW（ChatGPT/Codex/Qwen/Claude/Herdr），DSH 保留 COMPACT
- K7：4/6 agent 统一为 STOP，DSH 保留 PAUSE，Herdr 为 VSP (vertical split)
- K8：4/6 agent 统一为 SEND，DSH 保留 RESUME，Herdr 为 HSP (horizontal split)
- K9：3/6 agent 有 FORK（ChatGPT/Codex/Qwen），DSH 保留 PLAN
- K10：ChatGPT 有 VOICE，Herdr 有 SESSION (`prefix+g`)

**设计理念**：
- K1-K3 = Session 选择（PANEL 1/2/3）
- K4-K5 = 批准/拒绝（OK/NO）— 高频操作放在最顺手的位置
- K6-K10 = 常用功能（VIEW/STOP/SEND/FORK/VOICE）

**注意**：
- ChatGPT 的 OK/NO 需要通过 native HID (ACT07/ACT08) 发送，companion 侧暂未实现
- Terminal agents 的 OK/NO 通过发送 `y`+Enter / `n`+Enter 实现

### ChatGPT BLE 原生 HID 映射 (2026-09-14 更新)

ChatGPT agent 在 BLE 模式下使用 Codex Micro 原生 ACT 代码，直接由 ESP32 固件发送给 ChatGPT Desktop，不经过 companion 中转。

| Companion 逻辑键 | 物理 bit | 标签 | ACT 代码 | 说明 |
|-----------------|---------|------|---------|------|
| K4 | bit 8 | OK | ACT07 | Approve |
| K5 | bit 9 | NO | ACT08 | Decline |
| K6 | bit 10 | FORK | ACT09 | Fork/分支 |
| K7 | bit 11 | FAST | ACT06 | 快速操作 |
| K8 | bit 12 | SIDEBAR | — | 无原生 ACT 代码，companion 中转 Cmd+B |
| K9 | bit 13 | SEND | ACT12 | 发送消息 |
| K10 | bit 14 | MIC | ACT10 | 麦克风 |

**Session 选择 (K1-K3, bits 5-7):不走原生 AG 码**（2026-09-22 改）
- 固件不再发 `AG00`-`AG02`；K1-K3 仍发标准 HID chord（Ctrl+F13/F14/F15），由 companion 处理
- companion 用 LCD 列表第 N 条 panel 的 thread id 打开 `codex://threads/<uuid>`，桌面被切到该线程
- 原因：`AG00`-`AG05` 对应桌面命令 `recentThread1`-`recentThread6`，索引的是**桌面侧栏自身的序列**
  （Projects 分组在前、Recents 在后），和 companion 按 recency 排的列表不是一回事，
  结果就是"按 K2 跳到列表第 3 条"这类错位。改由 companion 拥有映射后，列表和跳转必然一致。

**Codex Micro ACT 代码限制**：
- 可用的 ACT 代码：ACT06(FAST), ACT07(APPROVE), ACT08(DECLINE), ACT09(FORK), ACT10(MIC), ACT12(SEND)
- K8 (Toggle Sidebar) 没有对应的原生 ACT 代码，无法使用 Codex Micro 原生键实现
- K8 仍需通过 companion 中转（USB HID chord → companion → Cmd+B）

**USB 模式行为**（2026-09-28 更新）：
- 在 USB 模式下，companion 接收 HID chord 并尝试执行对应的 action
- ChatGPT 的 `v.oai.hid` 原生键（ACT06/ACT07/ACT08/ACT10、旋钮 `ENC_CW`/`ENC_CC`）与 KM 流量共用
  同一条 live vendor 传输，**USB 优先、BLE 兜底**
- Desktop 侧不区分总线，它按 HID 身份挑设备：`vendorId 0x303A` + `usagePage 0xFF00` + **PID 查注册表**，
  `0x8360` 才是 Codex Micro（`0x1001` 会被认成 Nomad E，原生能力不挂载）。固件因此把 USB 描述符的
  PID 也声明为 `0x8360`，与 BLE PnP 描述符一致
- companion 的 `open_deck()` 只按 VID + usage page + usage 过滤，不看 PID，所以这个身份变化不影响 companion
- 真机已确认（2026-09-28）：纯 USB 工作模式下 ChatGPT 的功能键（K4-K10）与旋钮调 reasoning 都生效
- K8 (SIDEBAR) 在两种模式下都正常工作（通过 Cmd+B shortcut）

**设计说明 (2026-09-14)**：
- K8/K9 交换：语音录完按 K9 发送更方便，SIDEBAR 移到 K8

### 改动原则

1. **标签尽量统一**：K4/K5 显示 OK/NO；K6 显示 VIEW；K7 显示 STOP；K8 显示 SEND；K9 显示 FORK
2. **高频操作放在顺手位置**：OK/NO 在 K4/K5，紧挨 Session 选择键
3. **无等价操作则留空**：FORK 只有 ChatGPT/Codex/Qwen 有；VOICE 只有 ChatGPT 有
4. **特殊 agent 保留功能**：DSH 保留 COMPACT/PAUSE/RESUME/PLAN；Herdr 保留 SPLIT
5. **不封死用户自定义**：profile override 机制保持不变

### 与原始 plugin.json 的差异

原始布局（修改前）：
```
K4=FORK/COMMAND  K5=SEND/STOP  K6=PREV/YES  K7=NEXT/NO  K8=VIEW  K9=FIND/MODE  K10=VOICE/HELP
```

新布局（修改后）：
```
K4=OK  K5=NO  K6=VIEW  K7=STOP  K8=SEND  K9=FORK  K10=VOICE
```

**变化总结**：
- K4/K5：从 FORK/SEND 改为 OK/NO — 批准/拒绝放在最顺手的位置
- K6：从 PREV 改为 VIEW — 展开/折叠功能
- K7：从 NEXT 改为 STOP — 停止当前操作
- K8：从 VIEW 改为 SEND — 发送/提交
- K9：从 FIND/MODE 改为 FORK — 创建分支
- K10：保持 VOICE（仅 ChatGPT）

**Herdr 特殊处理**：
- K4/K5 保留 SPLIT→/SPLIT↓
- K6 改为 VIEW (prefix+b)
- K7-K10 留空

**DSH 特殊处理**：
- K4/K5 改为 OK/NO
- K6-K9 保留 COMPACT/PAUSE/RESUME/PLAN
- K10 留空

### 待确认 ✅ 已决定

- [x] K4/K5 = OK/NO（批准/拒绝）
- [x] K6 = VIEW
- [x] K7 = STOP
- [x] K8 = SEND
- [x] K9 = FORK
- [x] K10 = VOICE（仅 ChatGPT）
- [x] DSH 保留 COMPACT/PAUSE/RESUME/PLAN 在 K6-K9
- [x] Herdr 保留 SPLIT→/SPLIT↓ 在 K4-K5

---

## 实施步骤

1. 修改各 agent 的 `plugin.json` — 按最终映射表调整 ✅
2. 同步更新 `agent_actions.rs` adapter 表 ✅
3. 更新测试用例 ✅
4. 更新 `key_map.md` 文档 ✅
5. 更新 `key_map.md` 记录此变更
