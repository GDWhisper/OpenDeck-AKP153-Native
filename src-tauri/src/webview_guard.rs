//! Recovers the main window when its WebView2 process fails.
//!
//! WebView2 does not automatically restart a crashed browser process. Without
//! this guard the Rust side keeps running (tray, devices and plugins still
//! work) while the UI stays dead: every script evaluation fails with
//! `ERROR_INVALID_STATE` and the main window never repaints. Reloading the
//! webview recovers a lost renderer; losing the browser process requires
//! restarting the whole application.

#[cfg(windows)]
mod imp {
	use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
	use std::time::{Duration, SystemTime, UNIX_EPOCH};

	use tauri::{AppHandle, Manager};
	use webview2_com::Microsoft::Web::WebView2::Win32::*;
	use webview2_com::ProcessFailedEventHandler;

	/// Renderer reloads allowed before falling back to a full restart.
	const MAX_RENDERER_RELOADS: u32 = 2;

	/// Reloads separated by at least this many seconds get a fresh budget, so a
	/// crash once every few days never accumulates into a forced restart.
	const RENDERER_RELOAD_WINDOW_SECS: u64 = 60;

	/// Set while a recovery is in progress. WebView2 fires one event per failed
	/// process, and restarting twice would spawn competing instances.
	static HANDLING_FAILURE: AtomicBool = AtomicBool::new(false);
	static RENDERER_RELOADS: AtomicU32 = AtomicU32::new(0);
	static LAST_RENDERER_RELOAD_SECS: AtomicU64 = AtomicU64::new(0);

	pub fn install(app: &AppHandle) {
		let Some(window) = app.get_webview_window("main") else {
			log::warn!("webview guard: main window not found; WebView2 crashes will not be recovered");
			return;
		};

		let app_handle = app.clone();
		let result = window.with_webview(move |platform_webview| {
			// SAFETY: tauri executes this closure on the WebView2 UI thread.
			let webview = match unsafe { platform_webview.controller().CoreWebView2() } {
				Ok(webview) => webview,
				Err(error) => {
					log::warn!("webview guard: failed to get ICoreWebView2: {error}");
					return;
				}
			};

			let mut token = 0;
			let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
				on_process_failed(app_handle.clone(), args);
				Ok(())
			}));

			// SAFETY: registering an event handler on a valid ICoreWebView2 instance.
			if let Err(error) = unsafe { webview.add_ProcessFailed(&handler, &mut token) } {
				log::warn!("webview guard: failed to register the ProcessFailed handler: {error}");
			} else {
				log::debug!("webview guard: watching for WebView2 process failures");
			}
		});

		if let Err(error) = result {
			log::warn!("webview guard: failed to access the main webview: {error}");
		}
	}

	fn on_process_failed(app: AppHandle, args: Option<ICoreWebView2ProcessFailedEventArgs>) {
		let Some(args) = args else { return };

		// SAFETY: the argument interface is valid for the duration of the callback.
		let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
		if let Err(error) = unsafe { args.ProcessFailedKind(&mut kind) } {
			log::warn!("webview guard: failed to read the WebView2 process failure kind: {error}");
			return;
		}

		log::error!("webview guard: WebView2 process failed: {}", kind_name(kind));

		if crate::SHUTTING_DOWN.load(Ordering::SeqCst) {
			return;
		}
		if HANDLING_FAILURE.swap(true, Ordering::SeqCst) {
			return;
		}

		match kind {
			COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
				log::error!("webview guard: WebView2 browser process exited; restarting to rebuild the UI");
				restart_after_delay(app);
			}
			COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
			| COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED
			| COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED => {
				reload_or_restart(app);
			}
			COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
				log::warn!("webview guard: WebView2 renderer is unresponsive; leaving it to recover");
				HANDLING_FAILURE.store(false, Ordering::SeqCst);
			}
			_ => {
				// GPU, utility and sandbox helper processes are recreated by
				// WebView2 itself; restarting the app for those would be disruptive.
				log::warn!("webview guard: auxiliary WebView2 process exited; waiting for WebView2 to recover");
				HANDLING_FAILURE.store(false, Ordering::SeqCst);
			}
		}
	}

	fn reload_or_restart(app: AppHandle) {
		let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
		if now.saturating_sub(LAST_RENDERER_RELOAD_SECS.load(Ordering::SeqCst)) > RENDERER_RELOAD_WINDOW_SECS {
			RENDERER_RELOADS.store(0, Ordering::SeqCst);
		}
		LAST_RENDERER_RELOAD_SECS.store(now, Ordering::SeqCst);

		let attempt = RENDERER_RELOADS.fetch_add(1, Ordering::SeqCst) + 1;
		if attempt > MAX_RENDERER_RELOADS {
			log::error!("webview guard: renderer failed {attempt} times in a row; restarting the app");
			restart_after_delay(app);
			return;
		}

		std::thread::spawn(move || {
			// Give WebView2 a moment to settle before asking it to reload.
			std::thread::sleep(Duration::from_millis(1000));

			if crate::SHUTTING_DOWN.load(Ordering::SeqCst) {
				return;
			}

			let Some(window) = app.get_webview_window("main") else {
				HANDLING_FAILURE.store(false, Ordering::SeqCst);
				return;
			};

			log::info!("webview guard: reloading the main window (attempt {attempt}/{MAX_RENDERER_RELOADS})");
			if let Err(error) = window.reload() {
				log::error!("webview guard: reload failed: {error}; restarting the app");
				crate::restart_app(&app);
				return;
			}

			HANDLING_FAILURE.store(false, Ordering::SeqCst);
		});
	}

	fn restart_after_delay(app: AppHandle) {
		std::thread::spawn(move || {
			// Give the failure callback time to return before tearing down the process.
			std::thread::sleep(Duration::from_millis(500));

			if crate::SHUTTING_DOWN.load(Ordering::SeqCst) {
				return;
			}

			crate::restart_app(&app);
		});

		// The graceful exit path tears down a WebView2 that has just failed. If it
		// stalls, terminate anyway so the restart helper can start a fresh process.
		std::thread::spawn(|| {
			std::thread::sleep(Duration::from_secs(10));
			log::error!("webview guard: graceful exit did not finish within 10s; forcing exit");
			std::process::exit(0);
		});
	}

	fn kind_name(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> &'static str {
		match kind {
			COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => "browser process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => "renderer process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => "renderer process unresponsive",
			COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED => "frame renderer process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED => "utility process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED => "sandbox helper process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED => "GPU process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED => "PPAPI plugin process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED => "PPAPI broker process exited",
			COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED => "unknown process exited",
			_ => "unrecognised process failure",
		}
	}
}

#[cfg(windows)]
pub use imp::install;

/// WebView2 is Windows-only; other platforms keep their existing behaviour.
#[cfg(not(windows))]
pub fn install(_app: &tauri::AppHandle) {}
