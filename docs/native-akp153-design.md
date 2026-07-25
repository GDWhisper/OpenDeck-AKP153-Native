# AKP153 原生识别 — 设计文档

> 分支：`feat/native-akp153`（基于 `upstream/main` @ `8db648f`）
> 目标：把 AJAZZ AKP153（VID `0x5548` / PID `0x6674`）的设备发现、驱动、图像、输入、错误恢复做进 OpenDeck 原生 core，
> 不再依赖 `st.lynx.plugins.opendeck-akp153` 插件进程。根因见 `docs/akp153-device-freeze-analysis.md`：克隆设备的 HID 会间歇性
> `0x8007001F` 卡死，而插件进程在 `disableelgato` 下既无力强制重枚举、宿主又无运行时存活/重启机制，故冻结不可恢复。
> 原生化后，HID 句柄生命周期收归宿主，可直接重连/重枚举，从根本上治愈 wedge。

---

## 1. 待复现的设备能力（来自插件 `opendeck-akp153`，作为规格）

### 1.1 设备常量（`mappings.rs`）
| 项 | 值 |
|---|---|
| `DEVICE_NAMESPACE` | `"99"`（设备 ID 前缀；`DEVICE_NAMESPACES` 鉴权用前 2 字符） |
| VID / PID | `0x5548`（MIRABOX）/ `0x6674`（AKP153_PID） |
| 查询 | `DeviceQuery::new(65440, 1, 0x5548, 0x6674)`（usage_page=65440, usage=1） |
| 布局 | `ROW_COUNT=3, COL_COUNT=6, KEY_COUNT=18, ENCODER_COUNT=0`（3×6，无编码器） |
| `protocol_version` | `1`（v1 协议） |
| 设备 ID | `99-<serial>-153`（`id_suffix()="153"`） |
| `human_name` | `"Ajazz AKP153"` |

### 1.2 图像格式（`get_image_format_for_key`，v1）
- 模式：`JPEG`
- 尺寸：键 `5 / 11 / 17` → `(82, 82)`，其余 → `(95, 95)`
- 旋转：`Rot90`；镜像：`Both`
- 图像来源：OpenDeck 以 data-URL（jpeg）形式下发，`handle_set_image` 解码为 `DynamicImage` 后 `set_button_image(pos, format, image)`（mirajazz 内部做旋转/镜像）

### 1.3 按键置换（`inputs.rs`，必须逐键移植）
- `opendeck_to_device(key)`：`[12,9,6,3,0,15,13,10,7,4,1,16,14,11,8,5,2,17][key]`
- `device_to_opendeck(key)`：`[4,10,16,3,9,15,2,8,14,1,7,13,0,6,12,5,11,17][key-1]`
- `process_input` 把设备原始输入转成 `DeviceInput::ButtonStateChange`

### 1.4 输入事件（`device_events_task`）
- `reader.read()` → `DeviceStateUpdate::{ButtonDown, ButtonUp}` → 出站 `key_down/up`
- AKP153 无编码器，故无 `Encoder*` 事件

### 1.5 恢复行为（迁移为原生恢复）
- 插件：`handle_error` → `deregister_device` + `NEEDS_RESCAN`（进程内重扫）。**原生版改为：HID 读错即重连句柄 / 触发宿主级重枚举**（见 §6）。

---

## 2. 上游 `elgato.rs` 结构（可复用模式）

- `ELGATO_DEVICES: RwLock<HashMap<String, AsyncStreamDeck>>` — 真实 HID 句柄，按设备 ID 索引。
- `shared::DEVICES: DashMap<String, DeviceInfo>` — 逻辑设备注册表，由 `inbound::devices::register_device` 写入。
- `init(device, id)`（elgato.rs:187）：每设备任务模板——清屏 → `register_device` → `reader.read(100.0)` 循环映射 `DeviceStateUpdate` → 出站 `key_down/up`/`encoder_*` → 读错 `Err` 则 `remove` + `deregister_device`。
- `initialise_devices()`（elgato.rs:292）：检查 `disableelgato`；为 false 时 `list_devices_async` 后对每个设备 `tokio::spawn(init(...))`。
- `update_image` / `clear_screen` / `set_brightness` / `reset_devices`：直接操作 `ELGATO_DEVICES`，使用 `elgato_streamdeck` 自家图像 API（与 mirajazz 不兼容）。
- **干净上游无 `reinitialise_devices` / `invalidate_hidapi`** —— 唤醒/重枚举逻辑需从我们 fork 的 sleep/wake 分支移植进来（见 §6）。

---

## 3. Backend 抽象方案（二选一）

