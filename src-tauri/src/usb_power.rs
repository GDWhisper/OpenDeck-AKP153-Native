//! Windows USB power management fix for AKP153 devices.
//!
//! The AKP153 firmware deadlocks when Windows selectively suspends it after
//! ~90 seconds of input idle (error 0x8007001F), requiring a physical replug.
//! Elgato's software disables `EnhancedPowerManagementEnabled` for its own
//! devices the same way; we own that responsibility for Ajazz hardware.
//!
//! The registry value is stored per device *instance* (one per USB port), so a
//! one-time fix does not survive plugging into a new port. This module checks
//! on every device discovery and self-heals via a UAC-elevated PowerShell that
//! writes the value and restarts the device node (no replug needed).

use std::sync::atomic::{AtomicBool, Ordering};

const ENUM_PATH: &str = r"SYSTEM\CurrentControlSet\Enum\USB\VID_5548&PID_6674";

/// Only prompt for elevation once per app run, even if the user declines.
static FIX_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// True while the elevated fix is running. It restarts the device node, so the
/// discovery loop must not open a HID handle until it finishes.
static FIX_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub fn power_fix_in_progress() -> bool {
	FIX_IN_PROGRESS.load(Ordering::SeqCst)
}

struct InProgressReset;

impl Drop for InProgressReset {
	fn drop(&mut self) {
		FIX_IN_PROGRESS.store(false, Ordering::SeqCst);
	}
}

/// Returns device instance IDs whose EnhancedPowerManagementEnabled is not 0.
/// A missing value is treated as needing the fix, as the HID driver defaults to enabled.
fn instances_needing_fix() -> Vec<String> {
	use winreg::RegKey;
	use winreg::enums::HKEY_LOCAL_MACHINE;

	let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
	let Ok(base) = hklm.open_subkey(ENUM_PATH) else { return Vec::new() };

	let mut out = Vec::new();
	for instance in base.enum_keys().flatten() {
		let Ok(params) = base.open_subkey(format!(r"{instance}\Device Parameters")) else { continue };
		match params.get_value::<u32, _>("EnhancedPowerManagementEnabled") {
			Ok(0) => (),
			_ => out.push(instance),
		}
	}
	out
}

/// Check all AKP153 device instances and, if any still have enhanced power
/// management enabled, run an elevated PowerShell to disable it and restart
/// the device nodes. Called from the device discovery loop; cheap when
/// everything is already fixed.
pub fn ensure_akp153_power_management() {
	let instances = instances_needing_fix();
	if instances.is_empty() {
		return;
	}

	if FIX_ATTEMPTED.swap(true, Ordering::SeqCst) {
		return;
	}

	log::warn!(
		"usb_power: {} AKP153 instance(s) have USB enhanced power management enabled (firmware deadlocks on suspend); requesting elevation to fix",
		instances.len()
	);

	FIX_IN_PROGRESS.store(true, Ordering::SeqCst);

	tokio::task::spawn_blocking(move || {
		let _in_progress = InProgressReset;
		let script = format!(
			r#"$base = 'HKLM:\{ENUM_PATH}'
Get-ChildItem $base | ForEach-Object {{
	$p = Join-Path $_.PSPath 'Device Parameters'
	if (Test-Path $p) {{ Set-ItemProperty -Path $p -Name EnhancedPowerManagementEnabled -Value 0 -Type DWord }}
	pnputil /restart-device "USB\VID_5548&PID_6674\$($_.PSChildName)" | Out-Null
}}
exit 0"#
		);

		let script_path = std::env::temp_dir().join("opendeck_akp153_power_fix.ps1");
		if let Err(e) = std::fs::write(&script_path, script) {
			log::error!("usb_power: failed to write fix script: {e}");
			return;
		}

		// Outer non-elevated PowerShell triggers the UAC prompt and waits for the result.
		let status = std::process::Command::new("powershell")
			.args([
				"-NoProfile",
				"-WindowStyle",
				"Hidden",
				"-Command",
				&format!(
					"Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File \"{}\"'",
					script_path.display()
				),
			])
			.status();

		let _ = std::fs::remove_file(&script_path);

		match status {
			Ok(s) if s.success() => log::info!("usb_power: AKP153 power management disabled and device restarted"),
			Ok(s) => log::warn!("usb_power: elevation declined or fix failed (exit: {s})"),
			Err(e) => log::error!("usb_power: failed to launch fix: {e}"),
		}
	});
}

/// Remove and rescan every present AKP153 device node. Stronger than
/// `restart-device`, which the PnP manager rejects (ERROR_NOT_SUPPORTED) while a
/// wedged device still owns a stuck I/O request. Must run while the app holds no
/// HID handles; callers use this from a freshly restarted process.
pub async fn reset_akp153_devices() -> Result<(), String> {
	tokio::task::spawn_blocking(|| -> Result<(), String> {
		let script = r#"$ErrorActionPreference = 'Stop'
# Disable USB enhanced power management for every known instance first, so a
# freshly enumerated device does not get suspended by Windows.
Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Enum\USB\VID_5548&PID_6674' -ErrorAction SilentlyContinue | ForEach-Object {
	$p = Join-Path $_.PSPath 'Device Parameters'
	if (Test-Path $p) {
		Set-ItemProperty -Path $p -Name EnhancedPowerManagementEnabled -Value 0 -Type DWord -ErrorAction SilentlyContinue
	}
}
# The HID class nodes go first: a wedged device can be left with HID children
# whose USB parent is already gone, and those orphans keep the device visible to
# the application while it can no longer be reached.
$hid = @(Get-PnpDevice | Where-Object { $_.InstanceId -like 'HID\VID_5548&PID_6674*' } | Select-Object -ExpandProperty InstanceId)
$usb = @(Get-PnpDevice | Where-Object { $_.InstanceId -like 'USB\VID_5548&PID_6674*' } | Select-Object -ExpandProperty InstanceId)
$failed = 0
foreach ($id in ($hid + $usb)) {
	& pnputil.exe /remove-device "$id" | Out-Null
	if ($LASTEXITCODE -ne 0) { $failed++ }
}
& pnputil.exe /scan-devices | Out-Null
if ($LASTEXITCODE -ne 0) { $failed++ }
Start-Sleep -Seconds 3
exit $failed"#;

		let script_path = std::env::temp_dir().join("opendeck_akp153_reset.ps1");
		std::fs::write(&script_path, script).map_err(|e| format!("failed to write reset script: {e}"))?;

		let command = format!(
			"try {{ $p = Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -PassThru -ErrorAction Stop -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File \"{}\"'; exit $p.ExitCode }} catch {{ exit 2 }}",
			script_path.display()
		);
		let status = std::process::Command::new("powershell").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &command]).status();

		let _ = std::fs::remove_file(&script_path);

		match status {
			Ok(s) if s.success() => {
				log::info!("usb_power: AKP153 device node removed and rescanned");
				Ok(())
			}
			Ok(s) => Err(format!("pnputil remove/scan failed or elevation was declined (exit: {s})")),
			Err(e) => Err(format!("failed to launch elevated PowerShell: {e}")),
		}
	})
	.await
	.map_err(|e| format!("reset task failed: {e}"))?
}
