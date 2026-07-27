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

	tokio::task::spawn_blocking(move || {
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
