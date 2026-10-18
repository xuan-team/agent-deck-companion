# dsh-deck-bridge 安装说明

让 Agent Deck 实体键盘通过 **DSH 自己的 HTTP 接口**切换网页里的 session,不再依赖 macOS 对 Chrome 的 Apple Events 授权。

装好后你会得到:

- 按 K1/K2/K3 → 后台的 DSH 标签页直接换 session,成功后 Companion 尽力聚焦已有标签页。
- 按 A3 激活 DSH → 复用已有的 DSH 标签页,**不会再新开一个 tab**。

## 它是怎么工作的

```
companion ──POST /api/deck/select──▶ DSH 宿主插件 ◀──GET /api/deck/next── Chrome 标签页
    ▲                                      │                                      │
    └───── acked:true ◀── POST /api/deck/ack ◀── uiWorkspace.openSession(id) ────┘
```

这些请求都在 DSH 自己的 `/api` 前缀下,所以 Host/Origin 栅栏和 cookie 鉴权由 DSH 统一施加,插件不用自己实现认证。浏览器用普通 HTTP 长轮询接收指令，`select` 等第一个 ack（最多 5 秒）才返回。页面无法打开目标 session 时回 nack，Companion 会看到 `client-rejected`。

`GET /api/deck/tabs` 额外回答"现在有几个标签页连着、分别是什么浏览器",这是 A3 敢复用已有 tab 的依据。

| 文件 | 跑在哪 | 作用 |
| --- | --- | --- |
| `lib/index.js` | DSH 宿主进程 | 登记长轮询、select、ack/nack 和 tab 查询路由；保留旧 SSE 路由兼容旧页面 |
| `lib/client.js` | 浏览器页面 | 长轮询 → 调 DSH 导航 API → 回 ack/nack |
| `cordis.patch.yml` | 声明文件 | 把模块插进 profile 的模块树 |

DSH 0.1.7 把"打开 session"从 `sessions.open()` 挪到了 `uiWorkspace.openSession()`。0.3.1 起页面每次切换时探测两个名字,所以同一份插件在新旧版本上都能用。

0.3.2 已核对 DSH 0.2.0-rc.2 的导航与宿主路由 API，并扩展 peerDependencies 兼容声明。
无需通过 `allow-version --accept-risk` 绕过版本检查。

### 主动上报（0.4.0 起）

```
DSH 宿主插件 ──POST 127.0.0.1:34115/api/dsh/push──▶ companion
              （api-session/added|removed|status|activity|error）
```

companion 原先每 5 秒读一次 DSH 的投影文件。插件改成同时订阅 DSH 自己的
`api-session/*` 事件——也就是浏览器侧边栏渲染用的同一份事件——一有变化就通知 companion 立刻去看。

- **推送只是"去看一眼"的信号**，不携带状态。companion 收到后仍用它自己的读取器解析 session，
  所以 DSH 的语义只有一份实现，丢一次推送只损失延迟，不会出现错误状态。
- 订阅的是 `api-session/added`/`removed`/`status`/`activity`/`error`：粒度正好是"会话列表变了"。
  更细的 `agent/status`、`session/event`（每个 turn、每个 token 都发）没有订阅——companion 重新
  采样时投影文件已经是新的。
- 令牌读 companion 自己写的 `~/.agent-deck-companion/hook-token`（0600），
  端口默认 34115，可用 `AGENT_DECK_COMPANION_PORT` 覆盖；连不上就静默降级。
- **轮询仍然是兜底**：收到推送后 companion 把 DSH 轮询放宽到 30 秒，推送停止后自动回到 5 秒。
- 命令事件在 400ms 内合并成一次推送；单次推送超时 800ms，且**永不阻塞 DSH 宿主**。

只在 **DSH 宿主**里生效，页面侧不需要改动，所以改完只需重启 DSH web（不必刷新也行，
但刷新无妨）。companion 侧不重启也能收——重启 companion 时若插件已在跑，会等下一次事件。

Session 切换通过 HTTP 完成，不需要 macOS 输入或浏览器自动化权限。将已有 tab 带到前台
仍由 Companion 的 Apple Events 标签页聚焦逻辑完成，可能需要系统现有的浏览器自动化授权。

## 前置条件

- 已能启动 DSH web:默认用 `npx @deepseek-ai/dsh web`。DSH 用自带的一份 pnpm 管理 profile,所以你不需要单独装 pnpm。
- companion 是本仓库构建出的版本(`dsh-deck-bridge` 0.3.0 起)。
- 首次运行过 `npx @deepseek-ai/dsh web`,这样 `~/.dsh/.credentials.yaml` 已存在 —— companion 要用它签一个 loopback 鉴权 cookie。

