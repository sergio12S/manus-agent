//! Best-effort reporting of wallet activity into GBRAIN, our shared agent memory.
//! Reporting never blocks or fails a payment; GBRAIN being down is not an error.

use serde_json::json;

#[derive(Clone)]
pub struct Gbrain {
    http: reqwest::Client,
    url: Option<String>,
    api_key: Option<String>,
}

impl Gbrain {
    pub fn from_env() -> Self {
        let url = match std::env::var("MANUS_GBRAIN_URL") {
            Ok(value) if value == "off" || value.is_empty() => None,
            Ok(value) => Some(value),
            Err(_) => Some("http://127.0.0.1:3030/mcp".to_string()),
        };
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .expect("http client"),
            url,
            api_key: std::env::var("MANUS_GBRAIN_KEY")
                .ok()
                .or_else(local_token)
                .filter(|k| !k.is_empty()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            http: reqwest::Client::new(),
            url: None,
            api_key: None,
        }
    }

    /// Emit a signal. `topic` is e.g. `wallet.operation` or `wallet.approval`;
    /// `priority` follows GBRAIN's -10..=10 scale.
    pub fn emit(&self, topic: &str, content: String, priority: i8) {
        let Some(url) = self.url.clone() else {
            return;
        };
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let topic = topic.to_string();
        tokio::spawn(async move {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "gbrain_signal_emit",
                    "arguments": {
                        "topic": topic,
                        "content": content,
                        "priority": priority,
                        "tags": "manus,wallet"
                    }
                }
            });
            let mut request = http
                .post(url)
                .header("accept", "application/json, text/event-stream")
                .json(&body);
            if let Some(key) = api_key {
                request = request.header("X-API-Key", key);
            }
            let _ = request.send().await;
        });
    }
}

/// GBRAIN's per-machine token for local agents, the same one it writes into agent configs.
fn local_token() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".gemini-brain/local_token");
    std::fs::read_to_string(path)
        .ok()
        .map(|token| token.trim().to_string())
}
