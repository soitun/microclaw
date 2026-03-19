//! Agent engine that proxies requests to a remote hapi hub.
//!
//! Instead of running the built-in LLM tool loop, this engine:
//! 1. Authenticates with the hapi hub to get a JWT
//! 2. Finds (or waits for) an active session
//! 3. Sends the user message via POST /api/sessions/:id/messages
//! 4. Listens on SSE /api/events for the agent's response
//! 5. Optionally auto-approves permission requests

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::agent_engine::{AgentEngine, AgentEvent, AgentRequestContext};
use crate::config::HapiEngineConfig;
use crate::runtime::AppState;

/// JWT + expiry cache so we don't re-auth on every message.
struct AuthCache {
    token: String,
    /// We refresh proactively before the 15-min expiry.
    obtained_at: std::time::Instant,
}

pub struct HapiAgentEngine {
    config: HapiEngineConfig,
    client: Client,
    auth: RwLock<Option<AuthCache>>,
    /// Cache: microclaw chat_id -> hapi session_id
    session_map: RwLock<HashMap<i64, String>>,
}

// ── Hapi API types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthResponse {
    token: String,
}

#[derive(Deserialize)]
struct SessionsResponse {
    sessions: Vec<HapiSession>,
}

#[derive(Deserialize)]
struct HapiSession {
    id: String,
    active: bool,
}

#[derive(Deserialize)]
struct SessionResponse {
    session: HapiSessionDetail,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HapiSessionDetail {
    active: bool,
    #[serde(default)]
    agent_state: Option<HapiAgentState>,
}

#[derive(Deserialize)]
struct HapiAgentState {
    #[serde(default)]
    requests: Vec<HapiPermissionRequest>,
}

#[derive(Deserialize)]
struct HapiPermissionRequest {
    id: String,
}

#[derive(Serialize)]
struct SendMessageBody {
    text: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    messages: Vec<HapiMessage>,
}

#[derive(Deserialize, Clone)]
struct HapiMessage {
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    content: String,
    #[serde(default)]
    role: String,
}

// ── SSE event types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: serde_json::Value,
}

// ── Implementation ──────────────────────────────────────────────────────────

impl HapiAgentEngine {
    pub fn new(config: HapiEngineConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build reqwest client");
        Self {
            config,
            client,
            auth: RwLock::new(None),
            session_map: RwLock::new(HashMap::new()),
        }
    }

    fn base_url(&self) -> &str {
        self.config.url.trim_end_matches('/')
    }

    /// Get a valid JWT, refreshing if needed.
    async fn get_token(&self) -> anyhow::Result<String> {
        {
            let cache = self.auth.read().await;
            if let Some(ref c) = *cache {
                // Refresh if older than 12 minutes (JWT expires at 15)
                if c.obtained_at.elapsed() < Duration::from_secs(720) {
                    return Ok(c.token.clone());
                }
            }
        }

        let url = format!("{}/api/auth", self.base_url());
        let body = serde_json::json!({ "accessToken": self.config.access_token });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let auth_resp: AuthResponse = resp.json().await?;

        let mut cache = self.auth.write().await;
        *cache = Some(AuthCache {
            token: auth_resp.token.clone(),
            obtained_at: std::time::Instant::now(),
        });
        Ok(auth_resp.token)
    }

    /// Find the first active session on the hub.
    async fn find_active_session(&self, token: &str) -> anyhow::Result<Option<String>> {
        let url = format!("{}/api/sessions", self.base_url());
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        let sessions: SessionsResponse = resp.json().await?;
        Ok(sessions
            .sessions
            .into_iter()
            .find(|s| s.active)
            .map(|s| s.id))
    }

    /// Get session detail (for checking permission requests).
    async fn get_session(&self, token: &str, session_id: &str) -> anyhow::Result<HapiSessionDetail> {
        let url = format!("{}/api/sessions/{}", self.base_url(), session_id);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        let sr: SessionResponse = resp.json().await?;
        Ok(sr.session)
    }

