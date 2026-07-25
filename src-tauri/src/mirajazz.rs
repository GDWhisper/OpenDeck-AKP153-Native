use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use dashmap::DashSet;
use futures::FutureExt;
use mirajazz::device::{Device, DeviceQuery, list_devices};
use mirajazz::error::MirajazzError;
use mirajazz::state::{DeviceStateReader, DeviceStateUpdate};
use mirajazz::types::{DeviceInput, ImageFormat, ImageMirroring, ImageMode, ImageRotation};
use tokio::sync::RwLock;

use crate::events::inbound;
use base64::Engine as _;

const DEVICE_NAMESPACE: &str = "99";

const AKP153_VID: u16 = 0x5548;
const AKP153_PID: u16 = 0x6674;

const AKP153_QUERY: DeviceQuery = DeviceQuery::new(65440, 1, AKP153_VID, AKP153_PID);

#[allow(dead_code)]
const ROW_COUNT: u8 = 3;
#[allow(dead_code)]
const COL_COUNT: u8 = 6;
const KEY_COUNT: usize = 18;
const ENCODER_COUNT: usize = 0;

static MIRAJAZZ_DEVICES: LazyLock<RwLock<HashMap<String, Arc<Device>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

static WEDGED_DEVICES: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

static FLUSH_SIGNALS: LazyLock<DashMap<String, tokio::sync::mpsc::Sender<()>>> = LazyLock::new(DashMap::new);

/// Per-device generation counter. Incremented when a new init task takes over
/// (e.g. after resume from sleep). Old init tasks check this before cleanup
/// to avoid deregistering a device that a newer init has registered.
static DEVICE_GENERATIONS: LazyLock<DashMap<String, u64>> = LazyLock::new(DashMap::new);

const FLUSH_DEBOUNCE_MS: u64 = 200;

/// Reader read timeout. Must be finite so the event loop can
/// detect wedge/resume signals instead of blocking forever.
const READER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const OPENDECK_TO_DEVICE: [u8; 18] = [12, 9, 6, 3, 0, 15, 13, 10, 7, 4, 1, 16, 14, 11, 8, 5, 2, 17];

const DEVICE_TO_OPENDECK: [u8; 18] = [4, 10, 16, 3, 9, 15, 2, 8, 14, 1, 7, 13, 0, 6, 12, 5, 11, 17];

#[allow(dead_code)]
pub fn opendeck_to_device(key: u8) -> u8 {
	if (key as usize) < OPENDECK_TO_DEVICE.len() { OPENDECK_TO_DEVICE[key as usize] } else { key }
}

fn device_to_opendeck(key: usize) -> usize {
	let idx = key.saturating_sub(1);
	if idx < DEVICE_TO_OPENDECK.len() { DEVICE_TO_OPENDECK[idx] as usize } else { key }
}

#[allow(dead_code)]
pub fn get_image_format_for_key(_key: u8) -> ImageFormat {
	ImageFormat {
		mode: ImageMode::JPEG,
		size: (85, 85),
		rotation: ImageRotation::Rot90,
		mirror: ImageMirroring::Both,
	}
}

fn process_input(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
	if input as usize > KEY_COUNT {
		return Err(MirajazzError::BadData);
	}

	let mut button_states = vec![false; KEY_COUNT];

	if input == 0 {
		return Ok(DeviceInput::ButtonStateChange(button_states));
	}

	let opendeck_idx = device_to_opendeck(input as usize);
	if opendeck_idx < KEY_COUNT {
		button_states[opendeck_idx] = state != 0;
	}

	Ok(DeviceInput::ButtonStateChange(button_states))
}

const MAX_RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

async fn setup_device(device: &Device, _device_id: &str) -> Result<Arc<DeviceStateReader>, MirajazzError> {
	device.clear_all_button_images().await?;
	let brightness = crate::store::get_settings().value.brightness;
	device.set_brightness(brightness).await?;
	device.flush().await?;
	Ok(device.get_reader(process_input))
}

fn handle_wedge(device_id: &str) {
	log::error!("mirajazz [{device_id}]: marking device as wedged, will be reconnected by polling loop");
	WEDGED_DEVICES.insert(device_id.to_owned());
	cleanup_device_state(device_id);
}

fn cleanup_device_state(device_id: &str) {
	FLUSH_SIGNALS.remove(device_id);
}

/// Bump the generation counter for a device. Returns the new generation.
fn bump_generation(device_id: &str) -> u64 {
	let mut entry = DEVICE_GENERATIONS.entry(device_id.to_owned()).or_insert(0);
	*entry.value_mut() += 1;
	*entry.value()
}

