# macOS .app 发布、权限与日志指南

更新日期：2026-09-30。适用于 Agent Deck Companion 当前 macOS POC，以及后续向本地开发者、最终用户分发 `.app` 的工作。

## 1. 当前状态与发布边界

| 项目 | 当前状态 | 正式发布要求 |
| --- | --- | --- |
| 应用标识 | `dev.agentdeck.companion` | 保持稳定 |
| 签名 | 完整 `.app` 的 ad-hoc 签名 | 使用固定的 Developer ID Application 证书 |
| 公证 | 尚未配置 | 完成 notarization，并确认 stapling 结果 |
| 权限 | macOS 由用户授权 | 安装引导解释输入监控与辅助功能的用途 |
| 日志 | Rust `eprintln!` 输出到 stderr | 增加持久化、轮转及用户可导出的日志 |
| 已验证架构 | 本机 Apple Silicon | Intel 支持需要另行构建和验证 |

当前包适合本地验证。签名验证成功不等于完成公证，也不等于用户已授予辅助功能权限。以上正式发布要求是后续工作，本文不代表它们已经实现。

## 2. 本次 sidebar 故障记录

### 2.1 现象与定位

手动按 `⌘B` 和 `⌘⇧S` 都能切换桌面应用 sidebar，但硬件 K8 以及 Companion HTTP mock 触发的 K8 没有效果。

实际日志：

```text
[fn_key] execution=Agent(Shortcut { key_code: 11, modifiers: KeyModifiers(1) })
[fn_key] → post plan=Shortcut { key_code: 11, modifiers: KeyModifiers(1) }
[macos_input] CGEvent posting denied; trying System Events for key=11
[macos_input] System Events failed for key=11: ... “osascript”不允许发送按键。 (1002)
```

这证明语义事件已经进入 Companion，动作解析为 `⌘B`，失败发生在 macOS 模拟按键发送阶段。HTTP 返回 200 仅代表接收并处理了请求，不能证明异步动作已成功送达目标应用。

### 2.2 签名与启动方式的证据

本次检查原构建得到：

- 裸 release 二进制为 `adhoc,linker-signed`，签名标识为编译产物名称，`Info.plist` 未绑定。
- Designated Requirement 为 `cdhash` 条件，身份绑定该次构建内容。
- 原 `.app` 严格签名验证失败：`code has no resources but signature indicates they must be present`。
- 从 `npm run tauri:dev` 或直接执行裸 release 二进制时，添加权限后仍被拒绝。

改为完整 bundle 签名、通过 `open` 启动 `.app` 并授权后，用户确认 K8 sidebar 可以切换。没有独立实验分离每个因素，不能把所有失败都单独归因于签名；后续应统一使用完整 `.app` 来验证权限。

### 2.3 已做的修改

`src-tauri/tauri.conf.json` 在 `bundle` 中新增：

```json
"macOS": {
  "signingIdentity": "-"
}
```

`-` 表示 ad-hoc 签名，不需要安装证书。Tauri 打包时对完整 `.app` 签名。重新构建后的验证结果：

```text
Identifier=dev.agentdeck.companion
Info.plist entries=14
Sealed Resources version=2
valid on disk
satisfies its Designated Requirement
```

它修正了完整 bundle 的签名，但 Designated Requirement 仍绑定构建哈希。重新构建后，旧权限可能失效；固定 bundle identifier 本身不能保证权限跨构建保留。

桌面端另补了发送前权限检查：创建 `CGEvent` 成功不能证明 macOS 允许发送。未获得发送权限时会尝试已有 System Events 备用路径，并记录失败原因。

固件另发现并修复一个独立问题：ChatGPT 的 K8 在 Vendor 通道不可用时，可能按下、松开各发一次，造成两次 sidebar toggle。现改为短按松开时只发送一次；长按 ≥600ms 只打开配额页。这不是上述 HTTP 测试失败的直接原因。

## 3. 权限申请的代码入口

| 文件/入口 | 职责 |
| --- | --- |
| `src-tauri/src/hardware_hid.rs` / `start` | 安装 `ListenOnly` Event Tap，监听硬件组合键并进入统一 `process_event` |
| `src-tauri/src/activation.rs` / 功能键执行路径 | 在发送模拟按键前调用 `has_or_request_access()` |
| `src-tauri/src/macos_input.rs` / `has_or_request_access` | `CGPreflightPostEventAccess()` 检查发送权限；未授权时调用 `CGRequestPostEventAccess()` |
| `src-tauri/src/macos_input.rs` / `post_key` | 再次检查发送权限，发送受控的 CGEvent |
| `src-tauri/src/macos_input.rs` / `post_key_applescript` | System Events 备用发送，并输出执行错误 |
| `src-tauri/tauri.conf.json` / `bundle.macOS.signingIdentity` | 控制打包签名 |

