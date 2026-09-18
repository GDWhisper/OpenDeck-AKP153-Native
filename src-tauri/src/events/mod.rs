pub mod frontend;
pub mod inbound;
pub mod outbound;

use inbound::RegisterEvent;

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{SinkExt, StreamExt, stream::SplitSink};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Sockets = LazyLock<Mutex<HashMap<String, (u64, SplitSink<WebSocketStream<TcpStream>, Message>)>>>;
static PLUGIN_SOCKETS: Sockets = LazyLock::new(|| Mutex::new(HashMap::new()));
static PROPERTY_INSPECTOR_SOCKETS: Sockets = LazyLock::new(|| Mutex::new(HashMap::new()));
static PLUGIN_QUEUES: LazyLock<RwLock<HashMap<String, Vec<Message>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static PROPERTY_INSPECTOR_QUEUES: LazyLock<RwLock<HashMap<String, Vec<Message>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
/// Tags each connection so a stale reader task cannot unregister a newer socket with the same key.
static CONNECTION_IDS: AtomicU64 = AtomicU64::new(0);

pub async fn registered_plugins() -> Vec<String> {
	PLUGIN_SOCKETS.lock().await.keys().map(|x| x.to_owned()).collect()
}

/// Register a plugin or property inspector to send and receive events with its WebSocket.
pub async fn register_plugin(event: RegisterEvent, stream: WebSocketStream<TcpStream>) {
	let id = CONNECTION_IDS.fetch_add(1, Ordering::Relaxed);
	let (read, write) = stream.split();
	match event {
		RegisterEvent::RegisterPlugin { uuid } => {
			log::debug!("Registered plugin {}", uuid);
			PLUGIN_SOCKETS.lock().await.insert(uuid.clone(), (id, read));
			if let Some(queue) = PLUGIN_QUEUES.write().await.remove(&uuid) {
				let mut sockets = PLUGIN_SOCKETS.lock().await;
				if let Some((_, sink)) = sockets.get_mut(&uuid) {
					for message in queue {
						let _ = sink.feed(message).await;
					}
					let _ = sink.flush().await;
				}
			}
			tokio::spawn(async move {
				let uuid = uuid;
				write.for_each(|event| inbound::process_incoming_message(event, &uuid, false)).await;
				let mut sockets = PLUGIN_SOCKETS.lock().await;
				if sockets.get(&uuid).is_some_and(|(current, _)| *current == id) {
					sockets.remove(&uuid);
				}
			});
		}
		RegisterEvent::RegisterPropertyInspector { uuid } => {
			PROPERTY_INSPECTOR_SOCKETS.lock().await.insert(uuid.clone(), (id, read));
			if let Some(queue) = PROPERTY_INSPECTOR_QUEUES.write().await.remove(&uuid) {
				let mut sockets = PROPERTY_INSPECTOR_SOCKETS.lock().await;
				if let Some((_, sink)) = sockets.get_mut(&uuid) {
					for message in queue {
						let _ = sink.feed(message).await;
					}
					let _ = sink.flush().await;
				}
			}
			tokio::spawn(async move {
				let uuid = uuid;
				write.for_each(|event| inbound::process_incoming_message_pi(event, &uuid)).await;
				let mut sockets = PROPERTY_INSPECTOR_SOCKETS.lock().await;
				if sockets.get(&uuid).is_some_and(|(current, _)| *current == id) {
					sockets.remove(&uuid);
					PROPERTY_INSPECTOR_QUEUES.write().await.remove(&uuid);
				}
			});
		}
	};
}
