use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, LazyLock};

use futures::FutureExt;
use mirajazz::device::{Device, DeviceQuery, list_devices};
use mirajazz::error::MirajazzError;
use mirajazz::state::{DeviceStateReader, DeviceStateUpdate};
use mirajazz::types::{DeviceInput, ImageFormat, ImageMirroring, ImageMode, ImageRotation};
use tokio::sync::RwLock;

use crate::events::inbound;
use base64::Engine as _;

// ─── Device Constants (from opendeck-akp153 plugin mappings.rs) ─────────────

/// Device namespace prefix — "99" (hex of 153).
/// Must be 2 characters and unique across all device backends.
const DEVICE_NAMESPACE: &str = "99";

const AKP153_VID: u16 = 0x5548; // MIRABOX
const AKP153_PID: u16 = 0x6674;

/// HID query: usage_page=65440 (0xFFA0), usage=1, matching AKP153.
const AKP153_QUERY: DeviceQuery = DeviceQuery::new(65440, 1, AKP153_VID, AKP153_PID);

#[allow(dead_code)]
const ROW_COUNT: u8 = 3;
#[allow(dead_code)]
const COL_COUNT: u8 = 6;
const KEY_COUNT: usize = 18; // ROW_COUNT * COL_COUNT
const ENCODER_COUNT: usize = 0;

/// Stores active mirajazz device handles, keyed by device ID (e.g. "99-355499441494-153").
static MIRAJAZZ_DEVICES: LazyLock<RwLock<HashMap<String, Device>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

// ─── Key index mapping tables ───────────────────────────────────────────────

/// Maps OpenDeck logical key index → physical device key index.
const OPENDECK_TO_DEVICE: [u8; 18] = [12, 9, 6, 3, 0, 15, 13, 10, 7, 4, 1, 16, 14, 11, 8, 5, 2, 17];

/// Maps physical device key index (after subtracting 1) → OpenDeck logical key index.
const DEVICE_TO_OPENDECK: [u8; 18] = [4, 10, 16, 3, 9, 15, 2, 8, 14, 1, 7, 13, 0, 6, 12, 5, 11, 17];

#[allow(dead_code)]
pub fn opendeck_to_device(key: u8) -> u8 {
	if (key as usize) < OPENDECK_TO_DEVICE.len() { OPENDECK_TO_DEVICE[key as usize] } else { key }
}

fn device_to_opendeck(key: usize) -> usize {
	// Device reports keys starting at 1, so subtract 1 to index into the table.
	let idx = key.saturating_sub(1);
	if idx < DEVICE_TO_OPENDECK.len() { DEVICE_TO_OPENDECK[idx] as usize } else { key }
}

// ─── Image format ────────────────────────────────────────────────────────────

/// Returns the correct image format for a given key position (AKP153, v1 protocol).
#[allow(dead_code)]
pub fn get_image_format_for_key(_key: u8) -> ImageFormat {
	// v1 protocol: all keys use the same format.
	ImageFormat {
		mode: ImageMode::JPEG,
		size: (85, 85),
		rotation: ImageRotation::Rot90,
		mirror: ImageMirroring::Both,
	}
}

// ─── Input processing (callback for mirajazz reader) ─────────────────────────

/// Converts raw device input (key index + state) into a DeviceInput.
/// This is called by the mirajazz reader to parse HID reports.
fn process_input(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
	if input as usize > KEY_COUNT {
		return Err(MirajazzError::BadData);
	}

	let mut button_states = vec![false; KEY_COUNT];

	if input == 0 {
		// No button pressed — return all-released state.
		return Ok(DeviceInput::ButtonStateChange(button_states));
	}

	// Map device key (1-based) to OpenDeck index, then mark pressed.
	let opendeck_idx = device_to_opendeck(input as usize);
	if opendeck_idx < KEY_COUNT {
		button_states[opendeck_idx] = state != 0;
	}

	Ok(DeviceInput::ButtonStateChange(button_states))
}

