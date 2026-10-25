use serde::Serialize;
use serde_json::{Value, json};

pub type EventSender = tokio::sync::broadcast::Sender<Value>;

pub fn emit(sender: &EventSender, kind: &str, data: &impl Serialize) {
    let _ = sender.send(json!({"type": kind, "data": data}));
}
