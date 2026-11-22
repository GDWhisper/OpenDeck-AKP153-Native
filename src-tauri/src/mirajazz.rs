use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use dashmap::DashSet;
use futures::FutureExt;
use mirajazz::device::{Device, DeviceQuery, list_devices};
use mirajazz::error::MirajazzError;
use mirajazz::state::{DeviceStateReader, DeviceStateUpdate};
use mirajazz::types::{DeviceInput, ImageFormat, ImageMirroring, ImageMode, ImageRotation};
use tauri::Emitter;
use tokio::sync::RwLock;

use crate::events::inbound;
use base64::Engine as _;

const DEVICE_NAMESPACE: &str = "99";

const AKP153_VID: u16 = 0x5548;
const AKP153_PID: u16 = 0x6674;

const AKP153_QUERY: DeviceQuery = DeviceQuery::new(65440, 1, AKP153_VID, AKP153_PID);

const DEVICE_NAME: &str = "Ajazz AKP153";

/// Consecutive setup timeouts after which the USB device node is reset. The
/// polling loop retries every 10s, so this waits roughly 30s before escalating.
const SETUP_TIMEOUTS_BEFORE_RESET: u32 = 3;

/// How long the device gets to come back after its node was reset. The only
/// reconnect path is the 10s polling loop, plus a full setup budget (30s) and
/// the margin for a power-management fix restarting the node once more.
const RECOVERY_REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

const RECOVERY_EVENT: &str = "device_recovery";

/// Restarting the application is the last resort: a stuck HID pipe can only be
/// released by exiting the process. The counter bounds how many consecutive
/// restarts a device that stays wedged may trigger before we give up.
const MAX_RECOVERY_RESTARTS: u32 = 3;
const RECOVERY_RESTART_MARKER: &str = ".akp153_recovery_restart_count";
const RECOVERY_PENDING_MARKER: &str = ".akp153_recovery_pending";
const RECOVERY_PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

static STARTUP_RECOVERY_CHECKED: AtomicBool = AtomicBool::new(false);

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

/// Tracks device IDs that currently have an active init task running.
/// Prevents spawning duplicate init tasks that would open competing HID handles.
static ACTIVE_INIT_TASKS: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

/// Consecutive setup timeouts per device. Cleared when setup succeeds.
static SETUP_TIMEOUTS: LazyLock<DashMap<String, u32>> = LazyLock::new(DashMap::new);

/// Devices for which an automatic USB reset has already been attempted. Removed
/// on successful setup so a later wedge in the same run can recover again.
static RECOVERY_ATTEMPTED: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

/// Devices with a recovery in flight. The polling loop must not open a new HID
/// handle while the device node is being re-enumerated.
static RECOVERY_ACTIVE: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

/// Devices that have been registered at least once in this process. A device
/// that worked and then fails is wedged, so recovery starts immediately instead
/// of waiting for a fresh connect to time out repeatedly.
static REGISTERED_DEVICES: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

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