### 方案 A（推荐，先落地）：并行模块 + 薄分发层
- 新增 `mirajazz.rs`，**镜像 `elgato.rs` 结构**：自有 `MIRAJAZZ_DEVICES: RwLock<HashMap<String, mirajazz::Device>>`、自有 `init`/`update_image`/`clear_screen`/`set_brightness`/`reset`/`initialise_mirajazz_devices`。
- 在既有调用点按设备 ID 前缀分发：`"99"` → mirajazz 后端；`"sd"`（或其它）→ elgato 后端。
- **优点**：不改动 `elgato.rs` 类型与上游核心，爆炸半径最小，回退最容易（删 `mirajazz.rs` + 分发即可）。
- **代价**：`update_image` 等调用点需各加一处分发（按 id 前缀），有少量重复。

### 方案 B（长期干净，但侵入大）：`DeviceBackend` trait
- 定义 trait `{ update_image, clear_screen, set_brightness, reset, register }`，`elgato.rs` 与 `mirajazz.rs` 各自实现；用 `BACKENDS: DashMap<String, Box<dyn DeviceBackend>>` 统一注册表。
- `update_image(ctx, img)` 走 trait 方法，调用点无需关心后端。
- **优点**：单一入口、无分发散落、易扩展其它克隆设备。
- **代价**：需改造上游 `ELGATO_DEVICES` 的值类型与 `update_image` 签名 → 上游分歧大、回退难。

**结论**：v1 用**方案 A** 尽快拿到稳定原生 AKP153；待验证可靠后，再视情况重构为方案 B。

---

## 4. `mirajazz.rs` 模块设计（方案 A）

```
pub static MIRAJAZZ_DEVICES: LazyLock<RwLock<HashMap<String, mirajazz::Device>>> = ...;

// 移植自插件 mappings.rs / inputs.rs
const AKP153_VID: u16 = 0x5548;
const AKP153_PID: u16 = 0x6674;
fn get_image_format_for_key(key: u8) -> ImageFormat { ... }   // v1: JPEG, (95,95)/(82,82), Rot90, Both
fn opendeck_to_device(key: u8) -> u8 { ... }                   // 置换表
fn device_to_opendeck(key: u8) -> u8 { ... }

// 每设备任务（对齐 elgato::init）
async fn init(device: mirajazz::Device, id: String) {
    clear_all_button_images(); set_brightness(); flush();
    MIRAJAZZ_DEVICES.write().insert(id, device);
    register_device("", DeviceInfo { id, plugin:"", name:"Ajazz AKP153", rows:3, columns:6, encoders:0, ... });
    let reader = device.get_reader(process_input);
    loop {
        match reader.read(None).await {
            Ok(updates) => for u in updates {
                // 注意：设备键索引必须经 device_to_opendeck 置换后再出站
                ButtonDown(k) => key_down(press(device_to_opendeck(k))),
                ButtonUp(k)   => key_up(press(device_to_opendeck(k))),
            },
            Err(_) => break,   // 见 §6 恢复
        }
    }
    MIRAJAZZ_DEVICES.write().remove(&id);
    deregister_device("", id);
}

pub async fn initialise_mirajazz_devices() {
    // 在 DEVICE_NAMESPACES 登记 "99" 命名空间（register_device 鉴权需要）
    DEVICE_NAMESPACES.write().insert("99", "opendeck_native_akp153");
    for dev in mirajazz::list_devices(&[AKP153_QUERY]).await {
        let id = format!("99-{}-153", dev.serial_number);
        if MIRAJAZZ_DEVICES.read().contains_key(&id) { continue; }
        match mirajazz::Device::connect(&dev, 1, 18, 0).await {
            Ok(d) => tokio::spawn(init(d, id)),
            Err(e) => log::warn!("AKP153 connect failed: {e}"),
        }
    }
}

// 图像管线：对齐 elgato::update_image，但走 mirajazz 图像 API
pub async fn update_image(ctx, image: Option<&str>) {
    if let Some(d) = MIRAJAZZ_DEVICES.read().get(&ctx.device) {
        if let Some(img) = image {
            let bytes = decode_data_url(img);          // jpeg base64
            let dyn = image::load_from_memory(&bytes)?;
            d.set_button_image(opendeck_to_device(ctx.position), get_image_format_for_key(ctx.position), dyn).await?;
        } else {
            d.clear_button_image(opendeck_to_device(ctx.position)).await?;
        }
        d.flush().await?;
    }
}
pub async fn clear_screen(id) / set_brightness(id, b) / reset() { ... }  // 同上风格
```

---

## 5. 集成点（改动清单）

