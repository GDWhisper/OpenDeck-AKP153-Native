# AKP153 原生识别 — 进度与交接文档

> 最后更新：2026-07-25
> 状态：**M0–M5 已完成**。代码通过 clippy + fmt，待真机冒烟测试。

---

## 0. 任务目标

把 AJAZZ AKP153（VID `0x5548` / PID `0x6674`）的设备发现、驱动、图像、输入、错误恢复做进 OpenDeck 原生 core，
**取代** `st.lynx.plugins.opendeck-akp153` 插件进程，从根本上消除"克隆设备 HID 卡死 → 插件不稳 → 宿主无恢复 → 永久冻结"的链路。

动机（完整根因分析见 fork 仓库 `docs/akp153-device-freeze-analysis.md`）：
- 克隆设备会间歇性抛 `0x8007001F ERROR_GEN_FAILURE` 把设备 wedge。
- 在 `disableelgato: true` 下，设备完全由插件进程持有 HID 句柄；插件能注销+重扫，但**无法强制重枚举**（只有 OS 级 reset 能解开 wedge），且宿主**无运行时看门狗/存活检测**，唯一插件重启逻辑只在系统唤醒时触发。
- 结论：**宿主插件系统机制缺运行时韧性**是主因；原生化让宿主收回 HID 句柄生命周期，可直接重连/重枚举，治愈 wedge。

---

## 1. 已完成的步骤

### 1.1 退路（备份分支）— 已完成
| 仓库 | 备份分支 | 指向 | 远程 |
|---|---|---|---|
| `OpenDeck`（fork，含 sleep/wake 合并） | `backup/pre-native-akp153` | `276dd2e` | ✅ 已推 origin (GDWhisper/OpenDeck) |
| `opendeck-akp153` 插件 | `backup/pre-native-akp153` | `dc3fb1b` | ✅ 已推 origin（远程已改名 `opendeck-akp153-Win`，推送成功） |
| `streamdeck-onairclock` 时钟插件 | `backup/pre-native-akp153` | `3c2a7e1` | ⚠️ 仅本地（origin 是他人仓库 `wortkrieg/streamdeck-onairclock`，未推） |

回退方式：`git checkout backup/pre-native-akp153`（fork 与插件仓库）。

### 1.2 干净基线 — 已完成
- 现有 `OpenDeck-dev` 仓库已配置 `upstream = nekename/OpenDeck` 且 fetch 过，用 worktree 方式秒级拉出干净上游：
- 新 worktree：`G:\Codes\opendeck\OpenDeck-clean`
- 分支：`feat/native-akp153`（跟踪 `upstream/main`，起点 `8db648f`）
- 该目录是**独立工作树、独立分支**，不污染 fork 的 `main`/`dev`。

### 1.3 调研 — 已完成
- 读插件 `mappings.rs` / `inputs.rs`：提取 AKP153 全部设备常量、图像格式、按键置换表（见设计文档 §1）。
- 读上游 `elgato.rs` / `shared.rs`：确认上游用两个独立 map（`ELGATO_DEVICES` 真实句柄 + `shared::DEVICES` 逻辑注册表），`init()` 是"每设备任务"模板，`initialise_devices()` 受 `disableelgato` 控制；**干净上游无 `reinitialise_devices`/`invalidate_hidapi`**（那是我们 fork sleep/wake 加的，需移植）。

### 1.4 设计文档 — 已完成
- `G:\Codes\opendeck\OpenDeck-clean\docs\native-akp153-design.md`
  - §1 设备常量清单（VID/PID/布局/图像格式/按键置换）
  - §2–§4 backend 抽象方案：**选定方案 A（并行 `mirajazz.rs` 模块 + 按 ID 前缀 `"99"` 薄分发）**，理由：爆炸半径最小、回退最容易
  - §5 集成点改动清单（Cargo.toml / main.rs / update_image 分发 / 唤醒恢复 / DEVICE_NAMESPACES / disableelgato 语义）
  - §6 错误恢复 / wedge 治愈（核心收益：宿主直接重连/重枚举）
  - §7 与插件的关系（原生取代插件，勿同时加载）
  - §8 实施里程碑 M0–M5
  - §9 风险与回退
  - §10 待决问题（3 项）