/// Setup clears 18 images, sets the brightness and flushes, which is hundreds of
/// HID reports. A slow but healthy device must not be torn down for exceeding
/// the connect budget, so setup gets its own, much larger timeout.
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Upper bound for any single HID operation. A wedged device can leave a write
/// pending forever, silently freezing the flush task while the process stays
/// healthy; a timeout converts that into a wedge + reconnect via the polling loop.
const HID_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A flush writes every cached image, so it needs a far larger budget than a
/// single control command. Aborting a flush mid-transfer is what pushes the
/// device firmware into its wedged state.
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn setup_device(device: &Device, device_id: &str) -> Result<Arc<DeviceStateReader>, MirajazzError> {
	log::info!("mirajazz [{device_id}]: setup step 1/4: clear_all_button_images");
	device.clear_all_button_images().await?;
	log::info!("mirajazz [{device_id}]: setup step 2/4: set_brightness");
	let brightness = crate::store::get_settings().value.brightness;
	device.set_brightness(brightness).await?;
	log::info!("mirajazz [{device_id}]: setup step 3/4: flush");
	device.flush().await?;
	log::info!("mirajazz [{device_id}]: setup step 4/4: get_reader");
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

fn emit_recovery_event(state: &str) {
	if let Some(app) = crate::APP_HANDLE.get() {
		let _ = app.emit(RECOVERY_EVENT, serde_json::json!({ "state": state, "name": DEVICE_NAME }));
	}
}

fn note_setup_timeout(device_id: &str) {
	let mut entry = SETUP_TIMEOUTS.entry(device_id.to_owned()).or_insert(0);
	*entry += 1;
	let count = *entry;
	drop(entry);

	log::warn!("mirajazz [{device_id}]: initial setup timed out ({count}/{SETUP_TIMEOUTS_BEFORE_RESET})");
}

fn should_attempt_recovery(device_id: &str) -> bool {
	if crate::SHUTTING_DOWN.load(std::sync::atomic::Ordering::Relaxed) {
		return false;
	}

	// A device that already worked and now times out is wedged: escalate at once.
	// A device that never worked gets a few attempts, so a slow start is not
	// mistaken for a broken one.
	let threshold = if REGISTERED_DEVICES.contains(device_id) { 1 } else { SETUP_TIMEOUTS_BEFORE_RESET };
	let threshold_reached = SETUP_TIMEOUTS.get(device_id).map(|count| *count >= threshold).unwrap_or(false);
	threshold_reached && RECOVERY_ATTEMPTED.insert(device_id.to_owned())
}

async fn reset_usb_device() -> Result<(), String> {
	#[cfg(windows)]
	{
		crate::usb_power::reset_akp153_devices().await
	}
	#[cfg(not(windows))]
	{
		Err("automatic USB reset is only supported on Windows".to_owned())
	}
}

/// Runs once per process, before any HID handle is opened. A previous process
/// wrote the pending marker just before restarting because pnputil cannot clear
/// a device node that still owns a stuck I/O request.
async fn run_pending_startup_recovery() {
	if STARTUP_RECOVERY_CHECKED.swap(true, Ordering::SeqCst) {
		return;
	}

	let pending = crate::shared::config_dir().join(RECOVERY_PENDING_MARKER);
	let recent = std::fs::metadata(&pending)
		.and_then(|metadata| metadata.modified())
		.map(|modified| modified.elapsed().map(|elapsed| elapsed < RECOVERY_PENDING_TTL).unwrap_or(false))
		.unwrap_or(false);

	if !recent {
		let _ = std::fs::remove_file(&pending);
		return;
	}

	log::warn!("mirajazz: running pending AKP153 device node reset after recovery restart");
	emit_recovery_event("resetting");

	match reset_usb_device().await {
		Ok(()) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
		Err(error) => log::error!("mirajazz: pending AKP153 device node reset failed: {error}"),
	}

	let _ = std::fs::remove_file(&pending);
}

fn restart_app_for_recovery() -> bool {
	let marker = crate::shared::config_dir().join(RECOVERY_RESTART_MARKER);

	let mut attempts: u32 = std::fs::read_to_string(&marker)
		.ok()
		.and_then(|content| content.trim().parse::<u32>().ok())
		.unwrap_or(0);

	if attempts >= MAX_RECOVERY_RESTARTS {
		log::error!("mirajazz: recovery restart limit reached ({attempts}/{MAX_RECOVERY_RESTARTS}), giving up until the device recovers on its own");
		return false;
	}

	let Some(app) = crate::APP_HANDLE.get() else { return false };

	attempts += 1;
	let _ = std::fs::write(&marker, attempts.to_string());

	#[cfg(windows)]
	if let Err(error) = std::fs::write(crate::shared::config_dir().join(RECOVERY_PENDING_MARKER), attempts.to_string()) {
		log::error!("mirajazz: failed to persist pending recovery marker: {error}");
		return false;
	}

	log::warn!("mirajazz: restarting OpenDeck to clear the wedged HID pipe ({attempts}/{MAX_RECOVERY_RESTARTS})");
	crate::restart_app(app);
	true
}

/// The HID endpoint is wedged: reopening the handle no longer works, so ask the
/// OS to re-enumerate the device node. Runs once per wedge episode.
fn schedule_usb_recovery(device_id: String) {
	log::error!("mirajazz [{device_id}]: setup keeps timing out, resetting AKP153 USB device node");
	emit_recovery_event("resetting");

	RECOVERY_ACTIVE.insert(device_id.clone());

	tokio::spawn(async move {
		// Give the init task a moment to drop its HID handles before re-enumerating.
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;

		// The device may simply have been unplugged; resetting the USB node and
		// restarting the application would be pointless then.
		match list_devices(&[AKP153_QUERY]).await {
			Ok(devices) if devices.is_empty() => {
				log::warn!("mirajazz [{device_id}]: device is not enumerated, waiting for it to come back");
				RECOVERY_ACTIVE.remove(&device_id);
				RECOVERY_ATTEMPTED.remove(&device_id);
				return;
			}
			Ok(_) => {}
			Err(error) => log::warn!("mirajazz [{device_id}]: enumeration check failed: {error}"),
		}

		// Removing the device node only works once the process no longer owns a
		// stuck HID handle; otherwise the PnP manager reports ERROR_REBOOT_REQUIRED.
		// The handles are released now that dropping them no longer waits on the
		// wedged IRP, so the reset can happen without restarting the application.
		match reset_usb_device().await {
			Ok(()) => {
				// The polling loop is the only path that opens a HID handle, and it
				// skips devices marked RECOVERY_ACTIVE. Keeping the mark would make
				// this wait unable to succeed, so release it before waiting.
				RECOVERY_ACTIVE.remove(&device_id);

				let wait_secs = RECOVERY_REGISTER_TIMEOUT.as_secs();
				for _ in 0..wait_secs {
					tokio::time::sleep(std::time::Duration::from_secs(1)).await;
					if crate::shared::DEVICES.contains_key(&device_id) {
						log::info!("mirajazz [{device_id}]: recovered after USB device node reset");
						return;
					}
				}
				log::error!("mirajazz [{device_id}]: USB device was reset but did not re-register");
				// The device is gone from the bus entirely, so it has to be plugged
				// back in; restarting the application would not change that.
				if let Ok(devices) = list_devices(&[AKP153_QUERY]).await
					&& devices.is_empty()
				{
					log::warn!("mirajazz [{device_id}]: device is no longer enumerated, waiting for it to be plugged back in");
					RECOVERY_ATTEMPTED.remove(&device_id);
					emit_recovery_event("failed");
					return;
				}
				// The reset alone was not enough, fall through to restarting the app.
				if restart_app_for_recovery() {
					emit_recovery_event("restarting");
				} else {
					emit_recovery_event("failed");
				}
			}
			Err(error) => {
				log::warn!("mirajazz [{device_id}]: USB device reset failed: {error}");
				RECOVERY_ACTIVE.remove(&device_id);
				if restart_app_for_recovery() {
					emit_recovery_event("restarting");
				} else {
					emit_recovery_event("failed");
				}
			}
		}
	});
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
	safe_hid_op_with_timeout(device_id, HID_OP_TIMEOUT, op).await
}

async fn safe_hid_op_with_timeout<T>(
	device_id: &str,
	timeout: std::time::Duration,
	op: impl Future<Output = Result<T, MirajazzError>>,
) -> Option<T> {
	if WEDGED_DEVICES.contains(device_id) {
		return None;
	}

	match tokio::time::timeout(timeout, AssertUnwindSafe(op).catch_unwind()).await {
		Ok(Ok(Ok(result))) => Some(result),
		Ok(Ok(Err(e))) => {
			log::warn!("mirajazz [{device_id}]: HID error: {e}");
			None
		}
		Ok(Err(_panic)) => {
			if WEDGED_DEVICES.contains(device_id) {
				return None;
			}
			handle_wedge(device_id);
			if let Some(device) = MIRAJAZZ_DEVICES.write().await.remove(device_id) {
				drop(device);
			}
			None
		}
		Err(_elapsed) => {
			log::error!("mirajazz [{device_id}]: HID operation timed out after {timeout:?}");
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

/// Ask the frontend to re-render and re-push all key images.
/// Needed after any (re)connect because setup_device clears the screen and only
/// self-refreshing actions (e.g. clocks) would repaint on their own; the old
/// plugin achieved this by sending a rerenderImages event over WebSocket.
fn request_rerender(device_id: &str) {
	let device_id = device_id.to_owned();
	tokio::spawn(async move {
		if let Some(app) = crate::APP_HANDLE.get()
			&& let Err(e) = crate::events::frontend::profiles::rerender_images(app).await
		{
			log::warn!("mirajazz [{device_id}]: rerender request failed: {e}");
		}
	});
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

		safe_hid_op_with_timeout(&device_id, FLUSH_TIMEOUT, device.flush()).await;
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
			SETUP_TIMEOUT,
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
				request_rerender(device_id);
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

/// Owns an ACTIVE_INIT_TASKS entry so removal still happens if init unwinds.
struct ActiveInitGuard(String);

impl ActiveInitGuard {
	fn claim(device_id: &str) -> Self {
		ACTIVE_INIT_TASKS.insert(device_id.to_owned());
		Self(device_id.to_owned())
	}
}

impl Drop for ActiveInitGuard {
	fn drop(&mut self) {
		ACTIVE_INIT_TASKS.remove(&self.0);
	}
}

async fn init(device: Arc<Device>, device_id: String, _active: ActiveInitGuard) {
	let generation = bump_generation(&device_id);

	let result = init_inner(device.clone(), device_id.clone(), generation).await;

	let is_current = is_current_generation(&device_id, generation);

	if !is_current {
		log::info!("mirajazz [{device_id}]: stale init (gen {generation}), skipping cleanup");
	} else {
		if matches!(result, InitResult::Wedged) {
			MIRAJAZZ_DEVICES.write().await.remove(&device_id);
			WEDGED_DEVICES.remove(&device_id);
			cleanup_device_state(&device_id);
			if let Err(e) = inbound::devices::deregister_device("", inbound::PayloadEvent { payload: device_id.clone() }).await {
				log::warn!("mirajazz [{device_id}]: deregister_device failed: {e}");
			}
		}
		DEVICE_GENERATIONS.remove_if(&device_id, |_, v| *v == generation);
	}

	let recovery_due = is_current && should_attempt_recovery(&device_id);

	log::info!("mirajazz [{device_id}]: device task ended (gen {generation})");

	if recovery_due {
		// Schedule first: dropping a device with a stuck write can block while the
		// driver cancels the IRP, and recovery must not depend on that completing.
		schedule_usb_recovery(device_id.clone());
		drop(device);
	}
}

enum InitResult {
	NormalExit,
	Wedged,
}

async fn init_inner(device: Arc<Device>, device_id: String, generation: u64) -> InitResult {
	let reader = match tokio::time::timeout(SETUP_TIMEOUT, AssertUnwindSafe(setup_device(&device, &device_id)).catch_unwind()).await {
		Ok(Ok(Ok(reader))) => reader,
		Ok(Ok(Err(e))) => {
			log::warn!("mirajazz [{device_id}]: initial setup failed: {e}");
			return InitResult::NormalExit;
		}
		Ok(Err(_panic)) => {
			log::error!("mirajazz [{device_id}]: initial setup panicked (device likely wedged)");
			handle_wedge(&device_id);
			return InitResult::Wedged;
		}
		Err(_) => {
			if is_current_generation(&device_id, generation) {
				note_setup_timeout(&device_id);
			}
			return InitResult::NormalExit;
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
				name: DEVICE_NAME.to_owned(),
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
		return InitResult::NormalExit;
	}

	SETUP_TIMEOUTS.remove(&device_id);
	REGISTERED_DEVICES.insert(device_id.clone());
	let restart_marker = crate::shared::config_dir().join(RECOVERY_RESTART_MARKER);
	// A recovery that was started by a previous process shows up as the marker
	// file, one started by this process as the in-memory set.
	if RECOVERY_ATTEMPTED.remove(&device_id).is_some() || restart_marker.exists() {
		// The device works again, so the next wedge starts a fresh recovery cycle.
		let _ = std::fs::remove_file(&restart_marker);
		emit_recovery_event("recovered");
	}

	log::info!("mirajazz [{device_id}]: device registered, entering event loop");
	request_rerender(&device_id);

	let press = |position: u8| inbound::PayloadEvent {
		payload: inbound::devices::PressPayload { device: device_id.clone(), position },
	};

	let mut reader = reader;
	loop {
		if WEDGED_DEVICES.contains(&device_id) {
			log::info!("mirajazz [{device_id}]: device wedged, exiting event loop");
			return InitResult::Wedged;
		}

		if !is_current_generation(&device_id, generation) {
			log::info!("mirajazz [{device_id}]: newer generation detected, exiting event loop");
			return InitResult::NormalExit;
		}

		let updates = match AssertUnwindSafe(reader.read(Some(READER_TIMEOUT))).catch_unwind().await {
			Ok(Ok(updates)) => updates,
			Ok(Err(e)) => {
				if matches!(e, MirajazzError::ImageError(_) | MirajazzError::BadData) {
					log::debug!("mirajazz [{device_id}]: non-critical reader error: {e}");
					continue;
				}

				log::warn!("mirajazz [{device_id}]: fatal reader error: {e}, attempting reconnect");

				if REGISTERED_DEVICES.contains(&device_id) {
					// The device worked before and its pipe is now broken; reconnecting
					// the same broken pipe only wastes time, so recover the device node.
					log::error!("mirajazz [{device_id}]: reader failed on a working device, treating it as wedged");
					handle_wedge(&device_id);
					return InitResult::Wedged;
				}

				match try_reconnect(&device_id, generation).await {
					Some(new_reader) => {
						reader = new_reader;
						continue;
					}
					None => return InitResult::NormalExit,
				}
			}
			Err(_) => {
				log::error!("mirajazz [{device_id}]: reader panicked (device likely wedged)");
				handle_wedge(&device_id);
				return InitResult::Wedged;
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
}

pub async fn initialise_mirajazz_devices() {
	run_pending_startup_recovery().await;

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

	// AKP153 firmware deadlocks when Windows suspends it; self-heal the per-instance
	// registry setting before (re)connecting. No-op once all instances are fixed.
	#[cfg(windows)]
	{
		crate::usb_power::ensure_akp153_power_management();
		// The fix restarts the device node. Opening a handle while it runs would
		// produce a spurious wedge and one more disconnect, so let the polling
		// loop connect on a later tick.
		if crate::usb_power::power_fix_in_progress() {
			log::info!("mirajazz: waiting for the AKP153 power-management fix before connecting");
			return;
		}
	}

	for dev in &devices {
		let serial = dev.serial_number.as_deref().unwrap_or("355499441494");
		let device_id = format!("{DEVICE_NAMESPACE}-{serial}-153");

		if MIRAJAZZ_DEVICES.read().await.contains_key(&device_id) {
			continue;
		}

		if ACTIVE_INIT_TASKS.contains(&device_id) {
			continue;
		}

		// A recovery is re-enumerating the device node; opening a handle now would
		// make the removal fail and the recovery pointless.
		if RECOVERY_ACTIVE.contains(&device_id) {
			continue;
		}

		if WEDGED_DEVICES.contains(&device_id) {
			if let Some(old) = MIRAJAZZ_DEVICES.write().await.remove(&device_id) {
				drop(old);
			}
			WEDGED_DEVICES.remove(&device_id);
			DEVICE_GENERATIONS.remove(&device_id);
			log::info!("mirajazz [{device_id}]: cleared wedged state, will retry connection");
		}

		log::info!("mirajazz: discovered AKP153 (VID={:#06x} PID={:#06x}) → id={device_id}", dev.vendor_id, dev.product_id);

		match tokio::time::timeout(CONNECT_TIMEOUT, Device::connect(dev, 1, KEY_COUNT, ENCODER_COUNT)).await {
			Ok(Ok(device)) => {
				tokio::spawn(init(Arc::new(device), device_id.clone(), ActiveInitGuard::claim(&device_id)));
			}
			Ok(Err(error)) => {
				log::warn!("mirajazz: failed to connect to AKP153 ({device_id}): {error}");
			}
			Err(_) => {
				log::warn!("mirajazz: Device::connect timed out for {device_id}");
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

	let mut generations: Vec<(String, u64)> = Vec::with_capacity(device_ids.len());
	for device_id in &device_ids {
		log::info!("mirajazz [{device_id}]: bumping generation for resume from sleep");
		generations.push((device_id.clone(), bump_generation(device_id)));
		WEDGED_DEVICES.remove(device_id);
		// Timeouts from before the sleep must not push the post-wake reconnect into
		// the recovery path while USB is still re-enumerating.
		SETUP_TIMEOUTS.remove(device_id);
	}

	if !device_ids.is_empty() {
		let mut devices = MIRAJAZZ_DEVICES.write().await;
		devices.clear();
	}

	let cleared = generations.len();
	tokio::spawn(async move {
		tokio::time::sleep(std::time::Duration::from_secs(12)).await;
		for (device_id, generation) in &generations {
			// Removing an entry a reconnect already re-bumped would kill that live generation.
			if DEVICE_GENERATIONS.remove_if(device_id, |_, v| *v == *generation).is_some() {
				log::info!("mirajazz [{device_id}]: gen {generation} not reconnected, cleared for the polling loop");
			}
		}
		log::info!("mirajazz: {cleared} AKP153 device(s) cleared for reconnection after system resume");
	});
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
