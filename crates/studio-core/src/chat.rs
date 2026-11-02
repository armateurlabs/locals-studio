use crate::{
    events::{EventSender, emit},
    store::Store,
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, oneshot};

#[derive(Clone)]
pub struct ChatManager {
    store: Store,
    events: EventSender,
    active: Arc<Mutex<HashMap<String, ActiveChat>>>,
    mutations: Arc<Mutex<()>>,
}

struct ActiveChat {
    conversation_id: String,
    cancel: Option<oneshot::Sender<()>>,
}

impl ChatManager {
    pub fn new(store: Store, events: EventSender) -> Self {
        Self::with_mutations(store, events, Arc::new(Mutex::new(())))
    }

    pub fn with_mutations(store: Store, events: EventSender, mutations: Arc<Mutex<()>>) -> Self {
        Self {
            store,
            events,
            active: Default::default(),
            mutations,
        }
    }

    pub async fn is_active(&self, conversation_id: &str) -> bool {
        self.active
            .lock()
            .await
            .values()
            .any(|v| v.conversation_id == conversation_id)
    }

    pub async fn start(&self, params: Value, base_url: String) -> Result<Value> {
        let conversation_id = params["conversation_id"]
            .as_str()
            .context("conversation_id is required")?
            .to_owned();
        let model = params["model"]
            .as_str()
            .context("model is required")?
            .to_owned();
        let mut conversation = self
            .store
            .get::<Value>("conversations", &conversation_id)?
            .context("Conversation not found")?;
        let messages = params
            .get("messages")
            .cloned()
            .unwrap_or_else(|| conversation["messages"].clone());
        if !messages.is_array() || messages.as_array().is_none_or(|m| m.is_empty()) {
            bail!("At least one message is required");
        }
        let mut active = self.active.lock().await;
        if active
            .values()
            .any(|v| v.conversation_id == conversation_id)
        {
            bail!("A reply is already running in this conversation");
        }
        conversation["messages"] = messages.clone();
        conversation["model"] = json!(model);
        conversation["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
        self.store
            .set("conversations", &conversation_id, &conversation)?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        active.insert(
            request_id.clone(),
            ActiveChat {
                conversation_id: conversation_id.clone(),
                cancel: Some(cancel_tx),
            },
        );
        drop(active);
        let manager = self.clone();
        let reply = json!({"request_id":request_id,"conversation_id":conversation_id});
        tokio::spawn(async move {
            let result = manager
                .generate(
                    &request_id,
                    &conversation_id,
                    &model,
                    messages,
                    params.get("parameters").cloned().unwrap_or(json!({})),
                    base_url,
                    cancel_rx,
                )
                .await;
            if let Err(error) = result {
                emit(
                    &manager.events,
                    "chat.error",
                    &json!({"request_id":request_id,"conversation_id":conversation_id,"error":format!("{error:#}")}),
                );
            }
            manager.active.lock().await.remove(&request_id);
        });
        Ok(reply)
    }

    pub async fn cancel(&self, id: &str) -> Result<Value> {
        if let Some(chat) = self.active.lock().await.get_mut(id) {
            if let Some(cancel) = chat.cancel.take() {
                let _ = cancel.send(());
            }
        }
        Ok(json!({"cancelled":true,"request_id":id}))
    }

    async fn generate(
        &self,
        request_id: &str,
        conversation_id: &str,
        model: &str,
        messages: Value,
        parameters: Value,
        base_url: String,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<()> {
        let mut body = parameters.as_object().cloned().unwrap_or_default();
        let output_token_limit = requested_output_token_limit(&body);
        body.insert("model".into(), json!(model));
        body.insert("messages".into(), messages);
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage":true}));
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let session_id = studio_session_id(conversation_id);
        let response = tokio::select! {
            _ = &mut cancel => { emit(&self.events,"chat.done",&json!({"request_id":request_id,"conversation_id":conversation_id,"cancelled":true})); return Ok(()); },
            result = client.post(format!("{base_url}/v1/chat/completions")).header("x-locals-session-id", session_id).json(&body).send() => result?,
        };
        if !response.status().is_success() {
            bail!(
                "Model request failed: {} {}",
                response.status(),
                response.text().await?
            );
        }
        let routed_profile_id = response
            .headers()
            .get("x-locals-profile-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let routed_model_id = response
            .headers()
            .get("x-locals-model-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let routed_alias = response
            .headers()
            .get("x-locals-server-alias")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Value::Null;
        let mut finish_reason = None;
        let mut cancelled = false;
        let mut failure = None;
        'generation: loop {
            let chunk = tokio::select! {
                _ = &mut cancel => { cancelled = true; break; },
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = match chunk {
                Ok(c) => c,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            };
            let events = match parser.push(&chunk) {
                Ok(events) => events,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            };
            for event in events {
                if event.trim() == "[DONE]" {
                    break 'generation;
                }
                let value: Value = match serde_json::from_str(&event) {
                    Ok(value) => value,
                    Err(error) => {
                        failure = Some(format!("Invalid model streaming response: {error}"));
                        break 'generation;
                    }
                };
                if let Some(error) = value.get("error") {
                    failure = Some(error.to_string());
                    break;
                }
                if let Some(value_usage) = value.get("usage").filter(|v| !v.is_null()) {
                    usage = value_usage.clone();
                }
                if let Some(reason) = value["choices"][0]["finish_reason"].as_str() {
                    finish_reason = Some(reason.to_owned());
                }
                let delta = &value["choices"][0]["delta"];
                let text = delta["content"].as_str().unwrap_or("");
                let thought = delta["reasoning_content"].as_str().unwrap_or("");
                content.push_str(text);
                reasoning.push_str(thought);
                if !text.is_empty() || !thought.is_empty() {
                    emit(
                        &self.events,
                        "chat.delta",
                        &json!({"request_id":request_id,"conversation_id":conversation_id,"delta":text,"reasoning_delta":thought}),
                    );
                }
            }
            if failure.is_some() {
                break;
            }
        }
        // Drop the HTTP stream immediately on cancellation so the gateway and worker stop decoding.
        drop(stream);
        let _mutation = self.mutations.lock().await;
        let mut conversation = self
            .store
            .get::<Value>("conversations", conversation_id)?
            .context("Conversation was removed during generation")?;
        let message = json!({"id":uuid::Uuid::new_v4().to_string(),"role":"assistant","content":content,"reasoning_content":reasoning,"model":model,"usage":usage,"finish_reason":finish_reason,"output_token_limit":output_token_limit,"routed_profile_id":routed_profile_id,"routed_model_id":routed_model_id,"routed_alias":routed_alias,"cancelled":cancelled,"error":failure,"created_at":chrono::Utc::now().to_rfc3339()});
        conversation["messages"]
            .as_array_mut()
            .context("Invalid saved messages")?
            .push(message.clone());
        conversation["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
        self.store
            .set("conversations", conversation_id, &conversation)?;
        emit(
            &self.events,
            "chat.done",
            &json!({"request_id":request_id,"conversation_id":conversation_id,"cancelled":cancelled,"usage":usage,"message":message}),
        );
        if let Some(error) = failure {
            bail!("{error}");
        }
        Ok(())
    }
}

fn studio_session_id(conversation_id: &str) -> String {
    format!("studio-{:x}", Sha256::digest(conversation_id.as_bytes()))
}

fn requested_output_token_limit(parameters: &serde_json::Map<String, Value>) -> Option<u64> {
    parameters
        .get("max_completion_tokens")
        .or_else(|| parameters.get("max_tokens"))
        .and_then(Value::as_u64)
        .filter(|limit| *limit > 0)
}

#[derive(Default)]
pub struct SseParser {
    bytes: Vec<u8>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > 8 * 1024 * 1024 {
            bail!("Streaming event exceeds 8 MiB");
        }
        let mut events = Vec::new();
        loop {
            let Some((offset, length)) = [
                self.bytes
                    .windows(2)
                    .position(|v| v == b"\n\n")
                    .map(|i| (i, 2)),
                self.bytes
                    .windows(4)
                    .position(|v| v == b"\r\n\r\n")
                    .map(|i| (i, 4)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(offset, _)| *offset) else {
                break;
            };
            let frame = String::from_utf8(self.bytes.drain(..offset + length).collect())?;
            let data = frame
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                events.push(data);
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_session_headers_preserve_arbitrary_rpc_ids_without_exposing_them() {
        for id in [
            "chat-123".to_owned(),
            "conversation partagée 🌍".to_owned(),
            "line\nbreak".to_owned(),
            "long".repeat(1024),
        ] {
            let session = studio_session_id(&id);
            assert_eq!(session.len(), 71);
            assert!(session.starts_with("studio-"));
            assert!(session.is_ascii());
            assert!(reqwest::header::HeaderValue::from_str(&session).is_ok());
            assert_eq!(session, studio_session_id(&id));
            assert_ne!(session, id);
        }
        assert_ne!(studio_session_id("chat"), studio_session_id("chat "));
        assert_ne!(studio_session_id("chat"), studio_session_id("studio-chat"));
        assert_ne!(studio_session_id(""), studio_session_id("default"));
    }
    #[test]
    fn output_token_limit_requires_a_known_positive_integer_and_uses_the_completion_parameter() {
        for (parameters, expected) in [
            (json!({}), None),
            (json!({"max_tokens":8192}), Some(8192)),
            (
                json!({"max_tokens":1024,"max_completion_tokens":4096}),
                Some(4096),
            ),
            (json!({"max_tokens":0}), None),
            (json!({"max_tokens":-1}), None),
            (json!({"max_tokens":1.5}), None),
            (json!({"max_tokens":"8192"}), None),
            (
                json!({"max_tokens":8192,"max_completion_tokens":null}),
                None,
            ),
            (json!({"max_tokens":8192,"max_completion_tokens":0}), None),
        ] {
            assert_eq!(
                requested_output_token_limit(parameters.as_object().unwrap()),
                expected,
                "{parameters}"
            );
        }
    }

    #[test]
    fn fragmented_utf8_and_crlf_events_are_preserved() {
        let mut parser = SseParser::default();
        let bytes = "data: {\"delta\":\"Hello 🌍\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(parser.push(&[*byte]).unwrap());
        }
        assert_eq!(events, vec!["{\"delta\":\"Hello 🌍\"}", "[DONE]"]);
    }
    #[test]
    fn mixed_delimiters_in_one_chunk_keep_separate_events() {
        assert_eq!(
            SseParser::default()
                .push(b"data: one\r\n\r\ndata: two\n\n")
                .unwrap(),
            vec!["one", "two"]
        );
    }
}