// ─── Per-device task (M1: connect + register + reader loop + M4 reconnect) ───

/// Maximum number of immediate reconnection attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Delay between reconnection attempts.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
/// Timeout for a single connect + init sequence.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Initialise a newly-connected device: clear screen, set brightness, flush.
/// Returns the reader on success.
async fn setup_device(device: &Device, _device_id: &str) -> Result<Arc<DeviceStateReader>, MirajazzError> {
	device.clear_all_button_images().await?;
	let brightness = crate::store::get_settings().value.brightness;
	device.set_brightness(brightness).await?;
	device.flush().await?;
	Ok(device.get_reader(process_input))
}

/// Attempt to re-discover and reconnect the device after a fatal reader error.
/// On success, inserts the new device into `MIRAJAZZ_DEVICES` and returns the new reader.
async fn try_reconnect(device_id: &str) -> Option<Arc<DeviceStateReader>> {
	// Shutdown & remove old device to release HID handle.
	if let Some(old_device) = MIRAJAZZ_DEVICES.write().await.remove(device_id) {
		let _ = old_device.shutdown().await;
	}

	for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
		log::info!("mirajazz [{device_id}]: reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS}");
		tokio::time::sleep(RECONNECT_DELAY).await;

		// Re-enumerate devices.
		let devices = match list_devices(&[AKP153_QUERY]).await {
			Ok(d) => d,
			Err(e) => {
				log::warn!("mirajazz [{device_id}]: enumerate failed during reconnect: {e}");
				continue;
			}
		};

		// Find matching device by serial.
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

		// Connect with timeout + panic guard to avoid hanging/crashing on wedged device.
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
				MIRAJAZZ_DEVICES.write().await.insert(device_id.to_owned(), new_device);
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

/// Per-device async task. Connects, registers with OpenDeck, reads events, deregisters on exit.
async fn init(device: Device, device_id: String) {
	// Initial setup with timeout + panic guard (async-hid may panic on wedged device).
	let reader = match tokio::time::timeout(CONNECT_TIMEOUT, AssertUnwindSafe(setup_device(&device, &device_id)).catch_unwind()).await {
		Ok(Ok(Ok(reader))) => reader,
		Ok(Ok(Err(e))) => {
			log::warn!("mirajazz [{device_id}]: initial setup failed: {e}");
			return;
		}
		Ok(Err(_panic)) => {
			log::error!("mirajazz [{device_id}]: initial setup panicked (device likely wedged)");
			return;
		}
		Err(_) => {
			log::warn!("mirajazz [{device_id}]: initial setup timed out");
			return;
		}
	};

	// Insert device handle into MIRAJAZZ_DEVICES.
	MIRAJAZZ_DEVICES.write().await.insert(device_id.clone(), device);

	// Register with OpenDeck's device system.
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
				r#type: 0, // Standard keypad type
			},
		},
	)
	.await
	{
		log::error!("mirajazz [{device_id}]: register_device failed: {e}");
		MIRAJAZZ_DEVICES.write().await.remove(&device_id);
		return;
	}

	log::info!("mirajazz [{device_id}]: device registered, entering event loop");

	// Event loop — read input and route to OpenDeck's outbound event system.
	let press = |position: u8| inbound::PayloadEvent {
		payload: inbound::devices::PressPayload { device: device_id.clone(), position },
	};

	let mut reader = reader;
	loop {
		let updates = match reader.read(None).await {
			Ok(updates) => updates,
			Err(e) => {
				// Non-critical errors: skip and continue.
				if matches!(e, MirajazzError::ImageError(_) | MirajazzError::BadData) {
					log::debug!("mirajazz [{device_id}]: non-critical reader error: {e}");
					continue;
				}

				log::warn!("mirajazz [{device_id}]: fatal reader error: {e}, attempting reconnect");

				// M4: try to reconnect immediately.
				match try_reconnect(&device_id).await {
					Some(new_reader) => {
						reader = new_reader;
						continue;
					}
					None => break,
				}
			}
		};

		for update in updates {
			let result = match update {
				DeviceStateUpdate::ButtonDown(key) => inbound::devices::key_down(press(key)).await,
				DeviceStateUpdate::ButtonUp(key) => inbound::devices::key_up(press(key)).await,
				_ => Ok(()), // AKP153 has no encoders
			};
			if let Err(e) = result {
				log::warn!("mirajazz [{device_id}]: event routing error: {e}");
			}
		}
	}

	// Cleanup: deregister device.
	MIRAJAZZ_DEVICES.write().await.remove(&device_id);
	if let Err(e) = inbound::devices::deregister_device("", inbound::PayloadEvent { payload: device_id.clone() }).await {
		log::warn!("mirajazz [{device_id}]: deregister_device failed: {e}");
	}
	log::info!("mirajazz [{device_id}]: device task ended");
}

