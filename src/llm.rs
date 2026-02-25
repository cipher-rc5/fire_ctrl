/// file: src/llm.rs
/// description: OpenAI-compatible LLM client and extraction/ranking utilities.
/// LLM client — OpenAI-compatible HTTP API (works with OpenAI, Groq, Ollama,
/// LMStudio, or any OpenAI-spec endpoint).
///
/// Uses direct HTTP calls against OpenAI-compatible chat-completion endpoints,
/// configured via `OPENAI_BASE_URL` + `OPENAI_API_KEY`.
use crate::config::LlmConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tracing::debug;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionRequest {
    pub content: String,
    pub prompt: String,
    pub schema: Option<serde_json::Value>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub data: serde_json::Value,
    pub metadata: ExtractionMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

struct LlmCompletion {
    text: String,
    tokens: Option<u32>,
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionMetadata {
    pub model: String,
    pub tokens_used: Option<u32>,
    pub processing_time_ms: u64,
}

// ---------------------------------------------------------------------------
// LLM client
// ---------------------------------------------------------------------------

pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> Result<Self> {
        if let Some(model) = &cfg.embedding_model {
            debug!(embedding_model = %model, "Embedding model configured");
        }

        Ok(Self {
            http: reqwest::Client::new(),
            config: cfg.clone(),
        })
    }

    pub async fn extract(&self, req: &ExtractionRequest) -> Result<ExtractionResult> {
        let start = Instant::now();

        let system_prompt = req.system_prompt.clone().unwrap_or_else(|| {
            if let Some(schema) = &req.schema {
                format!(
                    "You are a data extraction assistant. Extract information from the provided \
                     content according to this JSON schema:\n{}\n\nReturn ONLY valid JSON \
                     that matches the schema, with no markdown fences or prose.",
                    serde_json::to_string_pretty(schema).unwrap_or_default()
                )
            } else {
                "You are a data extraction assistant. Extract the requested information \
                 from the provided content and return it as valid JSON only, with no \
                 markdown fences or additional prose."
                    .to_string()
            }
        });

        let extraction_prompt = if req.prompt.trim().is_empty() {
            "Extract data that matches the schema. If the source is very long, return a representative subset that still matches the schema exactly.".to_string()
        } else {
            req.prompt.clone()
        };

        let mut last_err = anyhow!("LLM extraction failed");

        for attempt in 0..self.config.max_retries {
            let content = if attempt == 0 {
                req.content.clone()
            } else {
                let max_chars = 24_000usize
                    .saturating_div((attempt as usize) + 1)
                    .max(6_000);
                req.content.chars().take(max_chars).collect::<String>()
            };

            let user_msg = format!(
                "Content:\n{}\n\nExtraction Request:\n{}",
                content, extraction_prompt
            );

            match tokio::time::timeout(
                Duration::from_secs(self.config.timeout_seconds),
                self.chat_complete(&system_prompt, &user_msg),
            )
            .await
            {
                Ok(Ok(completion)) => {
                    let json_str = self.extract_json(&completion.text)?;
                    let data: serde_json::Value = serde_json::from_str(&json_str)
                        .context("Failed to parse LLM response as JSON")?;

                    return Ok(ExtractionResult {
                        data,
                        metadata: ExtractionMetadata {
                            model: self.config.model_name.clone(),
                            tokens_used: completion.tokens,
                            processing_time_ms: start.elapsed().as_millis() as u64,
                        },
                        warning: completion.warning,
                    });
                }
                Ok(Err(e)) => {
                    last_err = anyhow!("LLM request failed: {e}");
                    debug!(attempt, error = %last_err, "LLM attempt failed");
                }
                Err(_) => {
                    last_err = anyhow!("LLM request timed out");
                }
            }

            if attempt + 1 < self.config.max_retries {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
        }

        Err(last_err)
    }

    async fn chat_complete(&self, system: &str, user: &str) -> Result<LlmCompletion> {
        self.raw_chat_complete(system, user).await
    }

    async fn raw_chat_complete(&self, system: &str, user: &str) -> Result<LlmCompletion> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if !self.config.api_key.is_empty() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.config.api_key))?,
            );
        }

        let body_with_reasoning = serde_json::json!({
            "model": self.config.model_name,
            "temperature": 0.1,
            "reasoning_effort": "none",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ]
        });

        let body_plain = serde_json::json!({
            "model": self.config.model_name,
            "temperature": 0.1,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ]
        });

        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        if self.config.base_url.contains("api.groq.com") {
            let resp: serde_json::Value = self
                .http
                .post(url)
                .headers(headers)
                .json(&body_plain)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await?;

            let text = resp
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| anyhow!("LLM returned empty response"))?
                .to_string();

            let tokens = resp
                .get("usage")
                .and_then(|u| u.get("total_tokens"))
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);

            return Ok(LlmCompletion {
                text,
                tokens,
                warning: None,
            });
        }

        let mut warning: Option<String> = None;
        let resp: serde_json::Value = match self
            .http
            .post(url.clone())
            .headers(headers.clone())
            .json(&body_with_reasoning)
            .send()
            .await
        {
            Ok(r) => match r.error_for_status() {
                Ok(ok) => ok.json::<serde_json::Value>().await?,
                Err(e) => {
                    debug!(error = %e, "Raw chat request with reasoning_effort failed, retrying without it");
                    warning = Some(
                        "Provider rejected optional reasoning tuning parameter; request retried without it."
                            .to_string(),
                    );
                    self.http
                        .post(url)
                        .headers(headers)
                        .json(&body_plain)
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<serde_json::Value>()
                        .await?
                }
            },
            Err(e) => {
                debug!(error = %e, "Raw chat request with reasoning_effort failed, retrying without it");
                warning = Some(
                    "Provider rejected optional reasoning tuning parameter; request retried without it."
                        .to_string(),
                );
                self.http
                    .post(url)
                    .headers(headers)
                    .json(&body_plain)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<serde_json::Value>()
                    .await?
            }
        };

        let text = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("LLM returned empty response"))?
            .to_string();

        let tokens = resp
            .get("usage")
            .and_then(|u| u.get("total_tokens"))
            .and_then(|t| t.as_u64())
            .map(|t| t as u32);

        Ok(LlmCompletion {
            text,
            tokens,
            warning,
        })
    }

    fn extract_json(&self, content: &str) -> Result<String> {
        let trimmed = content.trim();

        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return Ok(trimmed.to_string());
        }

        // ```json … ``` fence
        if let Some(start) = trimmed.find("```json") {
            let after = &trimmed[start + 7..];
            if let Some(end) = after.find("```") {
                return Ok(after[..end].trim().to_string());
            }
        }

        // Bare ``` fence
        if let Some(start) = trimmed.find("```") {
            let after = &trimmed[start + 3..];
            if let Some(end) = after.find("```") {
                let inner = after[..end].trim();
                if inner.starts_with('{') || inner.starts_with('[') {
                    return Ok(inner.to_string());
                }
            }
        }

        // Embedded object
        if let (Some(s), Some(e)) = (trimmed.find('{'), trimmed.rfind('}')) {
            return Ok(trimmed[s..=e].to_string());
        }

        // Embedded array
        if let (Some(s), Some(e)) = (trimmed.find('['), trimmed.rfind(']')) {
            return Ok(trimmed[s..=e].to_string());
        }

        bail!("Could not extract JSON from LLM response")
    }

    pub async fn health_check(&self) -> Result<()> {
        let req = ExtractionRequest {
            content: "ping".to_string(),
            prompt: r#"Return {"status":"ok"}"#.to_string(),
            schema: None,
            system_prompt: None,
        };
        self.extract(&req).await.map(|_| ())
    }

    pub fn max_input_chars(&self) -> usize {
        self.config.max_input_chars
    }
}