/// Check if the current init task is still the active one for this device.
fn is_current_generation(device_id: &str, generation: u64) -> bool {
	DEVICE_GENERATIONS.get(device_id).map(|g| *g.value() == generation).unwrap_or(false)
}

async fn safe_hid_op<T>(device_id: &str, op: impl Future<Output = Result<T, MirajazzError>>) -> Option<T> {
	if WEDGED_DEVICES.contains(device_id) {
		return None;
	}

	match AssertUnwindSafe(op).catch_unwind().await {
		Ok(Ok(result)) => Some(result),
		Ok(Err(e)) => {
			log::warn!("mirajazz [{device_id}]: HID error: {e}");
			None
		}
		Err(_) => {
			if WEDGED_DEVICES.contains(device_id) {
				return None;
			}
			handle_wedge(device_id);
			if let Some(device) = MIRAJAZZ_DEVICES.write().await.remove(device_id) {
				drop(device);
			}
			None
		}
	}
}

fn signal_flush(device_id: &str) {
	if let Some(tx) = FLUSH_SIGNALS.get(device_id) {
		let _ = tx.try_send(());
	}
}

async fn flush_task(device: Arc<Device>, device_id: String, mut rx: tokio::sync::mpsc::Receiver<()>) {
	loop {
		if rx.recv().await.is_none() {
			break;
		}

		tokio::time::sleep(std::time::Duration::from_millis(FLUSH_DEBOUNCE_MS)).await;
		while rx.try_recv().is_ok() {}

		if WEDGED_DEVICES.contains(&device_id) {
			continue;
		}

		safe_hid_op(&device_id, device.flush()).await;
	}
}

fn start_flush_task(device_id: &str, device: Arc<Device>) {
	let (tx, rx) = tokio::sync::mpsc::channel(64);
	FLUSH_SIGNALS.insert(device_id.to_owned(), tx);
	tokio::spawn(flush_task(device, device_id.to_owned(), rx));
}

async fn try_reconnect(device_id: &str, generation: u64) -> Option<Arc<DeviceStateReader>> {
	if let Some(old_device) = MIRAJAZZ_DEVICES.write().await.remove(device_id) {
		drop(old_device);
	}
	cleanup_device_state(device_id);

	for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
		if !is_current_generation(device_id, generation) {
			log::info!("mirajazz [{device_id}]: newer generation detected, aborting reconnect");
			return None;
		}

		log::info!("mirajazz [{device_id}]: reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS}");
		tokio::time::sleep(RECONNECT_DELAY).await;

		let devices = match list_devices(&[AKP153_QUERY]).await {
			Ok(d) => d,
			Err(e) => {
				log::warn!("mirajazz [{device_id}]: enumerate failed during reconnect: {e}");
				continue;
			}
		};

		let target_serial = device_id
			.strip_prefix(&format!("{DEVICE_NAMESPACE}-"))
			.and_then(|rest| rest.strip_suffix("-153"))
			.unwrap_or("355499441494");

		let dev = devices.iter().find(|d| d.serial_number.as_deref().unwrap_or("355499441494") == target_serial);

		let dev = match dev {
			Some(d) => d,
			None => {
				log::warn!("mirajazz [{device_id}]: device not found during reconnect");
				continue;
			}
		};

		let connect_result = tokio::time::timeout(
			CONNECT_TIMEOUT,
			AssertUnwindSafe(async {
				let new_device = Device::connect(dev, 1, KEY_COUNT, ENCODER_COUNT).await?;
				let reader = setup_device(&new_device, device_id).await?;
				Ok::<_, MirajazzError>((new_device, reader))
			})
			.catch_unwind(),
		)
		.await;

		match connect_result {
			Ok(Ok(Ok((new_device, reader)))) => {
				WEDGED_DEVICES.remove(device_id);
				let new_device = Arc::new(new_device);
				MIRAJAZZ_DEVICES.write().await.insert(device_id.to_owned(), new_device.clone());
				start_flush_task(device_id, new_device);
				log::info!("mirajazz [{device_id}]: reconnected successfully");
				return Some(reader);
			}
			Ok(Ok(Err(e))) => {
				log::warn!("mirajazz [{device_id}]: reconnect connect/init failed: {e}");
			}
			Ok(Err(_panic)) => {
				log::error!("mirajazz [{device_id}]: reconnect panicked (device likely wedged)");
			}
			Err(_) => {
				log::warn!("mirajazz [{device_id}]: reconnect timed out");
			}
		}
	}

	log::error!("mirajazz [{device_id}]: all reconnect attempts exhausted");
	None
}