// ─── Initialisation ──────────────────────────────────────────────────────────

/// Attempt to discover and initialise all connected mirajazz-compatible devices.
///
/// Called every 10s from the main loop (alongside elgato::initialise_devices).
/// Registers the "99" device namespace and connects to any new AKP153 devices found.
pub async fn initialise_mirajazz_devices() {
	// Register namespace so that the plugin auth system recognises "99-..." device IDs as ours.
	crate::plugins::DEVICE_NAMESPACES.write().await.insert(DEVICE_NAMESPACE.to_owned(), "opendeck_native_akp153".to_owned());

	// Enumerate matching HID devices.
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
		// v1 devices share a fixed serial; generate the ID using id_suffix pattern.
		let serial = dev.serial_number.as_deref().unwrap_or("355499441494");
		let device_id = format!("{DEVICE_NAMESPACE}-{serial}-153");

		if MIRAJAZZ_DEVICES.read().await.contains_key(&device_id) {
			continue;
		}

		log::info!("mirajazz: discovered AKP153 (VID={:#06x} PID={:#06x}) → id={device_id}", dev.vendor_id, dev.product_id);

		match Device::connect(dev, 1, KEY_COUNT, ENCODER_COUNT).await {
			Ok(device) => {
				tokio::spawn(init(device, device_id));
			}
			Err(error) => {
				log::warn!("mirajazz: failed to connect to AKP153 ({device_id}): {error}");
			}
		}
	}
}

// ─── Image / Brightness / Reset (M3: called from outbound routing) ───────────

/// Set or clear a button image on a mirajazz device.
pub async fn update_image(context: &crate::shared::Context, image: Option<&str>) -> Result<(), anyhow::Error> {
	if let Some(device) = MIRAJAZZ_DEVICES.read().await.get(&context.device) {
		if let Some(img_data) = image {
			// Decode base64 jpeg from data URL (format: "data:image/jpeg;base64,...")
			let data = img_data.split_once(',').map(|(_, d)| d).unwrap_or(img_data);
			let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
			let dyn_image = image::load_from_memory(&bytes)?;
			let fmt = get_image_format_for_key(context.position);
			device.set_button_image(opendeck_to_device(context.position), fmt, dyn_image).await?;
		} else {
			device.clear_button_image(opendeck_to_device(context.position)).await?;
		}
		device.flush().await?;
	}
	Ok(())
}

/// Clear the entire screen of a mirajazz device.
pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
	if let Some(device) = MIRAJAZZ_DEVICES.read().await.get(id) {
		device.clear_all_button_images().await?;
		device.flush().await?;
	}
	Ok(())
}

/// Set the brightness of a mirajazz device.
pub async fn set_brightness(id: &str, brightness: u8) {
	if let Some(device) = MIRAJAZZ_DEVICES.read().await.get(id) {
		let _ = device.set_brightness(brightness.clamp(0, 100)).await;
		let _ = device.flush().await;
	}
}

/// Reset all connected mirajazz devices (called on app exit).
pub async fn reset_devices() {
	for (_id, device) in MIRAJAZZ_DEVICES.read().await.iter() {
		let _ = device.reset().await;
		let _ = device.flush().await;
	}
}
