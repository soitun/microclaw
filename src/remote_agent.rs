//! Generic remote agent engine.
//!
//! Sends a synchronous POST request to a configured endpoint and waits for
//! the agent's reply.  The protocol is intentionally simple:
//!
//! ```text
//! POST <url>
//! Authorization: Bearer <token>
//! X-Timeout-Secs: 600
//!
//! { "message": "...", "chat_id": "..." }
//! → { "ok": true, "response": "..." }
//! ```

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::agent_engine::{AgentEngine, AgentEvent, AgentRequestContext};
use crate::config::RemoteAgentConfig;
use crate::runtime::AppState;

#[derive(Deserialize)]
struct RemoteAgentResponse {
    ok: bool,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

pub struct RemoteAgentEngine {
    config: RemoteAgentConfig,
    client: Client,
}

impl RemoteAgentEngine {
    pub fn new(config: RemoteAgentConfig) -> Self {
        let timeout = Duration::from_secs(config.timeout_secs + 30); // client timeout > server timeout
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        Self { config, client }
    }
}

#[async_trait]
impl AgentEngine for RemoteAgentEngine {
    async fn process(
        &self,
        state: &AppState,
        context: AgentRequestContext<'_>,
        override_prompt: Option<&str>,
        image_data: Option<(String, String)>,
    ) -> anyhow::Result<String> {
        self.process_with_events(state, context, override_prompt, image_data, None)
            .await
    }

    async fn process_with_events(
        &self,
        state: &AppState,
        context: AgentRequestContext<'_>,
        override_prompt: Option<&str>,
        _image_data: Option<(String, String)>,
        event_tx: Option<&UnboundedSender<AgentEvent>>,
    ) -> anyhow::Result<String> {
        // 1. Get the user's latest message
        let chat_id = context.chat_id;
        let user_text = if let Some(prompt) = override_prompt {
            prompt.to_string()
        } else {
            let db = state.db.clone();
            let history =
                microclaw_storage::db::call_blocking(db, move |db| db.get_recent_messages(chat_id, 5))
                    .await?;
            history
                .into_iter()
                .rev()
                .find(|m| !m.is_from_bot)
                .map(|m| m.content)
                .unwrap_or_default()
        };

        if user_text.is_empty() {
            return Ok("(no message to send)".to_string());
        }

        info!(
            "Sending to remote agent (chat_id={}): {}",
            chat_id,
            &user_text[..user_text.len().min(100)]
        );

        // 2. POST to the remote agent endpoint
        let body = serde_json::json!({
            "message": user_text,
            "chat_id": chat_id.to_string(),
        });

        let resp = self
            .client
            .post(&self.config.url)
            .bearer_auth(&self.config.bearer_token)
            .header("X-Timeout-Secs", self.config.timeout_secs.to_string())
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let result: RemoteAgentResponse = resp.json().await?;

        if !result.ok {
            let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Remote agent error: {err}"));
        }

        let response = result
            .response
            .unwrap_or_else(|| "(empty response)".to_string());

        if let Some(tx) = event_tx {
            let _ = tx.send(AgentEvent::FinalResponse {
                text: response.clone(),
            });
        }

        Ok(response)
    }
}