### 1.5 实现代码 — M0–M5 已完成
- `Cargo.toml`：添加 `mirajazz = "0.15.1"`（crates.io，与插件一致）。
- `src-tauri/src/mirajazz.rs`：完整模块——
  - 设备常量、按键置换表、图像格式
  - `MIRAJAZZ_DEVICES` 静态 map
  - `initialise_mirajazz_devices()`：命名空间注册 + `list_devices` + `Device::connect` + `spawn(init)`
  - `setup_device()`：清屏/亮度/flush 统一初始化，返回 reader
  - `init()`：setup_device (带 timeout) → `register_device` → 事件循环（`reader.read` → `key_down`/`key_up`）→ 错误时 reconnect → 放弃后 `deregister`
  - `try_reconnect()`：shutdown 旧设备 → 最多 3 次 × (2s delay + enumerate + connect + setup)，超时 8s
  - `process_input()`：原始 HID → `device_to_opendeck` 置换 → `DeviceInput::ButtonStateChange`
  - `update_image()` / `clear_screen()` / `set_brightness()` / `reset_devices()`
- `src-tauri/src/main.rs`：`mod mirajazz;`，10s 循环并列调用，退出时 `reset_devices`
- `src-tauri/src/events/outbound/devices.rs`：`update_image`/`clear_screen`/`set_device_brightness` 按 `"99-"` 前缀分发到 `mirajazz` 模块
- **验证**：`cargo clippy -- -D warnings` 零错误零警告 + `cargo fmt -- --check` 干净通过。

---

## 2. 关键结论与决策

1. **看门狗是轮询机制**：`0001-plugin-watchdog.patch` 是 30s 一轮 `try_wait()` 轮询子进程，只认"进程退出"，对"进程活着但设备/插件已 wedged"无效 → 不应作为根治手段，宿主恢复应走**事件驱动（WS 断开即重启）+ 设备心跳超时**。
2. **原生化是最对症的根治**：拿到宿主级 HID 重枚举能力（插件在 `disableelgato` 下没有），且消除进程边界整类失效。
3. **backend 抽象选方案 A**：先并行模块 + 分发，最小侵入；验证可靠后再考虑方案 B（trait 统一）。
4. **唤醒/重枚举逻辑需从 fork 移植**：干净上游没有，原生 AKP153 若需睡眠恢复必须自带。
5. **待决问题已拍板**（2026-07-25）：
   - `mirajazz` 依赖来源 → **crates.io `0.15.1`**
   - v1 热插拔 → **仅启动+唤醒（复用上游 10s 循环）**
   - sleep/wake 移植 → **暂不移植，M4 按需处理**

---

## 3. 待决问题（已拍板 ✅）

1. ✅ `mirajazz` 依赖来源 → **crates.io `"0.15.1"`**（与插件一致）
2. ✅ v1 热插拔 → **仅启动+唤醒**（复用上游 `main.rs` 10s 循环调 `initialise_mirajazz_devices`）
3. ✅ sleep/wake 移植 → **暂不移植**（上游已有亮度归零唤醒；HID invalidate 留 M4）

---

## 4. 已完成：M4 恢复 + M5 收尾

### M4 恢复 ✅
- `try_reconnect()`：reader fatal error 后，`shutdown()` 释放旧 HID 句柄，最多重试 3 次（每次 2s delay + enumerate + connect with 8s timeout + setup_device）。
- 非致命错误（`ImageError` / `BadData`）直接 continue，不触发重连。
- 重连成功 → 拿到新 reader，继续 event loop；失败 → deregister，任务结束（10s polling 兜底）。

### M5 收尾 ✅
- `cargo clippy -- -D warnings`：零警告。
- `cargo fmt -- --check`：零差异。
- **防御性优化**：所有 HID 操作（`setup_device` / reconnect）包裹 `AssertUnwindSafe(...).catch_unwind()`，防御 `async-hid` 在 wedged 设备上 panic 崩溃进程。Panic 视为 fatal error，日志 `"panicked (device likely wedged)"`，安全退出或重试。
- 已通过真机验证：插件移除后成功识别设备。
- 待与 `backup/pre-native-akp153` 行为对比确认功能平替。

---

## 5. 关键路径速查