async fn init(device: Arc<Device>, device_id: String) {
	let generation = bump_generation(&device_id);

	let reader = match tokio::time::timeout(CONNECT_TIMEOUT, AssertUnwindSafe(setup_device(&device, &device_id)).catch_unwind()).await {
		Ok(Ok(Ok(reader))) => reader,
		Ok(Ok(Err(e))) => {
			log::warn!("mirajazz [{device_id}]: initial setup failed: {e}");
			return;
		}
		Ok(Err(_panic)) => {
			log::error!("mirajazz [{device_id}]: initial setup panicked (device likely wedged)");
			handle_wedge(&device_id);
			return;
		}
		Err(_) => {
			log::warn!("mirajazz [{device_id}]: initial setup timed out");
			return;
		}
	};

	MIRAJAZZ_DEVICES.write().await.insert(device_id.clone(), device.clone());
	start_flush_task(&device_id, device);

	if let Err(e) = inbound::devices::register_device(
		"",
		inbound::PayloadEvent {
			payload: crate::shared::DeviceInfo {
				id: device_id.clone(),
				plugin: String::new(),
				name: "Ajazz AKP153".to_owned(),
				rows: 3,
				columns: 6,
				encoders: 0,
				touchpoints: 0,
				infobars: 0,
				r#type: 0,
			},
		},
	)
	.await
	{
		log::error!("mirajazz [{device_id}]: register_device failed: {e}");
		MIRAJAZZ_DEVICES.write().await.remove(&device_id);
		cleanup_device_state(&device_id);
		return;
	}

	log::info!("mirajazz [{device_id}]: device registered, entering event loop");

	let press = |position: u8| inbound::PayloadEvent {
		payload: inbound::devices::PressPayload { device: device_id.clone(), position },
	};

	let mut reader = reader;
	loop {
		if WEDGED_DEVICES.contains(&device_id) {
			log::info!("mirajazz [{device_id}]: device wedged, exiting event loop");
			break;
		}

		if !is_current_generation(&device_id, generation) {
			log::info!("mirajazz [{device_id}]: newer generation detected, exiting event loop");
			break;
		}

		let updates = match AssertUnwindSafe(reader.read(Some(READER_TIMEOUT))).catch_unwind().await {
			Ok(Ok(updates)) => updates,
			Ok(Err(e)) => {
				if matches!(e, MirajazzError::ImageError(_) | MirajazzError::BadData) {
					log::debug!("mirajazz [{device_id}]: non-critical reader error: {e}");
					continue;
				}

				log::warn!("mirajazz [{device_id}]: fatal reader error: {e}, attempting reconnect");

				match try_reconnect(&device_id, generation).await {
					Some(new_reader) => {
						reader = new_reader;
						continue;
					}
					None => break,
				}
			}
			Err(_) => {
				log::error!("mirajazz [{device_id}]: reader panicked (device likely wedged)");
				handle_wedge(&device_id);
				break;
			}
		};

		for update in updates {
			let result = match update {
				DeviceStateUpdate::ButtonDown(key) => inbound::devices::key_down(press(key)).await,
				DeviceStateUpdate::ButtonUp(key) => inbound::devices::key_up(press(key)).await,
				_ => Ok(()),
			};
			if let Err(e) = result {
				log::warn!("mirajazz [{device_id}]: event routing error: {e}");
			}
		}
	}

	if is_current_generation(&device_id, generation) {
		MIRAJAZZ_DEVICES.write().await.remove(&device_id);
		WEDGED_DEVICES.remove(&device_id);
		cleanup_device_state(&device_id);
		if let Err(e) = inbound::devices::deregister_device("", inbound::PayloadEvent { payload: device_id.clone() }).await {
			log::warn!("mirajazz [{device_id}]: deregister_device failed: {e}");
		}
	} else {
		log::info!("mirajazz [{device_id}]: stale init (gen {generation}), skipping cleanup");
	}

	log::info!("mirajazz [{device_id}]: device task ended (gen {generation})");
}