profile 目录默认是 `~/.dsh/profiles/<名字>`;如果设置了 `DSH_HOME`,就是 `$DSH_HOME/profiles/<名字>`。下面统一用 `web` 这个 profile 名,请按你自己的改名。

## 安装

**最快**:打开 Agent Deck Companion → **Hooks** 页 → `DeepSeek Harness 插件` 一行 →「安装插件」。
它内部执行的就是下面这条 `npx` 命令,并会显示识别到的 profile 与版本。手工安装步骤保留在下面。

### 命令入口:一律写 `npx @deepseek-ai/dsh`

`dsh` **默认不在用户 PATH 里**(npx 装的,只会落在 `~/.npm/_npx/<hash>/node_modules/.bin/dsh`)。所以本文所有命令都写成完整形式:

```bash
npx @deepseek-ai/dsh plugin --profile web list
```

`fish: Unknown command: dsh` / `command not found: dsh` 不代表 DSH 没装,只是没进 PATH。想省掉前缀可以自己把那个软链加进 PATH,之后本文的 `npx @deepseek-ai/dsh` 等价替换成 `dsh` 即可。

### 一条命令(profile 已存在时)

```bash
npx @deepseek-ai/dsh plugin --profile web add link:/绝对路径/agent_deck_companion/dsh-deck-bridge
npx @deepseek-ai/dsh plugin --profile web list        # 应当看到 dsh-deck-bridge@link:…
```

路径要写**绝对路径**并带 `link:` 前缀。在插件目录里跑 `… plugin --profile web add ./` 会让 profile 记下一个相对路径,以后换个 cwd 就指不到东西了。

`dsh plugin` 是 DSH 自带的一份 pnpm 在 profile 目录里的转发:它写依赖,然后自动把**声明了 `dsh.bundle`** 的包追加进 `dsh.profile.bundles` —— 也就是说不用手改 bundles 列表,也不用自己跑 `pnpm install`。

依赖前缀请用 **`link:`**(软链),不要用 `file:`。

### ⚠️ 为什么不能用 `file:` 凑合

`~/.dsh/profiles/web` 用的是 pnpm 的 hoisted linker,`file:` 目录依赖只在**第一次安装时拷贝一次**。之后不管你怎么改源码、怎么 `pnpm install --force`,pnpm 都只会打印 `resolved dsh-deck-bridge … Already up to date`,而 `node_modules/dsh-deck-bridge` 里还是旧文件 —— 表现就是 companion 报:

```
[dsh] http-select session=… failed: DSH deck/select returned HTTP 404 Not Found
```

`link:` 建的是软链,以后改插件源码只要重启 DSH 就生效,不用再装。如果你确实想要一份冻结快照(`file:`),那每次更新后必须**删掉旧拷贝再装**:

```bash
rm -rf ~/.dsh/profiles/web/node_modules/dsh-deck-bridge
npx @deepseek-ai/dsh plugin --profile web add file:/绝对路径/agent_deck_companion/dsh-deck-bridge
```

### 手工安装(等价做法)

想在编辑器里看清楚状态,就直接改 profile 的 `package.json`:

```json
{
  "dependencies": {
    "dsh-deck-bridge": "link:/绝对路径/agent_deck_companion/dsh-deck-bridge"
  },
  "dsh": {
    "profile": {
      "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "dsh-deck-bridge"]
    }
  }
}
```

然后确认 `dsh.profile.bundles` 里**有** `"dsh-deck-bridge"`(它在列表中的位置决定 patch 顺序,一般追加在末尾),再让 DSH 自己那份 pnpm 去装 —— 你机器上很可能没有全局 `pnpm`,所以仍走 dsh 入口:

```bash
npx @deepseek-ai/dsh plugin --profile web list
```

`list` 会解析 profile 依赖;报缺包就补跑一次上面的 `add link:…`。

### 验证装对了(不用重启任何东西)

```bash
cd ~/.dsh/profiles/web
node -e "console.log(require('./node_modules/dsh-deck-bridge/package.json').version)"
grep -c "deck/select" node_modules/dsh-deck-bridge/lib/index.js
grep -c "api-session/status" node_modules/dsh-deck-bridge/lib/index.js
```

版本应当 ≥ `0.4.0`(0.1.7 之后的 DSH 需要 0.3.1+;主动上报需要 0.4.0+),第二、三条命令应当分别输出 `2` 和 `1`。输出 `0` 就说明装的还是旧拷贝 —— 回到上面两节。

### 让路由真的注册进 DSH

