//! The language model, through OpenRouter.

use std::time::Duration;

use anyhow::{Result, anyhow};
use open_client::Error;
use open_client::chat::session::{Compactor, OverflowHandler, Session};
use open_client::chat::{AI, ChatCompletion};
use serde_json::{Value, json};

use crate::config::Config;

const MAX_ATTEMPTS: u32 = 4;

#[derive(Clone)]
pub struct Llm {
    client: AI,
    configured: bool,
}

impl Llm {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(170)).build()?;
        let mut client = AI::with_client(config.openrouter_api_key.as_str(), config.openrouter_model.as_str(), None, http);
        client
            .base_url(config.openrouter_api_base.as_str())
            // Without this, OpenRouter may pick a provider that silently ignores tools or the schema.
            .param("provider", json!({ "require_parameters": true }));
        Ok(Self { client, configured: !config.openrouter_api_key.is_empty() })
    }

    pub fn configured(&self) -> bool {
        self.configured
    }

    /// A copy of the configured client, ready for per-call settings.
    pub fn client(&self) -> AI {
        self.client.clone()
    }
}

/// A session for a single bounded request, so its history never needs summarizing.
pub fn session() -> Session {
    Session::new(Compactor::new(u64::MAX))
}

fn is_transient(error: &Error) -> bool {
    match error {
        Error::Api { status, error } => {
            // OpenRouter can report an upstream failure inside an HTTP 200, with the real code in the body.
            let code = error.code.as_ref().and_then(Value::as_u64).or(status.map(|status| u64::from(status.as_u16())));
            code.is_some_and(|code| code == 408 || code == 429 || code >= 500)
        }
        Error::NothingToRespond => false,
        _ => true,
    }
}

fn is_empty(completion: &ChatCompletion) -> bool {
    let message = completion.message();
    message.text().trim().is_empty() && message.tool_calls.is_empty()
}

/// `AI::ask` with retries for overloads, rate limits and empty replies. A completion
/// returned from here always has text or tool calls.
pub async fn ask<H: OverflowHandler>(ai: &AI, session: &mut Session<H>) -> Result<ChatCompletion> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let failure = match ai.ask(session).await {
            Ok(completion) if is_empty(&completion) => {
                session.pop();
                anyhow!("the model returned an empty reply")
            }
            Ok(completion) => return Ok(completion),
            Err(error) if !is_transient(&error) => return Err(error.into()),
            Err(error) => anyhow!(error),
        };
        if attempt >= MAX_ATTEMPTS {
            return Err(failure);
        }
        tracing::warn!(error = %failure, attempt, "retrying the model");
        tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
    }
}