// ---------------------------------------------------------------------------
// Search result re-ranking (simple keyword relevance, no embedding needed)
// ---------------------------------------------------------------------------

pub fn rank_by_relevance(items: Vec<String>, query: &str) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().collect();
    let mut scored: Vec<(usize, String)> = items
        .into_iter()
        .map(|s| {
            let lower = s.to_lowercase();
            let score = terms.iter().filter(|t| lower.contains(*t)).count();
            (score, s)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, s)| s).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmConfig;

    fn test_client() -> LlmClient {
        let cfg = LlmConfig {
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: "test".to_string(),
            model_name: "llama3".to_string(),
            embedding_model: None,
            ollama_base_url: None,
            timeout_seconds: 10,
            max_retries: 1,
            max_input_chars: 24_000,
        };
        LlmClient::new(&cfg).unwrap()
    }

    #[test]
    fn extract_json_bare() {
        let c = test_client();
        let r = c.extract_json(r#"{"key":"value"}"#).unwrap();
        assert_eq!(r, r#"{"key":"value"}"#);
    }

    #[test]
    fn extract_json_fenced() {
        let c = test_client();
        let r = c.extract_json("```json\n{\"key\":\"value\"}\n```").unwrap();
        assert_eq!(r, r#"{"key":"value"}"#);
    }

    #[test]
    fn extract_json_embedded() {
        let c = test_client();
        let r = c
            .extract_json(r#"Here is the answer: {"key":"value"} done."#)
            .unwrap();
        assert_eq!(r, r#"{"key":"value"}"#);
    }
}