- 干净基线（实现场所）：`G:\Codes\opendeck\OpenDeck-clean`（`feat/native-akp153`）
- 设计文档：`OpenDeck-clean/docs/native-akp153-design.md`
- 本进度文档：`OpenDeck-clean/docs/native-akp153-progress.md`
- 设备规格来源（参考）：`G:\Codes\opendeck\opendeck-akp153\src\mappings.rs`、`inputs.rs`、`device.rs`、`watcher.rs`
- 上游结构参考：`OpenDeck-clean/src-tauri/src/elgato.rs`、`shared.rs`、`events/inbound/devices.rs`
- 退路分支：`backup/pre-native-akp153`（fork + akp153 插件已推 origin；时钟插件仅本地）
- 历史根因分析（fork）：`OpenDeck-dev/docs/akp153-device-freeze-analysis.md`

---

## 6. 长稳审计与修复（2026-09-18，`5d5bf0f`）

**可复用结论：长期不关机的风险不在 RAM，在「任务静默死亡」和「全局锁跨网络 I/O」。**
本机 54 天日志 + 50.8h 连续运行实测：私有内存 20.1MB、190 次休眠恢复全部 2-3s 内自行恢复。所以判断长稳问题时，先查状态表是否残留僵尸条目、锁是否被 hold 住，再看内存曲线。

**诊断错误（记此避免重犯）**：看到日志 `all reconnect attempts exhausted` 就断定 `MIRAJAZZ_DEVICES` 留下永久僵尸条目。实际 9s 后就恢复了 —— `try_reconnect()` 在 `mirajazz.rs:218` 开头就 `remove` 了条目，10s polling 循环会重新接管。结论：读日志必须看完整时间线，不能盯单行报错下判断。

**根因与修复位置**：
- 唤醒竞态：`resume_from_sleep()` 清 `DEVICE_GENERATIONS` 会把重连已重建的新一代一起删掉 → `DEVICE_GENERATIONS.remove_if(gen 相等才删)`；`ACTIVE_INIT_TASKS` 改由 `ActiveInitGuard` 的 `Drop` 释放，init  panic/提前 return 也不会永久卡住设备
- 注册失败僵尸：`elgato.rs` 里 `product().unwrap()` / `register_device().unwrap()` 直接抛掉整个 device task，设备条目留在 map 里永不重试 → 改为 `log::error!` + `remove` + `return`，交给 10s 轮询
- socket 串台：读者任务退出时按 uuid 无条件 `remove`，可能删掉同 uuid 的更新连接 → `PLUGIN_SOCKETS`/`PROPERTY_INSPECTOR_SOCKETS` 值加 `CONNECTION_IDS` 身份标签，删除前比对；队列改为「先插入 socket 再 drain」，否则并发 `send_to_plugin` 会把消息重新排到被回放的历史消息之后
- 无界队列：`PLUGIN_QUEUES`/`PROPERTY_INSPECTOR_QUEUES` 在插件永不连接时只增不减（唯一真实的内存增长源）→ `MAX_QUEUED_MESSAGES = 64`，超限丢最旧
- 恶意/异常对端：插件 WS 握手与首次注册无超时、`accept` 持续失败时 `while let Ok` 热转 → `HANDSHAKE_TIMEOUT = 10s` + `Err` 分支 1s 退避 + peer 触发的 `unwrap()` 全改早返回；静态服务器 URL 解码失败/`url[1..]` 切片 panic → 400
- 锁跨 I/O：`Store::save()` 在持有 `PROFILE_STORES` 写锁期间做同步文件锁写盘 → 新增 `save_async()`（`spawn_blocking`），profile 保存路径全部改用；`setFeedback`/`setFeedbackLayout` 补进上下文鉴权白名单
- 文件后缀污染：列 profile / 删除 / 改名未过滤 `.json.bak`、`.json.temp`；`simplified_profile.rs` 用固定 5 字符截断处理 `.json.bak` 会截错 → `strip_storage_suffix()` 按最长后缀优先
- 前端监听器泄漏：`listen()` 的 Promise 与 `onDestroy` 竞态导致 unlisten 丢失 → `keepUnlisten()` + `destroyed` 标志（Key / PropertyInspectorView / ProfileManager）；`rendererHelper` 图片加载无超时、overlay 异常跳过 `context.restore()` → 10s 超时 + try/catch；`PluginDetails` 改单例 `marked.Renderer` 污染全局 → 本地 `new Marked()`
- 未采纳：`power_events.rs` 里 Resume 时额外 `wake_from_computer_lock()`（会在锁屏点亮面板 + 对刚恢复的死句柄做 HID 写，把设备标成 wedged）；插件 sink `send()` 超时（需重构 per-socket sink，半帧损坏风险）；`get_device_profiles` 缓存（陈旧缓存会选错 profile）；profile JSON 里的 base64 图像

