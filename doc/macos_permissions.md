# macOS 权限与本地构建

Companion 主窗口显示「辅助功能」与「输入监控」的实际授权状态，并提供「申请权限」和「打开设置」。返回主窗口时重新检查权限；点击申请后会短暂轮询状态，但不会在轮询中反复弹窗。

- 辅助功能：发送快捷键、自动化操作。点击申请后，系统可引导用户进入设置；用户仍需手动启用。
- 输入监控：全局键盘事件监听（硬件兼容路径）。申请使用 `IOHIDRequestAccess` 并配合一次监听尝试，兼容 `CGRequestListenEventAccess`；授权后应用会自动重试安装事件监听器，若仍未生效再退出并重新启动。Vendor HID 数据通道不依赖该监听权限。
- 若系统既不弹窗、列表里也看不到 Companion（macOS 15 上 ad-hoc 构建可能出现），请在「输入监控」中点 `+` 手动添加 `/Applications/Agent Deck Companion.app`，或移除旧条目后重新添加。
- 权限不会由应用或脚本自动授予。无需屏幕录制权限。

## 为什么重新打包可能需要移除旧授权

当前 `src-tauri/tauri.conf.json` 使用 `signingIdentity: "-"`，即 ad hoc 临时签名。代码内容变化可能改变代码身份，旧的 TCC 授权无法作为新版本的有效授权。Apple 明确指出 ad hoc designated requirement 绑定具体版本的代码；正常保留隐私授权需要版本间兼容的签名要求。参见 [Apple TN3127](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)。

正式分发应使用同一 Developer ID 身份签名，开发迭代也应使用稳定的签名身份，保持 bundle ID `dev.agentdeck.companion` 和安装位置一致。不要在多个 build 目录或重复安装副本之间交替授权。开发签名与正式签名之间切换仍可能要求重新授权。

有签名证书后，可覆盖仓库的临时签名（替换下列身份名称）：

```bash
npm run tauri:build -- --bundles app --config '{"bundle":{"macOS":{"signingIdentity":"Developer ID Application: YOUR NAME (TEAMID)"}}}'
```

## 开发时重置过期授权

先退出 Companion。预览命令：

```bash
bash scripts/reset-macos-permissions.sh --dry-run
```

确需清除旧授权时执行：

```bash
bash scripts/reset-macos-permissions.sh --reset
```

脚本仅调用 `tccutil reset Accessibility dev.agentdeck.companion` 和 `tccutil reset ListenEvent dev.agentdeck.companion`，不会清除其他应用授权、修改 TCC 数据库或自动批准权限。它不是每次 build 的后置步骤。重新打开固定位置的应用，使用主窗口的申请按钮并在系统设置中授权；输入监控授权后重启。

辅助功能申请使用 [AXIsProcessTrustedWithOptions](https://developer.apple.com/documentation/applicationservices/1459186-axisprocesstrustedwithoptions)；只读检查不会触发申请。macOS 决定提示的形式和频率，已拒绝的权限可通过「打开设置」手动启用。