宿主的 `/api/deck/*` 是在 DSH 启动时登记的,所以:

1. **重启 DSH web**:`npx @deepseek-ai/dsh web`(会打断正在跑的 DSH agent 会话,挑合适的时候)。
2. **刷新 Chrome 里的 DSH 页面**。`client.js` 在页面加载时启动长轮询；不刷新就仍运行旧版本页面模块。
3. companion 不用重启。

### 端到端确认

在 DSH 页面按 ⌥⌘J 开 DevTools Console,执行:

```js
await fetch("/api/deck/tabs").then((r) => r.json());
```

- `{ok: true, tabs: 1, browsers: ["chrome"]}` → 装好了。
- `404` → 宿主没加载到新代码,重装 + 重启 DSH。
- `{tabs: 0}` → 路由在,但页面没刷新(或刷新后被别的 tab 顶掉了)。

然后按一下 K1/K2,companion 日志应出现:

```
[dsh] deck/tabs DeckTabs { tabs: 1, browsers: ["chrome"] }
[dsh] http-select session=session-… acked=true
```

`acked=true` 才代表页面真的切了 session。

主动上报的确认（0.4.0+，重启 DSH 后）：

- DSH 宿主输出（跑 `dsh web` 的那个终端）应当出现一次：

  ```
  dsh-deck-bridge push: connected to companion at http://127.0.0.1:34115/api/dsh/push
  ```

  之后每次会话列表变化，companion 侧会打印 `[dsh] push reason=… sessions=N`。
- Companion 主窗口 → **设置 → Agent 监控 → DSH 集成 → 「插件推送」** 应显示
  `已收到 N 次推送 …（轮询降为兜底）`；若是 `未收到推送（仍每 5 秒轮询）`，说明宿主还是旧插件
  或 companion 没在跑。

## 卸载

```bash
npx @deepseek-ai/dsh plugin --profile web remove dsh-deck-bridge
```

重启 DSH。没有插件时 K1-K3 会返回失败，不会模拟鼠标点击或请求 Chrome 脚本权限。

## 故障对照

| 日志 / 报错 | 含义 | 处理 |
| --- | --- | --- |
| `Unknown command: dsh` | DSH 没进 PATH,不是没装 | 改用 `npx @deepseek-ai/dsh plugin …` |
| `deck/select returned HTTP 404` | 宿主跑的是旧插件 | 用"验证装对了"两条命令确认;是旧拷贝就换 `link:` 重装,然后重启 DSH web |
| `reason: "no-client"` | 没有页面连着 | 刷新 DSH 页面;确认 DSH tab 没被关 |
| `reason: "ack-timeout"` | 页面曾连接但没确认切换 | 刷新 DSH 页面，查看 Console 中的 `dsh-deck-bridge poll:` 或 `dsh-deck-bridge response:` 报错 |
| `reason: "client-rejected"` | 页面收到指令，但 DSH 的 `uiWorkspace.openSession()` / `sessions.open()` 拒绝该 ID | 检查该 session 是否仍在 DSH 网页的列表中 |
| `detail=sessions.open is not a function` 或 `no session navigation API` | 页面跑的是 0.3.0 的插件，DSH 已把导航 API 换成 `uiWorkspace.openSession()` | 升级到 0.3.1 并刷新 DSH 页面 |
| 「插件推送」一直显示 `未收到推送` | 宿主插件 < 0.4.0，或 companion 未运行 | 升级插件并重启 DSH web；确认 `~/.agent-deck-companion/hook-token` 存在（在 Companion 里装过一次 Hooks 即会生成） |
| DSH 宿主输出 `push: connect ECONNREFUSED` | companion 没在跑 | 正常降级为 5 秒轮询；启动 companion 后下一次事件即恢复 |
| A3 每次新开一个 tab | 插件太旧,不报 `deck/tabs`,companion 无从判断已有 tab | 升级到 0.3.0 以上 |
| DSH 不在 3080 | — | companion 从 `dsh web --port` 自动识别,无需配置 |

多个 DSH tab 会**一起**切换 —— 这块屏的语义是"指向一个 session",不是"某一个浏览器窗口"。

## 安全边界

- 全部流量都在 loopback 上,并且经过 DSH `/api` 的 Host/Origin 栅栏和浏览器鉴权;栅栏是在分发到路由**之前**施加的,插件无法绕过或忘记加。
- session id 只做正则校验和原样转发(`^session-[A-Za-z0-9_-]{1,72}$`),永不拼接执行;`nonce` 同理限制为小写十六进制。
- 插件不读写任何业务数据,也不接受"执行命令"这类指令 —— 它能做的只有让页面显示某个 session。