    /// Send a user message to a hapi session.
    async fn send_message(
        &self,
        token: &str,
        session_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{}/api/sessions/{}/messages", self.base_url(), session_id);
        self.client
            .post(&url)
            .bearer_auth(token)
            .json(&SendMessageBody {
                text: text.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Approve a permission request.
    async fn approve_permission(
        &self,
        token: &str,
        session_id: &str,
        request_id: &str,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/sessions/{}/permissions/{}/approve",
            self.base_url(),
            session_id,
            request_id
        );
        let body = serde_json::json!({ "decision": "approved" });
        self.client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Get the latest assistant message after a given sequence number.
    async fn get_messages_after(
        &self,
        token: &str,
        session_id: &str,
        after_seq: u64,
    ) -> anyhow::Result<Vec<HapiMessage>> {
        // Use the web API with reverse pagination — get recent messages
        let url = format!(
            "{}/api/sessions/{}/messages?limit=50",
            self.base_url(),
            session_id
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        let mr: MessagesResponse = resp.json().await?;
        Ok(mr
            .messages
            .into_iter()
            .filter(|m| m.seq > after_seq)
            .collect())
    }

    /// Wait for the agent to produce a response by polling SSE events.
    /// Returns the final assistant message text.
    async fn wait_for_response(
        &self,
        token: &str,
        session_id: &str,
        event_tx: Option<&UnboundedSender<AgentEvent>>,
    ) -> anyhow::Result<String> {
        // Get current message seq as baseline
        let messages = self.get_messages_after(token, session_id, 0).await?;
        let baseline_seq = messages.iter().map(|m| m.seq).max().unwrap_or(0);

        // Poll via SSE with a timeout
        let sse_url = format!(
            "{}/api/events?sessionId={}&visibility=hidden",
            self.base_url(),
            session_id
        );

        let resp = self
            .client
            .get(&sse_url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;

        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;

        let mut buf = String::new();
        let timeout = Duration::from_secs(600); // 10 minute max wait
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(anyhow::anyhow!("Timed out waiting for hapi response"));
                }
            };

            match chunk {
                Some(Ok(bytes)) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));

                    // Process complete SSE lines
                    while let Some(pos) = buf.find("\n\n") {
                        let event_block = buf[..pos].to_string();
                        buf = buf[pos + 2..].to_string();

                        // Parse SSE data lines
                        let data = event_block
                            .lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .collect::<Vec<_>>()
                            .join("");

                        if data.is_empty() {
                            continue;
                        }

                        if let Ok(evt) = serde_json::from_str::<SseEvent>(&data) {
                            match evt.event_type.as_str() {
                                "message-received" => {
                                    // Check if this is an assistant message after our baseline
                                    if let Some(msg) = evt.data.get("message") {
                                        let seq = msg
                                            .get("seq")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        let content = msg
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let role = msg
                                            .get("role")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");

                                        // hapi messages from the agent have role "assistant"
                                        // but the field might also be absent; any message
                                        // after baseline that isn't "user" is the response
                                        if seq > baseline_seq && role != "user" && !content.is_empty() {
                                            if let Some(tx) = event_tx {
                                                let _ = tx.send(AgentEvent::FinalResponse {
                                                    text: content.to_string(),
                                                });
                                            }
                                            return Ok(content.to_string());
                                        }
                                    }
                                }
                                "session-updated" => {
                                    // Check for permission requests and auto-approve
                                    if self.config.auto_approve {
                                        if let Err(e) =
                                            self.check_and_approve_permissions(token, session_id).await
                                        {
                                            warn!("Failed to auto-approve permission: {e}");
                                        }
                                    }
                                }
                                "heartbeat" => {
                                    debug!("hapi SSE heartbeat");
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(anyhow::anyhow!("SSE stream error: {e}"));
                }
                None => {
                    // Stream ended — fall back to polling for the response
                    debug!("SSE stream ended, falling back to polling");
                    return self
                        .poll_for_response(token, session_id, baseline_seq)
                        .await;
                }
            }
        }
    }

    /// Fallback: poll messages endpoint until we see an assistant reply.
    async fn poll_for_response(
        &self,
        token: &str,
        session_id: &str,
        baseline_seq: u64,
    ) -> anyhow::Result<String> {
        for _ in 0..120 {
            // up to 2 minutes
            tokio::time::sleep(Duration::from_secs(1)).await;

            if self.config.auto_approve {
                let _ = self.check_and_approve_permissions(token, session_id).await;
            }

            let messages = self.get_messages_after(token, session_id, baseline_seq).await?;
            if let Some(msg) = messages
                .iter()
                .find(|m| m.role != "user" && !m.content.is_empty())
            {
                return Ok(msg.content.clone());
            }
        }
        Err(anyhow::anyhow!(
            "Timed out polling for hapi agent response"
        ))
    }

    /// Check for pending permission requests and approve them.
    async fn check_and_approve_permissions(
        &self,
        token: &str,
        session_id: &str,
    ) -> anyhow::Result<()> {
        let session = self.get_session(token, session_id).await?;
        if let Some(agent_state) = session.agent_state {
            for req in agent_state.requests {
                info!("Auto-approving hapi permission request: {}", req.id);
                self.approve_permission(token, session_id, &req.id).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl AgentEngine for HapiAgentEngine {
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
        // 1. Get the user's latest message from microclaw's DB
        let chat_id = context.chat_id;
        let db = state.db.clone();
        let user_text = if let Some(prompt) = override_prompt {
            prompt.to_string()
        } else {
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

        // 2. Authenticate
        let token = self.get_token().await?;

        // 3. Find an active session (or report error)
        let session_id = {
            // Check cache first
            let map = self.session_map.read().await;
            if let Some(sid) = map.get(&context.chat_id) {
                Some(sid.clone())
            } else {
                None
            }
        };

        let session_id = match session_id {
            Some(sid) => {
                // Verify it's still active
                match self.get_session(&token, &sid).await {
                    Ok(s) if s.active => sid,
                    _ => {
                        // Stale cache, find a new one
                        self.session_map.write().await.remove(&context.chat_id);
                        self.find_active_session(&token)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("No active hapi session found. Start a hapi session first (e.g. `hapi` in your terminal)."))?
                    }
                }
            }
            None => {
                let sid = self
                    .find_active_session(&token)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("No active hapi session found. Start a hapi session first (e.g. `hapi` in your terminal)."))?;
                self.session_map
                    .write()
                    .await
                    .insert(context.chat_id, sid.clone());
                sid
            }
        };

        info!(
            "Sending message to hapi session {} (chat_id={}): {}",
            session_id,
            context.chat_id,
            &user_text[..user_text.len().min(100)]
        );

        // 4. Send the message
        self.send_message(&token, &session_id, &user_text).await?;

        // 5. Wait for the response via SSE
        let response = self.wait_for_response(&token, &session_id, event_tx).await?;

        Ok(response)
    }
}
