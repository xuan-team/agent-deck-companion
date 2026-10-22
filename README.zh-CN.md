# Agent Deck Companion

[English](README.md) · [简体中文](README.zh-CN.md)

**本地优先的桌面面板，把电脑上的每个 AI Coding Agent 都收拢在一处 —— 无需任何硬件。**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey.svg)](https://github.com/xuan-team/agent-deck-companion)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB.svg)](https://tauri.app)
[![Back us on Kickstarter](https://img.shields.io/badge/Kickstarter-back%20us-05ce78?logo=kickstarter&logoColor=white)](https://www.kickstarter.com/projects/agentdeck/agentdeck-the-physical-control-deck-for-ai-coding-agents)

同时运行多个 Agent 时，很难判断哪个会话正在工作、哪个在等你输入、哪个刚产出新结果。
Companion 用一块小面板把这些状态集中呈现，点一下即可回到那个会话本身。它是 Agent 的控制面，
不是 IDE。

> **状态**：目前仅支持 macOS；Windows 与 Linux 计划中，尚未支持或验证。
> 采用 Tauri 2、Rust、SvelteKit 和 TypeScript。

---

## 演示视频

[![AgentDeck 演示：从手动切换 AI Agent 到一键控制](https://img.youtube.com/vi/Uf_wwHSCPIs/maxresdefault.jpg)](https://youtu.be/Uf_wwHSCPIs)

▶️ **在 YouTube 观看** —— 先看 Agent Deck Companion 单独使用，再看可选的 AgentDeck 硬件如何叠加其上。

---

## 为什么做它

现代开发分散在许多 Agent、终端、IDE 窗口与浏览器标签之间，每一次 `Alt-Tab` 都是在猜「到底哪个
在等我」。Companion 把这层状态收敛到一处：

- **看得见**：每个会话此刻在做什么（工作中、需要输入、已完成、出错）。
- **抓重点**：只把真正需要你的会话推到眼前，同时不抢走你当前窗口的焦点。
- **回得去**：一步回到那个会话本身 —— 正确的终端，或 ChatGPT 里正确的那条 thread。

## 功能

- **会话面板**：常驻面板列出所有被监控 Agent 的会话，点击一行即可打开该会话。独立浮窗支持按项目
  分组，以及 **需处理 / Codex / 其他 Agent** 三个可重排视图。
- **关注队列**：需要输入、出错或产出新结果的会话集中在一条队列里，支持已读 / 未读。
- **通知**：面板可见时用浮窗提醒卡，面板隐藏时可选择走 macOS 桌面通知。通知不抢焦点，点击即回到
  会话或关注队列。
- **跳转到会话**：依据会话可用的定位信息（终端、pane，或 Codex / ChatGPT 的
  `codex://threads/<id>` 深链）打开对应会话。无法定位时明确提示，而不是猜。
- **轻量浮窗**：可收起、隐藏、调整标签顺序，并从菜单栏重新显示；配色跟随主窗口选择的统一主题。
- **逐 Agent 开关监控**：在 **设置 → Agent 监控** 里只打开自己使用的 Agent。关闭后不再检测该
  Agent，其会话会从列表移除（A1–A5 槽位绑定保留，重新打开即恢复）。

## 没有硬件也能用

**使用 Companion 不需要任何 AgentDeck 硬件。** 硬件监听默认关闭，上面这一整套会话管理能力都可以
单独使用。

AgentDeck 是基于 ESP32 的可选配套键盘，在同一套桌面能力之上增加实体按键、屏幕、旋钮与键盘提示
音。见 [可选：AgentDeck 硬件](#可选agentdeck-硬件)。

## 支持的 Agent

不同来源的状态粒度与定位能力不同。Companion 通过逐 Agent 的适配层检测进程与状态；
进程存在不代表能够精确识别每个会话。

| Agent | 检测方式 | 生命周期状态 |
| --- | --- | --- |
| ChatGPT Desktop / Codex | 原生 | 进程 + 会话列表，深链跳转 |
| Claude Code | 进程 + hooks | 需安装 hooks |
| Qwen Code | 进程 + hooks | 需安装 hooks |
| Codex CLI | 进程 + hooks | 需安装 hooks |
| Herdr（CLI Agent） | 原生集成 | 按 pane |
| DeepSeek Harness (DSH) | 原生集成 + `dsh-deck-bridge` 插件 | 按会话 |
| OpenCode / Qoder | 插件 | 按会话 |

CLI Agent 通过显式安装的 hooks 上报生命周期状态；DeepSeek Harness 使用随附的
`dsh-deck-bridge` 插件。各 Agent 的细节见 [`doc/`](doc/)。

## 开始使用

### 前置条件

- macOS（已验证 Apple Silicon；Intel 需另行构建验证）；Windows 与 Linux 计划中。
- Node.js 20+ 与 npm
- Rust stable
- 对应平台的 [Tauri 2 系统构建依赖](https://tauri.app/start/prerequisites/)

### 从源码运行

```bash
npm install
npm run tauri:dev
```

1. 启动 Companion，再启动你使用的 Agent。
2. 默认「会话」页显示已发现的会话。点击「打开会话浮窗」，或用菜单栏显示 / 隐藏浮窗。
3. 需要 CLI 生命周期状态时，进入 **Hooks**，仅为自己使用的 Agent 点击「安装 Hooks」。Codex CLI
   首次加载新 Hooks 时，需在 CLI 的 `/hooks` 中审核并信任。
4. 在 **通知** 页按需启用系统通知；未授权也可使用浮窗和关注列表。系统通知需使用打包后的 macOS
   应用，开发窗口可能不可用。
5. 在 **设置 → Agent 监控** 里只打开自己使用的 Agent。关闭后不再检测该 Agent 的进程与状态，它的
   会话会从列表移除。
6. 点击会话返回工作窗口。若提示无法定位，请检查 Agent 是否仍在运行、宿主终端与相关权限。

普通用户不需要启用 ESP32 硬件支持，也不需要配置 A1–A5 或 K1–K10。

### 构建 macOS 应用

```bash
npm run tauri:build -- --bundles app
```

构建产物位于 `src-tauri/target/release/bundle/macos/`。当前采用 ad-hoc 签名，重新构建后可能需要
重新授权；构建、签名与分发说明见 [macOS 发布指南](doc/macos_release.md)。

推送 `v*` tag 时，GitHub Actions 会构建 Apple Silicon + Intel 的 universal DMG 并发布到
[Releases](https://github.com/xuan-team/agent-deck-companion/releases)。该构建只有 ad-hoc 签名、
未经公证，安装后需要执行一次：

```bash
xattr -dr com.apple.quarantine "/Applications/Agent Deck Companion.app"
```

## 可选：AgentDeck 硬件

**AgentDeck 是基于 ESP32 的配套硬件键盘**。它为同一套桌面能力增加实体按键、屏幕、旋钮与键盘提示
音；Companion 的独立软件功能无需硬件解锁。

仅硬件用户需要打开 **AgentDeck 硬件（可选）** 页：

1. 在 Companion 菜单栏启用「ESP32 硬件支持」，再连接设备。Keyboard HID 支持 USB / BLE；屏幕状态
   同步的 Vendor HID 当前限定为 BLE。
2. 按需授予输入监控权限；发送模拟快捷键时还可能需要辅助功能权限。授权输入监控后重启应用。
3. 点击「配置键位」，将 **A1–A5** 绑定为 Agent selector；可把一个槽位绑定到 **Companion**，快速
   进入会话浮窗导航。
4. 在 Companion 导航中，用 K1–K3 / K6–K8 打开当前页的会话，K4/K5 翻页，K9 退出导航，K10 显示 /
   隐藏浮窗；旋钮用于标签或翻页导航。实体导航能力取决于配套固件版本，界面会提示尚待固件支持的部分。
5. 需要时配置 **K1–K10** 的 Agent 动作、功能键提示和键盘通知音。

新配置默认 A1=Qwen、A2=Herdr、A3=ChatGPT、A4=Claude、A5=Companion。已有用户的配置保留。
Codex CLI 与 ChatGPT Desktop 当前共用一个可选 Agent 槽位。

关闭硬件支持后，重启应用可完整停止已启动的监听。没有设备时，也可以在硬件页模拟按键进行开发测试。

键位与协议细节见 [功能键映射](key_map.md)、[键盘通知音协议](notification_audio_protocol.md)和
[硬件排障](doc/qa.md)。

## 权限与本地数据

- **基础状态查看**不要求输入监控或辅助功能权限。部分窗口定位、快捷键发送可能需要辅助功能 /
  自动化权限；输入监控用于全局硬件按键。
- **系统通知**需要用户主动启用，并受 macOS 通知设置与勿扰模式控制。
- **Hooks** 显式安装 / 卸载，合并现有配置；首次修改已有配置前创建 `*.agent-deck-backup`。卸载仅
  清理 Companion 标记的 handler。
- Hook bridge 仅提取 session / task 标识、工作目录、事件名与时间戳等元数据，不保存 payload 中的
  prompt、tool input、tool response 或 transcript。
- 本机 API 仅监听 `127.0.0.1:34115`；Hooks 使用本地 token 鉴权。不要把本机调试 API 转发到公网。
- 配置保存在本机。macOS 配置目录为 `~/Library/Application Support/Agent Deck Companion/`；
  Hooks bridge 位于 `~/.agent-deck-companion/`。

## 架构与开发

- `src/`：SvelteKit 界面与 TypeScript API client。
- `src-tauri/`：Rust 状态、Agent adapter、窗口、进程和设备集成。
- [`ARCHITECTURE.md`](ARCHITECTURE.md)：整体结构。
- [`AGENTS.md`](AGENTS.md)：架构边界与贡献者的验证要求。

按改动区域运行相应检查：

```bash
npm run check
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
```

涉及事件路径时，还应在运行中的 Tauri 应用验证 `/api/hardware/key` 与 `/api/mock/status`。单独运行
`npm run dev` 仍需已有 Tauri 后端提供本机 API，不能替代桌面应用。

完整流程见 [CONTRIBUTING.md](CONTRIBUTING.md)，安全问题请按 [SECURITY.md](SECURITY.md) 报告。
参与本项目即表示同意 [行为准则](CODE_OF_CONDUCT.md)。

## 支持这个项目

Agent Deck Companion 免费开源（MIT）。**AgentDeck 硬件**正在 Kickstarter 众筹 —— 支持众筹，或给本
仓库点一个 star，都能帮我们把它继续做下去。

- 💛 [在 Kickstarter 支持 AgentDeck](https://www.kickstarter.com/projects/agentdeck/agentdeck-the-physical-control-deck-for-ai-coding-agents)
- ▶️ [观看演示视频](https://youtu.be/Uf_wwHSCPIs)

## 许可证

[MIT](LICENSE) © 2026 xuan team