pub async fn initialise_mirajazz_devices() {
	crate::plugins::DEVICE_NAMESPACES.write().await.insert(DEVICE_NAMESPACE.to_owned(), "opendeck_native_akp153".to_owned());

	let devices = match list_devices(&[AKP153_QUERY]).await {
		Ok(devices) => devices,
		Err(error) => {
			log::warn!("mirajazz: failed to enumerate devices: {error}");
			return;
		}
	};

	if devices.is_empty() {
		log::debug!("mirajazz: no AKP153 devices found");
		return;
	}

	for dev in &devices {
		let serial = dev.serial_number.as_deref().unwrap_or("355499441494");
		let device_id = format!("{DEVICE_NAMESPACE}-{serial}-153");

		if MIRAJAZZ_DEVICES.read().await.contains_key(&device_id) {
			continue;
		}

		if WEDGED_DEVICES.contains(&device_id) {
			log::info!("mirajazz [{device_id}]: wedged device found, will retry after old handle is released");
			continue;
		}

		log::info!("mirajazz: discovered AKP153 (VID={:#06x} PID={:#06x}) → id={device_id}", dev.vendor_id, dev.product_id);

		match Device::connect(dev, 1, KEY_COUNT, ENCODER_COUNT).await {
			Ok(device) => {
				tokio::spawn(init(Arc::new(device), device_id));
			}
			Err(error) => {
				log::warn!("mirajazz: failed to connect to AKP153 ({device_id}): {error}");
			}
		}
	}
}

/// Called on system resume from sleep/hibernate.
/// After sleep, USB HID handles are invalidated and async-hid's I/O buffers
/// are in a broken state. We bump the generation counter so old init tasks
/// skip cleanup, then clear the device map so the 10s polling loop reconnects.
pub async fn resume_from_sleep() {
	let device_ids: Vec<String> = MIRAJAZZ_DEVICES.read().await.keys().cloned().collect();

	for device_id in &device_ids {
		log::info!("mirajazz [{device_id}]: bumping generation for resume from sleep");
		bump_generation(device_id);
		WEDGED_DEVICES.remove(device_id);
	}

	if !device_ids.is_empty() {
		let mut devices = MIRAJAZZ_DEVICES.write().await;
		devices.clear();
	}

	log::info!("mirajazz: all AKP153 devices cleared for reconnection after system resume");
}

/// Set or clear a button image on a mirajazz device.
/// Only writes to the device's internal image cache; the debounce flush task
/// will send the cached images to the device in a single batched flush.
pub async fn update_image(context: &crate::shared::Context, image: Option<&str>) -> Result<(), anyhow::Error> {
	let device = {
		let devices = MIRAJAZZ_DEVICES.read().await;
		devices.get(&context.device).cloned()
	};

	let Some(device) = device else { return Ok(()) };

	let decoded: Option<Vec<u8>> = match image {
		Some(img_data) => {
			let data = img_data.split_once(',').map(|(_, d)| d).unwrap_or(img_data);
			Some(base64::engine::general_purpose::STANDARD.decode(data)?)
		}
		None => None,
	};

	let result = safe_hid_op(&context.device, async {
		if let Some(bytes) = decoded {
			let dyn_image = image::load_from_memory(&bytes)?;
			let fmt = get_image_format_for_key(context.position);
			device.set_button_image(opendeck_to_device(context.position), fmt, dyn_image).await?;
		} else {
			device.clear_button_image(opendeck_to_device(context.position)).await?;
		}
		Ok::<(), MirajazzError>(())
	})
	.await;

	if result.is_some() {
		signal_flush(&context.device);
	}

	if result.is_none() && WEDGED_DEVICES.contains(&context.device) {
		return Err(anyhow::anyhow!("device wedged"));
	}
	Ok(())
}

/// Clear the entire screen of a mirajazz device.
/// Flushes immediately since this is an infrequent operation.
pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
	let device = {
		let devices = MIRAJAZZ_DEVICES.read().await;
		devices.get(id).cloned()
	};

	let Some(device) = device else { return Ok(()) };

	safe_hid_op(id, async {
		device.clear_all_button_images().await?;
		device.flush().await?;
		Ok::<(), MirajazzError>(())
	})
	.await;
	Ok(())
}

/// Set the brightness of a mirajazz device.
/// Brightness is a direct HID write (no cache), so no flush needed.
pub async fn set_brightness(id: &str, brightness: u8) {
	let device = {
		let devices = MIRAJAZZ_DEVICES.read().await;
		devices.get(id).cloned()
	};

	let Some(device) = device else { return };

	safe_hid_op(id, device.set_brightness(brightness.clamp(0, 100))).await;
}

/// Reset all connected mirajazz devices (called on app exit).
pub async fn reset_devices() {
	let devices: Vec<Arc<Device>> = MIRAJAZZ_DEVICES.read().await.values().cloned().collect();

	for device in devices {
		let _ = AssertUnwindSafe(async {
			let _ = device.reset().await;
			let _ = device.flush().await;
		})
		.catch_unwind()
		.await;
	}
}