应用可以请求系统展示授权流程，不能自行授予权限。当前 `has_or_request_access()` 返回 true 也可能只是允许尝试备用发送，并非授权成功的证明。

| 权限 | 当前用途 | 拒绝后的表现 |
| --- | --- | --- |
| 输入监控 | 监听全局硬件键盘事件 | Event Tap 无法安装，硬件输入可能无法进入 Companion |
| 辅助功能 | 发送 `⌘B` 等模拟键盘事件 | `CGEvent posting denied`；备用发送也可能报 1002 |
| 自动化 | 采用 System Events 备用路径时，可能涉及 Apple Events 授权 | 需根据系统提示和具体错误单独排查 |

输入监控不能替代辅助功能；代码签名和公证也不会自动授予这些权限。开发启动方式可能改变系统显示的负责应用，例如启动开发进程的终端，具体以系统提示为准。

## 4. 本地开发者流程

以下命令在仓库根目录执行。构建产物路径是相对仓库的，不要求其他开发者使用本机的绝对路径。

### 4.1 日常开发与权限验证

UI 日常开发可继续使用：

```bash
npm run tauri:dev
```

验证完整应用的权限行为时，使用 `.app` 构建：

```bash
npm run tauri:build -- --bundles app
```

完全退出已有 Companion，包括开发版和裸二进制进程，然后启动：

```bash
open "src-tauri/target/release/bundle/macos/Agent Deck Companion.app"
```

在「系统设置 → 隐私与安全性」中，按功能需要开启输入监控、辅助功能。若旧条目已开启但仍拒绝发送：退出应用，移除旧 Companion 条目，重新添加刚构建的 `.app` 并开启权限，再启动。

不要在同一轮权限验证中混用 debug 裸二进制、release 裸二进制和 app bundle。应用启用了单实例插件；已有实例可能接管后续启动，影响判断。

### 4.2 检查签名

```bash
codesign --verify --strict --verbose=2 \
  "src-tauri/target/release/bundle/macos/Agent Deck Companion.app"

codesign -dv --verbose=4 \
  "src-tauri/target/release/bundle/macos/Agent Deck Companion.app"

codesign -dr - \
  "src-tauri/target/release/bundle/macos/Agent Deck Companion.app"
```

检查完整 bundle 验证成功，标识为 `dev.agentdeck.companion`。ad-hoc 包显示 `Signature=adhoc`、无 TeamIdentifier 是预期状态，但不是正式证书签名。

### 4.3 ad-hoc 开发包的限制

重新编译会改变构建哈希，系统可能不再认可旧授权。保留相同路径和名称有助于避免混淆，但不能代替稳定的证书身份。需要长期开发或发行时，配置固定签名证书，并实际验证升级前后权限行为；不要承诺每次升级都一定保留权限。

## 5. `.app` 日志查看

当前主要诊断日志使用 Rust `eprintln!`，release 包也会输出；不是只有 debug 编译才有日志。尚未接入自动日志文件或日志级别框架，因此不要假设设置 `RUST_LOG` 会控制这些输出。

### 5.1 开发者收集日志

先完全退出已有 Companion，再从仓库根目录执行：

```bash
open \
  --stdout /tmp/agent-deck.stdout.log \
  --stderr /tmp/agent-deck.stderr.log \
  "src-tauri/target/release/bundle/macos/Agent Deck Companion.app"

tail -F /tmp/agent-deck.stderr.log
```

`open --stdout/--stderr` 是本机已检查支持的选项。已有应用进程不会因为再次 `open` 就重新绑定日志输出。

### 5.2 最终用户收集日志

假设应用安装在 `/Applications`。退出 Companion 后执行：

```bash
open \
  --stdout /tmp/agent-deck.stdout.log \
  --stderr /tmp/agent-deck.stderr.log \
  "/Applications/Agent Deck Companion.app"

tail -F /tmp/agent-deck.stderr.log
```

复现一次故障，记录发生时间、应用版本、连接方式 USB/BLE，再提供该时间附近的日志。日志可能包含工作目录、会话信息等，分享前应检查并遮盖不需要的私人内容。`/tmp` 日志只用于临时排查，不是长期保存方案。