1. **`Cargo.toml`**：增加 `mirajazz` 依赖（git 或 path，与插件一致）。
2. **`main.rs`**：在调用 `elgato::initialise_devices()` 处，并列调用 `mirajazz::initialise_mirajazz_devices()`。
3. **`elgato.rs::update_image` / `clear_screen` / `set_brightness` / `reset_devices`**：函数体开头按 `context.device` / `id` 前缀 `"99"` 分发到 `mirajazz` 对应函数；非 `"99"` 走原 elgato 逻辑。
4. **唤醒恢复（移植自 fork sleep/wake）**：新增 `reinitialise_devices()`，内部依次重初始化 elgato 与 mirajazz 两类设备的 HID 句柄；`power_events.rs` 唤醒时调用之（替代 fork 里只重 elgato 的版本）。
5. **`DEVICE_NAMESPACES`**：`initialise_mirajazz_devices` 登记 `"99"`；不再依赖 `disableelgato` 来启用 AKP153。
6. **`disableelgato` 语义**：保留其仅控制 Elgato 原生路径；AKP153 原生检测**独立于** `disableelgato` 运行（AKP153 是 mirajazz 而非 elgato）。用户实际只有 AKP153，故 `disableelgato` 可保留 `true`（禁 Elgato）而 AKP153 仍由原生 mirajazz 接管。

---

## 6. 错误恢复 / wedge 治愈（核心收益）

- **读错处理**：`init` 的 `reader.read()` 返回 `Err`（即 `0x8007001F`）时，先 `deregister`，然后**尝试重连**：`Device::connect` 重新打开 HID 句柄，成功则继续服务，失败则退出任务（设备仍在枚举则由后续重枚举兜底）。
- **宿主级重枚举**：因为句柄由宿主持有，可在 `reinitialise_devices` 中对 mirajazz 设备执行 `drop` 旧句柄 + 重新 `list_devices` + `connect` + `invalidate_hidapi`（如 mirajazz 暴露）。这是插件在 `disableelgato` 下**做不到**的能力，是根除冻结的关键。
- **绝不 `.unwrap()` 吞错**：所有 outbound / HID 调用改 `?` + 日志 + 受控退出，避免 panic 把整个 OpenDeck 进程带崩（插件 `device.rs` 的 `.unwrap()` 是历史隐患）。

---

## 7. 与 akp153 插件的关系

- 原生识别**取代**该插件：启用原生后，**不要再安装/加载** `st.lynx.plugins.opendeck-akp153`，否则同一设备 `99-...-153` 会被注册两次。
- 插件仓库保留为参考与备份分支（`backup/pre-native-akp153`），必要时回退。
- 时钟等**其它插件**（onairclock）仍走插件机制，不受影响；它们的宿主侧存活/重启缺口（无运行时看门狗）仍建议另立任务修复。

---

## 8. 实施里程碑（不含本次文档）

1. **M0 依赖与骨架**：加 `mirajazz` 依赖；建 `mirajazz.rs` 空模块 + `initialise_mirajazz_devices` 仅做 `DEVICE_NAMESPACES` 登记与 `list_devices` 日志（不接 HID）。编译通过、不影响现有行为。
2. **M1 发现与注册**：实现 `connect` + `init` + `register_device`，在 `main.rs` 调用。验证设备出现在 OpenDeck（按键无响应、无图像也可接受，先确认注册）。
3. **M2 输入**：`reader` 循环 + `device_to_opendeck` 置换 → 出站 `key_down/up`。实测按键。
4. **M3 图像**：`update_image` 分发 + `set_button_image` + `opendeck_to_device` 置换 + `get_image_format_for_key`。实测显示。
5. **M4 恢复**：读错重连 + 移植 `reinitialise_devices` 唤醒重枚举。复现 `0x8007001F` 验证无冻结恢复。
6. **M5 收尾**：亮度/清屏/重置；`cargo clippy` + `fmt` + 真机冒烟；与 `backup/pre-native-akp153` 行为对比。

---

## 9. 风险与回退

- **上游分歧**：方案 A 把改动局限在 `mirajazz.rs` + 少量分发点，回退 = 删除该模块与分发；`feat/native-akp153` 分支可整体丢弃。
- **`mirajazz` API 差异**：以插件现有用法为参考，若 crate 版本不一致需对齐。
- **进程隔离丧失**：原生 bug 可能拖垮 OpenDeck；以 §6 的受控错误处理约束。
- **图像像素偏差**：旋转/镜像/尺寸须逐键核对（目标与插件完全一致）。

## 10. 待决问题（实现前拍板）

1. `mirajazz` 依赖来源（crates.io 版本 / git / 本地 path）？
2. v1 是否需热插拔（运行时周期重扫）？还是仅启动 + 唤醒（与 elgato 一致）？
3. 是否顺带把 fork 的 sleep/wake 特性也移植过来（原生 AKP153 若需睡眠恢复则必须）？