**排查提示**：
```
grep -c "bumping generation\|stale init\|marking device as wedged" <log>/OpenDeck.log
```
`stale init` 计数远大于休眠恢复次数，说明还有没堵住的代际路径。内存回归看进程私有工作集，不看 RSS 峰值。

## 7. 仓库状态变更（2026-09-18）

`G:\Codes\opendeck\OpenDeck`（旧 OpenDeck-Win 备份 worktree）的 `.git` 对象库已损毁，只剩一个空的 `worktrees/OpenDeck-clean/hooks` 壳，因此 `OpenDeck-clean/.git` 原先的 worktree 指针是死链。本地历史已无法从该目录找回。

现 `OpenDeck-clean` 是**独立仓库**（不再是 worktree），历史从云端权威线 `GDWhisper/OpenDeck-AKP153-Native` 重新克隆恢复：`main` 与 `feat/native-akp153` 同步，`backup/pre-native-akp153` 保留为退路。旧备份目录不再参与开发、合并与发布。

---

## 8. 退出 panic：tao 的 `Destroyed` 状态机（2026-10-10 诊断与修复）

**可复用结论：日志里的 panic 行不等于「运行中自发崩溃」。先把日志时间戳换算成本地时间（OpenDeck 日志是 UTC），拿它跟 System 日志对齐 —— `Get-WinEvent @{LogName="System"; Id=1074}`（用户主动关机/注销）和 `Microsoft-Windows-Kernel-Power` Id 42（进入睡眠）—— 再看 panic 是不是该次运行的最后一行。两条都对上，就是退出瞬间炸，与硬件无关。**

**诊断错误（记此避免重犯）**：一开始把它当作 AKP153 冻结的残余问题去查 HID 时间线，实际 24 次 panic 全在退出路径上，最后一次 HID 报错在 10-04。另外「Windows 没有崩溃报告」不能证明没崩：这里 panic 被 tao 的 `catch_unwind` 捕获后 `resume_unwind`，进程以 unwind 方式结束，WER 不记录 —— 45 天的 Application 日志只剩 2 条 MsiInstaller，差点被当成「没崩过」的证据。

**根因**：
- `tao-0.34.5/src/platform_impl/windows/event_loop/runner.rs:368` — `(Destroyed, _) => panic!("cannot move state from Destroyed")`
- 进入 `Destroyed` 只有两条路：`event_loop.rs:2416`（窗口过程收到 **WM_ENDSESSION** → `loop_destroyed()`，关机/注销/Restart Manager 都会发）和 `event_loop.rs:285`（`app.exit()` 后 `run()` 收尾）
- 之后只要还有一个消息抵达 tao 的隐藏事件目标窗口（`PROCESS_NEW_EVENTS_MSG_ID` → `runner.poll()`、`USER_EVENT_MSG_ID`、`EXEC_MSG_ID`）就 panic。panic 被 `catch_unwind` 存进 `panic_error`，回到消息循环后 `take_panic_error()` → `reset_runner()` + `resume_unwind`（`event_loop.rs:271-274`）→ 主线程展开 → 进程退出
- 我们自己放大了它：`RunEvent::Exit` 触发时状态**已经是 Destroyed**，而闭包里 `block_on(plugins::deactivate_plugins())` 把主线程钉住数秒，期间 `application_watcher.rs:30` 的 250ms 无限循环照常 `emit` 到 webview（往主线程投递消息）。`block_on` 一返回，攒下的消息被 pump → 命中 panic

**证据**（`%LOCALAPPDATA%\opendeck\logs\OpenDeck.log`）：50 次启动 / 24 次 panic；11 次与 Id 1074 相差 ≤6 秒，含 2026-10-10 00:10:46 同秒；2026-10-09 20:43:15 那次在 Kernel-Power 42 前 8 秒；每次 panic 都是该运行的最后一行，下一行直接是新的 `Using ports`。

**顺带查出的部署滞后**：`D:\OpenDeck\opendeck.exe` 的时间戳是 2026-07-26 16:17，而 `src-tauri/target/release/` 里有 2026-09-18 22:52 的 2.13.1 产物和对应 NSIS 包 —— §6 那批长稳修复**从未安装到本机**。所以 10 月的 panic 来自 `5d5bf0f` 之前的二进制，既不能证明那批修复无效，也不能证明有效；本次部署会把它和 §8 一起带上。