| 日志 | 排查含义 |
| --- | --- |
| `[hardware_hid] unable to install event tap` | 先检查监听权限 |
| `[fn_key] key=K8 ...` | Companion 收到 K8 |
| `[fn_key] execution=...` | 检查解析出的动作及目标 Agent |
| `[fn_key] focused=false` | 目标聚焦失败，未发送按键 |
| `[macos_input] CGEvent posting denied` | 系统未授予发送权限 |
| `[macos_input] System Events failed ... (1002)` | 备用发送也被系统拒绝 |
| `[vendor_hid] ... ACK` | 对应 vendor 命令确认，不代表桌面 sidebar 操作成功 |
| `ChatGPT quota unavailable: codex binary not found` | 配额依赖定位失败，与 sidebar 发送权限独立 |

## 6. 正式分发流程：发布维护者

正式分发应使用固定的 Developer ID Application 证书完成签名与公证。Tauri 支持在 `bundle.macOS.signingIdentity` 或 `APPLE_SIGNING_IDENTITY` 环境变量中指定签名身份；公证配置按官方文档建立。仓库当前还没有可直接运行的完整正式发布流水线。[Tauri 签名与公证文档](https://tauri.app/distribute/sign/macos/)

1. 确认版本、目标架构、最低 macOS 版本和固件兼容性。当前硬件键区为 F16–F20，固件与 Companion 必须匹配。
2. 配置固定签名身份；CI 的证书私钥、证书密码和公证凭据放在受控 secrets 中，不写入仓库。
3. 构建完整 `.app` 和最终分发包，完成公证；确认票据 stapling 和最终产物验证结果。
4. 对最终下载包执行签名及 Gatekeeper 检查，在未安装过应用、未授过权限的另一台 Mac 上测试。
5. 使用正常下载、安装、首次启动流程验证，避免仅凭开发机已有授权判断发布成功。
6. 从旧版本升级，确认签名身份连续、权限行为和硬件功能。

检查命令示例（替换为实际最终产物路径）：

```bash
security find-identity -v -p codesigning
codesign --verify --strict --verbose=2 "/path/to/Agent Deck Companion.app"
spctl --assess --type execute --verbose=4 "/path/to/Agent Deck Companion.app"
xcrun stapler validate "/path/to/Agent Deck Companion.app"
```

不要通过签名后修改 `.app` 内容来修补资源或配置；修改后需要重新签名，并重新完成对应的公证验证。构建 `.app` 的基础流程参考 [Tauri application bundle 文档](https://tauri.app/distribute/macos-application-bundle/)。

## 7. 最终用户安装与首次使用

此流程适用于未来完成证书签名、公证的正式版本：

1. 下载对应架构的发布包，把 `Agent Deck Companion.app` 安装到 `/Applications`。
2. 从 Finder 或 Launchpad 打开应用。
3. 根据应用提示，在「系统设置 → 隐私与安全性」中授予输入监控和辅助功能。授权后按提示重新启动或重试动作。
4. 验证 A1–A5 切换 Agent，ChatGPT K8 短按切换 sidebar，长按打开配额页。
5. 拒绝权限时应仍能使用不依赖该权限的功能；发布前需验证并补齐清晰的状态提示。

最终用户不需要 Node.js、Rust、npm 或手动执行 `codesign`。当前 ad-hoc 内测包需明确标为内测，不能套用正式包已经完成公证的描述。用户看到签名或来源异常提示时，应由维护者检查产物，不能把关闭系统安全保护作为标准安装流程。

## 8. 发布前待办与验收

- [ ] 固定证书签名、公证、stapling，以及最终分发包验证。
- [ ] 首次使用界面分别说明监听与发送权限，拒绝后可重试，不把 HTTP 200 当成动作成功。
- [ ] 显示权限状态、发送失败和签名/版本信息，避免只能看终端日志。
- [ ] 持久化日志、轮转、日志导出及个人信息处理策略。
- [ ] 明确配额功能对 Codex 可执行文件的依赖和定位方式，缺失时显示可理解的状态。
- [ ] 新用户安装、旧用户升级、USB/BLE、K8 短按/长按的实机测试。
- [ ] 确认 F16–F20 无系统亮度副作用，全部 K1–K10 动作正常。
- [ ] 对所宣称支持的每种 macOS 版本和 CPU 架构完成验证。

本文记录的已完成验证：完整 ad-hoc `.app` 签名通过；通过 `open` 启动并授权后，用户确认 sidebar 恢复。正式公证、跨版本授权保留、其他用户 Mac 上的安装体验尚未验证。
