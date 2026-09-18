pub mod applications;
pub mod deep_link;
pub mod devices;
pub mod encoder;
pub mod keypad;
pub mod misc;
pub mod property_inspector;
pub mod settings;
pub mod states;
pub mod will_appear;

use futures::SinkExt;
use serde::Serialize;

/// Backlog retained for a plugin or property inspector that is not currently connected.
const MAX_QUEUED_MESSAGES: usize = 64;

#[derive(Serialize)]
struct Coordinates {
	row: u8,
	column: u8,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct GenericInstancePayload {
	settings: serde_json::Value,
	coordinates: Coordinates,
	controller: String,
	state: u16,
	isInMultiAction: bool,
}

impl GenericInstancePayload {
	fn new(instance: &crate::shared::ActionInstance) -> Self {
		let coordinates = match &instance.context.controller[..] {
			"Encoder" => Coordinates {
				row: 0,
				column: instance.context.position,
			},
			_ => {
				let columns = crate::shared::DEVICES.get(&instance.context.device).unwrap().columns;
				Coordinates {
					row: instance.context.position / columns,
					column: instance.context.position % columns,
				}
			}
		};

		Self {
			settings: instance.settings.clone(),
			coordinates,
			controller: instance.context.controller.clone(),
			state: instance.current_state,
			isInMultiAction: instance.context.index != 0,
		}
	}
}

async fn send_to_plugin(plugin: &str, data: &impl Serialize) -> Result<(), anyhow::Error> {
	let message = tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(data)?.into());
	let mut sockets = super::PLUGIN_SOCKETS.lock().await;

	if let Some((_, socket)) = sockets.get_mut(plugin) {
		socket.send(message).await?;
	} else {
		let mut queues = super::PLUGIN_QUEUES.write().await;
		let queue = queues.entry(plugin.to_owned()).or_default();
		if queue.len() >= MAX_QUEUED_MESSAGES {
			queue.remove(0);
		}
		queue.push(message);
	}

	Ok(())
}

async fn send_to_all_plugins(data: &impl Serialize) -> Result<(), anyhow::Error> {
	let mut entries = tokio::fs::read_dir(crate::shared::config_dir().join("plugins")).await?;
	while let Ok(Some(entry)) = entries.next_entry().await {
		let path = match entry.metadata().await?.is_symlink() {
			true => tokio::fs::read_link(entry.path()).await?,
			false => entry.path(),
		};
		let metadata = tokio::fs::metadata(&path).await?;
		if metadata.is_dir() {
			let _ = send_to_plugin(entry.file_name().to_str().unwrap(), data).await;
		}
	}
	Ok(())
}

async fn send_to_property_inspector(context: &crate::shared::ActionContext, data: &impl Serialize) -> Result<(), anyhow::Error> {
	let message = tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(data)?.into());
	let key = context.to_string();
	let mut sockets = super::PROPERTY_INSPECTOR_SOCKETS.lock().await;

	if let Some((_, socket)) = sockets.get_mut(&key) {
		socket.send(message).await?;
	} else {
		let mut queues = super::PROPERTY_INSPECTOR_QUEUES.write().await;
		let queue = queues.entry(key).or_default();
		if queue.len() >= MAX_QUEUED_MESSAGES {
			queue.remove(0);
		}
		queue.push(message);
	}

	Ok(())
}