**修复位置**：
- `src-tauri/src/main.rs`：新增 `SHUTTING_DOWN: AtomicBool`，`RunEvent::Exit` 开头置位；退出顺序改为 deactivate plugins → `flush_debounced_saves()` → 限时 2 秒 await `elgato::reset_devices()` / `mirajazz::reset_devices()`（原来是 `tokio::spawn` 发了不管，硬退出会把设备复位一起砍掉，所以必须等）→ `flush_events_blocking()` → Windows 下 `std::process::exit(0)`，不再把控制权交回 tao 的消息泵
- `src-tauri/src/store/profiles.rs`：`flush_debounced_saves()` — abort 掉 `PROFILE_SAVE_DEBOUNCE` 里未到期的 2 秒 debounce 任务并立即落盘。修的是真实数据丢失：panic → unwind → runtime drop 取消未到期任务，关机前 2 秒内改的按键配置会没；日志里 17 条 `Failed to save profile for device ...: device not found` 也来自这条 debounce 在设备消失后才触发
- `src-tauri/src/application_watcher.rs`：250ms 循环遇 `SHUTTING_DOWN` break；两处 `get_webview_window("main").unwrap()` 改成 `if let Some(..)` / `let Some(..) else { continue }`

**未处理**：`events/inbound/misc.rs:35,41,53,65`、`events/frontend/mod.rs:63`、`events/frontend/instances.rs:205,270`、`events/frontend/profiles.rs:117`、`main.rs:174,194,288` 仍有 `get_webview_window("main").unwrap()`。它们只在窗口确实存在时被调用（命令/插件事件驱动），退出路径已被硬退出封住，暂不扩大改动面。

**排查提示**：
```
grep -c "cannot move state from Destroyed" OpenDeck.log     # 修复后的版本应为 0
grep -n "panicked at" OpenDeck.log | tail                   # 若仍出现，检查它是不是该次运行的最后一行
```

---

## 9. 待办 B — 升级到 tauri 2.12 取上游正解

上游在 `tao-0.37.1/src/platform_impl/windows/event_loop.rs:2336-2343` 的 WM_ENDSESSION 分支里，`loop_destroyed()` 之后直接 `std::process::exit(0)`，注释说明原因与我们 §8 同源（Restart Manager 只发消息、不会替我们关闭，返回后消息继续泵）。

版本对应（本机 `K:\.cargo\registry` 已有 .crate，可离线比对）：

| tauri | tao | wry | 是否修复 |
|---|---|---|---|
| 2.10.2（当前） | 0.34.5 | 0.54.1 | 否 |
| 2.11.5 | 0.35.0 | 0.55.0 | **否**，WM_ENDSESSION 仍只 `loop_destroyed()`，`runner.rs:371` panic 分支原样 |
| 2.12.1 | 0.37.0 | 0.57.0 | 是 |

**必须一步跨到 2.12，2.11 是无效中间态。**

触发时机（满足任一即启动，不要无限期搁置）：
1. §8 上线后日志里 `cannot move state from Destroyed` 再次出现 —— 说明还有未被覆盖的 Destroyed 后投递路径（Restart Manager、wry 侧自建窗口等），只能靠上游在源头收口。
2. 硬退出暴露副作用：Windows 报「应用程序阻止关机」（cleanup 超出 ENDSESSION 时限）、插件未按 `deactivate_plugins` 正常收尾、或退出后设备残留图像/亮度。
3. 需要 tao 0.37 / wry 0.57 修的其他 Windows 缺陷（webview 崩溃、DPI、devtools），或有安全公告要求跟进 tauri 版本。
4. 常规节奏：§8 版本连续 3–4 周长稳（对齐 §6 的长稳审计基线，无 panic、无 `marking device as wedged`）后，作为**单独一批**提交，不与硬件/HID 修复混在同一 PR。

迁移回归面（做 B 时逐项验证，不能只过编译）：插件 WebSocket 握手/断开重连与 `PLUGIN_QUEUES` 回放、property inspector iframe 连接、托盘显示隐藏与菜单、single-instance + deep-link、autostart `--hide`、`--restart-helper` 重启链路、AKP153 唤醒恢复（`bumping generation` → 4 步 setup → `entering event loop`）、`deno task tauri build` 产物能否正常安装运行。
