# OpenCode v2 状态接入

适配并核对本机版本：2.0.26。使用 [v2 插件接口](https://opencode.ai/v2/docs/build/plugins)；事件字段按 [2.0.26 session schema](https://github.com/anomalyco/opencode/blob/v2.0.26/packages/schema/src/session-event.ts) 和 [TUI Context](https://github.com/anomalyco/opencode/blob/v2.0.26/packages/plugin/src/tui/context.ts) 核对。

## 安装

在 Companion 的 Hooks 设置中安装 OpenCode。应用写入：

- `~/.config/opencode/plugins/agent-deck-companion/index.js`：服务端事件订阅。
- 同目录 `tui.js`：TUI 缓存状态及实际 Herdr pane 绑定。

沿用 `~/.agent-deck-companion/hook-token` 认证与 loopback HTTP；不修改 `opencode.json`、模型、权限或其他插件。已有同名非本应用文件会拒绝覆盖；卸载仅删除带本应用标记的文件。安装状态同时校验两个文件。自定义 OpenCode 配置目录的用户需将两个入口放到相应目录；当前安装器使用上述默认目录。

支持本地插件热加载的实例可自动加载；未加载时待当前任务结束后重新启动 OpenCode。不要通过全量 location reload 强制安装，因为它可能取消正在等待的权限 / 问题。

## 状态与活动

| 原生事件 | Companion |
| --- | --- |
| execution.started | working，单独记提交时间 |
| execution.succeeded | 完成，沿用现有 idle / unread 完成投影；单独记完成时间 |
| execution.failed | error |
| execution.interrupted | idle，不伪造完成 |
| permission.asked / form.created | needs_input |
| permission.replied / form.replied / form.cancelled | 清除对应阻塞，再聚合该会话其他工作 |
| text.delta | 400ms 合并，只更新原生输出时间 |
| session.renamed / moved | 更新元数据 |
| session.deleted | offline |

不发送正文、推理内容、工具参数或用户输入。输出、已送达按键、提交与完成保持独立；输出覆盖为 `native_output_keyboard_partial`，不宣称观察到用户直接敲入的每个键。重复事件去重、时间单调；发送超时和断连不会阻塞 agent，重试队列有上限，长时间离线可能丢弃早期事件。

## 身份和性能边界

v2 服务可共享，不能用服务进程继承的 Herdr 环境或 PID 给所有 session 定位。服务端按原生 ID 更新；TUI 才报告自己的实际 pane，后端验证同一 Herdr socket 和实时 OpenCode pane。没有可靠定位的会话保持 Unknown transport，继续遵循“不展示无法定位目标”的规则，不按项目目录猜测跳转。

每个 location 插件仅转发所属根会话，子会话的阻塞归并到根，不把子会话完成当作整个任务完成。原生状态不会被过期的 Herdr 启发式覆盖；pane 关闭仍退出显示。

TUI 每 250ms 仅读内存缓存，变化才发送；每秒读取轻量 `/api/instance`，Companion 重启后重新绑定。后台不调用 `pane.read`，不抢焦点。正常连接时以约 1.5s 为状态感知目标；这不是所有来源的端到端保证。DSH 仍有 5s 查询，Codex 新列表发现也有独立刷新周期。

## 验证

`node --test integrations/opencode-v2/plugin.test.mjs integrations/opencode-v2/tui.test.mjs` 覆盖状态事件、子会话阻塞、跨项目过滤、重复事件、输出合并 / 正文不外传、重试、TUI 选择、重启恢复与清理。Rust 回归覆盖安装幂等 / 配置保护、晚发现 pane 的原生 ID 关联、状态优先级与 pane 退出。

真实服务已确认插件 active、两个 TUI pane 的不同原生 ID 关联；独立临时测试会话的表单创建 / 取消到 Companion API 状态约 85ms / 70ms，测试后删除，无模型调用。屏幕绘制、实体按键及长时 CPU 仍需现场验收。
