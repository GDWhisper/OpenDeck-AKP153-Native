# OpenDeck 插件开发指导文档

> 本文档根据 OpenDeck 源码（`src-tauri/src/plugins/`、`src-tauri/src/events/`）整理，
> 描述宿主实际实现的协议行为。协议兼容 Elgato Stream Deck SDK 与 [OpenAction](https://openaction.amankhanna.me/) API。
> 内置示例插件见 `plugins/com.amansprojects.starterpack.sdPlugin/`。

---

## 1. 总体架构

OpenDeck 是宿主进程，插件是独立进程（或 webview），通过 WebSocket 通信：

```
设备硬件 ←HID→ OpenDeck 宿主 ←WebSocket(JSON)→ 插件进程
                    ↑
              HTTP 静态服务（图标、Property Inspector 页面）
```

- **WebSocket 端口**：`PORT_BASE`，从 57116 起探测第一个可用端口（`plugins/mod.rs`）。
- **HTTP 静态服务端口**：`PORT_BASE + 2`，托管插件目录内静态资源。
- 插件启动时通过命令行参数拿到端口，主动连接 `ws://localhost:PORT_BASE`。
- 插件未连接时，宿主发出的消息会缓存在 `PLUGIN_QUEUES`，连接后补发，不会丢事件。

## 2. 插件目录与安装位置

每个插件是一个目录，目录名即插件 UUID（惯例以 `.sdPlugin` 结尾）：

```
<config_dir>/plugins/<com.example.myplugin.sdPlugin>/
├── manifest.json              # 必需
├── manifest.{windows|macos|linux}.json  # 可选，平台覆盖（json-patch merge）
├── <二进制/脚本/HTML>          # CodePath 指向的入口
├── icons/…                    # 图标（png/svg，manifest 内路径不带扩展名也可）
└── propertyInspector/…        # PI 的 HTML/CSS/JS
```

配置目录位置：

| 平台 | 路径 |
|---|---|
| Windows | `%appdata%\opendeck\plugins\` |
| Linux | `~/.config/opendeck/plugins/` |
| macOS | `~/Library/Application Support/opendeck/plugins/` |
| Flatpak | `~/.var/app/me.amankhanna.opendeck/config/opendeck/plugins/` |

支持符号链接：开发时可把仓库目录 symlink 进 plugins 目录。

## 3. manifest.json

宿主解析代码：`src-tauri/src/plugins/manifest.rs` + `src-tauri/src/shared.rs`（`Action`/`Encoder`/`ActionState`）。
字段名兼容 Stream Deck SDK 的大驼峰（`Name`）写法。

### 3.1 顶层字段

| 字段 | 必需 | 说明 |
|---|---|---|
| `Name` / `Author` / `Version` / `Icon` | ✅ | 基本信息；`Version` 需为 semver |
| `OS` | ✅ | 数组，如 `[{ "Platform": "windows" }]`；取值 `windows` / `mac` / `linux` |
| `Actions` | ✅ | 动作数组，见 3.2 |
| `Category` | ❌ | 动作列表分类名，默认 `"Custom"` |
| `CategoryIcon` | ❌ | 分类图标路径 |
| `CodePath` | ❌* | 通用入口路径 |
| `CodePathWin` / `CodePathMac` / `CodePathLin` | ❌* | 平台专用入口（OpenDeck 扩展：`CodePathLin`） |
| `CodePaths` | ❌* | **OpenDeck 扩展**：Rust target triple → 二进制路径映射，优先级最高，如 `"x86_64-pc-windows-msvc": "bin/plugin.exe"` |
| `PropertyInspectorPath` | ❌ | 所有动作的默认 PI 页面 |
| `DeviceNamespace` | ❌ | **OpenDeck 扩展**：声明为设备插件，见 §8 |
| `ApplicationsToMonitor` | ❌ | 平台 → 进程名数组；进程启动/退出时收到 `applicationDidLaunch` / `applicationDidTerminate` |
| `HasSettingsInterface` | ❌ | **OpenDeck 扩展**：声明有全局设置界面，宿主会发 `showSettingsInterface` 事件 |

\* 入口路径至少需要一种。入口选择顺序（当前平台匹配时）：`CodePaths[TARGET]` > `CodePath{Win|Mac|Lin}` > `CodePath`。

### 3.2 Action 字段

| 字段 | 默认 | 说明 |
|---|---|---|
| `Name` / `UUID` | 必需 | UUID 惯例为反向域名，如 `com.example.myplugin.myaction` |
| `Icon` | `""` | 动作图标 |
| `Tooltip` | `""` | 悬浮提示 |
| `States` | 必需 | 状态数组；`"Image": "actionDefaultImage"` 表示复用动作图标 |
| `Controllers` | `["Keypad"]` | 可含 `"Keypad"`、`"Encoder"` |
| `SupportedInMultiActions` | `true` | 是否可放入 Multi Action |
| `VisibleInActionsList` | `true` | 是否在动作列表显示 |
| `DisableAutomaticStates` | `false` | 禁用按键自动切换状态 |
| `PropertyInspectorPath` | 继承顶层 | 该动作专用 PI 页面 |
| `Encoder` | — | 旋钮配置（`Icon`、`StackColor`、`TriggerDescription`、`layout` 等） |

### 3.3 平台覆盖

存在 `manifest.windows.json` / `manifest.macos.json` / `manifest.linux.json` 时，
宿主用 `json_patch::merge` 合并到主 manifest（仅当前平台的文件生效）。

## 4. 插件运行方式

宿主按入口文件类型决定运行方式（`plugins/mod.rs::initialise_plugin`）：

| 入口 | 运行方式 | 备注 |
|---|---|---|
| `.html` / `.htm` / `.xhtml` | 隐藏 Tauri webview 加载 `http://localhost:PORT+2/<路径>` | 页面需定义 `connectElgatoStreamDeckSocket` 或 `connectOpenActionSocket` 全局函数，宿主注入调用；插件目录放一个名为 `debug` 的文件可显示窗口并开 devtools |
| `.js` / `.mjs` / `.cjs` | `node <入口> <参数>` | 要求 Node.js ≥ 20 |
| 其他（二进制） | 直接子进程 | Unix 下自动 `chmod 755` |
| 仅 Windows 二进制且当前非 Windows | `wine <入口> <参数>` | 设置开启 `separatewine` 时使用插件目录内独立 `wineprefix` |

子进程插件启动参数（与 Stream Deck SDK 相同）：

```
<binary> -port <PORT> -pluginUUID <UUID> -registerEvent registerPlugin -info <JSON>
```

`-info` JSON 内容（`plugins/info_param.rs`）：

```json
{
	"application": { "font": "...", "language": "en", "platform": "windows", "platformVersion": "...", "version": "7.1.0" },
	"plugin": { "uuid": "...", "version": "..." },
	"devicePixelRatio": 0,
	"colors": { ... },
	"devices": [ { "id": "...", "name": "...", "size": { "rows": 3, "columns": 5 }, "type": 0 } ]
}
```

插件 stdout/stderr 重定向到 `<log_dir>/plugins/<uuid>.log`。

## 5. 注册流程

插件连接 WebSocket 后，**第一条消息必须是注册事件**：

```json
{ "event": "registerPlugin", "uuid": "<插件UUID>" }
```

Property Inspector 用：

```json
{ "event": "registerPropertyInspector", "uuid": "<ActionContext字符串>" }
```

注册后宿主立即补发排队消息，随后开始正常事件收发。连接断开即视为插件下线（socket 从 `PLUGIN_SOCKETS` 移除）。

## 6. 事件参考

所有消息为 JSON 文本帧，以 `event` 字段区分类型（camelCase）。

### 6.1 宿主 → 插件（outbound，`src-tauri/src/events/outbound/`）

| 事件 | 触发时机 |
|---|---|
| `willAppear` / `willDisappear` | 动作实例出现/消失（切换 profile、增删实例） |
| `keyDown` / `keyUp` | 按键按下/抬起（含 Multi Action 模拟按压） |
| `dialDown` / `dialUp` / `dialRotate` | 旋钮按下/抬起/旋转（`ticks`，负为逆时针） |
| `touchTap` | 触摸屏点击 |
| `didReceiveSettings` | 响应 `getSettings`，或 PI 改动设置时 |
| `didReceiveGlobalSettings` | 响应 `getGlobalSettings`，或另一端改动全局设置时 |
| `titleParametersDidChange` | 用户在 UI 编辑标题/字体 |
| `sendToPlugin` | PI 调用 `sendToPlugin` |
| `propertyInspectorDidAppear` / `propertyInspectorDidDisappear` | PI 打开/关闭 |
| `deviceDidConnect` / `deviceDidDisconnect` | 设备连接/断开 |
| `applicationDidLaunch` / `applicationDidTerminate` | `ApplicationsToMonitor` 命中的进程启动/退出 |
| `systemDidWakeUp` | 系统从睡眠唤醒（广播所有插件） |
| `didReceiveDeepLink` | `opendeck://` 深链接 |
| `showSettingsInterface` | 用户点击插件设置（需 `HasSettingsInterface`） |
| `setImage` / `setBrightness` | 仅设备插件（§8）：宿主要求刷新按键图像/亮度 |

事件载荷格式与 Stream Deck SDK 一致，例如 `keyDown`：

```json
{
	"event": "keyDown",
	"action": "com.example.myplugin.myaction",
	"context": "<ActionContext>",
	"device": "<设备ID>",
	"payload": {
		"settings": {}, "coordinates": { "row": 0, "column": 1 },
		"controller": "Keypad", "state": 0, "isInMultiAction": false
	}
}
```

`context` 是不透明字符串，插件原样回传即可。

### 6.2 插件/PI → 宿主（inbound，`src-tauri/src/events/inbound/mod.rs::InboundEventType`）

**动作类**（需 `context`，宿主校验实例归属，见 §7）：

| 事件 | 说明 |
|---|---|
| `setSettings` / `getSettings` | 实例设置读写（持久化到 profile） |
| `setTitle` | `payload: { title?, state?, target? }` |
| `setImage` | `payload: { image?, state? }`；image 为 data URI 或空（恢复默认） |
| `setState` | 切换实例状态 |
| `showAlert` / `showOk` | 按键上闪告警/成功图标 |
| `setFeedback` / `setFeedbackLayout` | Stream Deck+ 触摸屏布局 |
| `sendToPropertyInspector` | 发消息给该实例的 PI |

**插件级**：

| 事件 | 说明 |
|---|---|
| `setGlobalSettings` / `getGlobalSettings` | `context` 必须为插件自身 UUID |
| `openUrl` | `payload: { url }` |
| `logMessage` | `payload: { message }`，写入宿主日志 |
| `sendToPlugin` | 仅 PI 使用，转发给插件 |
| `switchProfile` / `deviceBrightness` | **特权事件**，仅内置 starterpack 插件可用 |

**设备类**（设备插件专用，见 §8）：`registerDevice`、`deregisterDevice`、`rerenderImages`、
`keyDown`、`keyUp`、`encoderChange`、`encoderDown`、`encoderUp`、`touchscreenPress`。

## 7. 鉴权规则

宿主对 inbound 消息做归属校验（`process_incoming_message`）：

- 带 `context` 的动作类事件：该 context 对应实例必须属于发送插件，否则静默丢弃。
- `setGlobalSettings` / `getGlobalSettings`：`context` 必须等于插件 UUID。
- `switchProfile` / `deviceBrightness`：仅 `com.amansprojects.starterpack.sdPlugin` 可发。
- PI 连接：只能操作自己注册时的 context。

## 8. 设备插件（DeviceNamespace，OpenDeck 扩展）

插件可以为非 Elgato 硬件提供支持：

1. manifest 声明 `"DeviceNamespace": "xx"`（两字符前缀，注册的设备 ID 必须以此开头）。
2. 插件自行通过 HID 等方式连接硬件，向宿主发送：
   - `registerDevice`：`payload` 为 `DeviceInfo`（`id`、`name`、`rows`、`columns`、`encoders`、`type` 等，见 `shared.rs`）。
   - 硬件输入转发：`keyDown` / `keyUp` / `encoderChange` / `encoderDown` / `encoderUp` / `touchscreenPress`（`payload` 含 `device` 与位置信息）。
   - 断开时 `deregisterDevice`。
3. 宿主渲染好按键图像后，向插件发 `setImage`（data URI）与 `setBrightness`，插件负责写入硬件。
4. `rerenderImages` 可要求宿主重发某设备全部图像。

注意：同一硬件不要同时被原生后端和设备插件接管，会重复注册/HID 冲突。

## 9. Property Inspector

- PI 是宿主 UI 中的 iframe，加载 `http://localhost:PORT+2/<PI路径>`，带 query 参数并由宿主调用页面的
  `connectElgatoStreamDeckSocket(port, uuid, registerEvent, info, actionInfo)`。
- PI 建立**独立** WebSocket 连接，注册事件为 `registerPropertyInspector`。
- PI 可用事件：`setSettings` / `getSettings`、`setGlobalSettings` / `getGlobalSettings`、
  `sendToPlugin`、`openUrl`、`logMessage`。
- 与插件通信：PI `sendToPlugin` → 插件收到 `sendToPlugin`；插件 `sendToPropertyInspector` → PI 收到 `sendToPropertyInspector`。
- 样式惯例：`sdpi.css`（参考 starterpack 的 `assets/propertyInspector/`）。

## 10. 开发、调试、发布

### 推荐路径（Rust）

用 [`openaction`](https://crates.io/crates/openaction) crate，参考 starterpack 源码
（`plugins/com.amansprojects.starterpack.sdPlugin/src/`）：注册事件处理器 trait，crate 处理 WebSocket/注册细节。

### 调试

- 宿主日志：`deno task tauri dev` 终端直接可见；或 `<log_dir>/logs/`。
- 插件日志：`<log_dir>/plugins/<uuid>.log`（stdout/stderr）。
- HTML 插件：目录放 `debug` 空文件 → 窗口可见 + devtools。
- PI 调试：右键宿主 UI → Inspect Element。
- 开发迭代：symlink 插件目录，改代码后在宿主"Plugins"页重新加载或重启宿主。

### 发布

打包为 `.streamDeckPlugin` / zip（顶层为 `<uuid>.sdPlugin/` 目录）。OpenDeck 支持从
[插件市场](https://marketplace.rivul.us/) 或本地文件安装；也支持 `opendeck://` 深链接安装。

## 11. 协议疑问的最终依据

文档与实现冲突时，以源码为准：

- 入站事件（插件→宿主）：`src-tauri/src/events/inbound/`
- 出站事件（宿主→插件）：`src-tauri/src/events/outbound/`
- 启动/生命周期：`src-tauri/src/plugins/mod.rs`
- manifest 解析：`src-tauri/src/plugins/manifest.rs`、`src-tauri/src/shared.rs`
