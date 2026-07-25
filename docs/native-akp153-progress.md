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
