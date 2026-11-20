//! Where Vox's "brain" runs: which LLM turns your voice into a tool call and a
//! spoken sentence.
//!
//! Three shapes, because the providers genuinely differ:
//!
//!  * **OpenAI-compatible HTTP** (Ollama, OpenAI) — streams SSE deltas and has a
//!    native `tools` API. This is the fast path and the only one that can speak
//!    while the model is still generating.
//!  * **Anthropic Messages API** — a different request/response shape entirely
//!    (`/v1/messages`, `x-api-key`, content blocks). Not an OpenAI shim.
//!  * **Local CLI subprocess** (`claude --print`, `codex exec`) — uses your
//!    SUBSCRIPTION, no API key. No tools API, so these ride the JSON-object
//!    prompt path that already exists for weak models.
//!
//! Latency is the honest trade-off, measured on this machine with a one-line
//! prompt: Ollama small model ≈ 0.5-1s, `claude --print` ≈ 6.6s, `codex exec`
//! ≈ 5s. A CLI provider is a real answer-quality upgrade and a real
//! conversational-feel downgrade; `provider_note` surfaces that in the UI.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// How a provider must be talked to.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Transport {
    /// Streaming SSE + native `tools`.
    OpenAiCompat,
    /// Anthropic `/v1/messages`.
    AnthropicApi,
    /// Subprocess; JSON-object prompt, no streaming, no tools API.
    Cli,
}

#[derive(Clone, Debug)]
pub struct Brain {
    pub id: &'static str,
    pub transport: Transport,
    pub url: &'static str,
    /// Env var holding the key. Empty for local providers.
    pub key_env: &'static str,
    /// Binary to run for `Transport::Cli`.
    pub bin: &'static str,
    pub model: String,
}

pub const PROVIDERS: &[&str] = &["ollama", "claude-cli", "codex-cli", "anthropic", "openai"];

/// Sensible default model per provider, used when the user hasn't picked one.
pub fn default_model(provider: &str) -> &'static str {
    match provider {
        // Small and fast: the brain runs on every single voice turn.
        "anthropic" => "claude-haiku-4-5",
        "openai" => "gpt-4o-mini",
        // The CLIs use whatever model they're configured with; the field is
        // informational there.
        "claude-cli" => "subscription",
        "codex-cli" => "subscription",
        _ => "qwen2.5:3b",
    }
}

/// Is this model id meaningful for this provider?
///
/// Model ids do not transfer: "qwen3:8b" means nothing to Anthropic, and
/// "claude-haiku-4-5" means nothing to Ollama. Without this check the two
/// settings drift apart — set the provider by env var and the persisted model
/// stays behind, so the UI shows a Claude provider next to an Ollama model and
/// every request fails.
pub fn model_fits(provider: &str, model: &str) -> bool {
    let m = model.trim();
    if m.is_empty() {
        return false;
    }
    match provider {
        // The CLI uses whatever account it is signed into; there is nothing to
        // choose, so only the placeholder is coherent.
        "claude-cli" | "codex-cli" => m == "subscription",
        "anthropic" => m.starts_with("claude-"),
        "openai" => m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3"),
        // Ollama model names are arbitrary, but a foreign id is still wrong.
        "ollama" => !m.starts_with("claude-") && !m.starts_with("gpt-") && m != "subscription",
        _ => true,
    }
}

pub fn resolve(provider: &str, model: &str) -> Brain {
    let model = if model.trim().is_empty() {
        default_model(provider).to_string()
    } else {
        model.to_string()
    };
    match provider {
        "anthropic" => Brain {
            id: "anthropic",
            transport: Transport::AnthropicApi,
            url: "https://api.anthropic.com/v1/messages",
            key_env: "ANTHROPIC_API_KEY",
            bin: "",
            model,
        },
        "openai" => Brain {
            id: "openai",
            transport: Transport::OpenAiCompat,
            url: "https://api.openai.com/v1/chat/completions",
            key_env: "OPENAI_API_KEY",
            bin: "",
            model,
        },
        "claude-cli" => Brain {
            id: "claude-cli",
            transport: Transport::Cli,
            url: "",
            key_env: "",
            bin: "claude",
            model,
        },
        "codex-cli" => Brain {
            id: "codex-cli",
            transport: Transport::Cli,
            url: "",
            key_env: "",
            bin: "codex",
            model,
        },
        _ => Brain {
            id: "ollama",
            transport: Transport::OpenAiCompat,
            url: "http://localhost:11434/v1/chat/completions",
            key_env: "",
            bin: "",
            model,
        },
    }
}

impl Brain {
    pub fn api_key(&self) -> Option<String> {
        if self.key_env.is_empty() {
            return None;
        }
        std::env::var(self.key_env).ok().filter(|k| !k.trim().is_empty())
    }

    /// Why this provider can't be used right now, if it can't. Checked before a
    /// turn so the failure is spoken rather than silent.
    pub fn unavailable(&self) -> Option<String> {
        match self.transport {
            Transport::Cli => (crate::daemons::find_bin(self.bin, &[]).is_none())
                .then(|| format!("{} is not installed", self.bin)),
            _ if !self.key_env.is_empty() && self.api_key().is_none() => {
                Some(format!("{} is not set", self.key_env))
            }
            _ => None,
        }
    }

    /// One-shot completion, no tools, no streaming. Every transport supports
    /// this; it backs the recap and the announcement summaries.
    pub fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Option<String> {
        match self.transport {
            Transport::Cli => self.run_cli(system, user),
            Transport::AnthropicApi => {
                let body = json!({
                    "model": self.model,
                    "max_tokens": max_tokens,
                    "system": system,
                    "messages": [{ "role": "user", "content": user }],
                });
                let v = self.post(&body, 90).ok()?;
                // Content is a list of blocks; take the text ones.
                let text: String = v
                    .get("content")?
                    .as_array()?
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(text.trim().to_string()).filter(|t| !t.is_empty())
            }
            Transport::OpenAiCompat => {
                let body = json!({
                    "model": self.model,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user }
                    ],
                    "max_tokens": max_tokens,
                    "stream": false
                });
                let v = self.post(&body, 90).ok()?;
                let t = v["choices"][0]["message"]["content"].as_str()?.trim().to_string();
                Some(t).filter(|t| !t.is_empty())
            }
        }
    }

    pub fn post(&self, body: &Value, timeout_secs: u64) -> Result<Value, String> {
        let mut req = ureq::post(self.url).timeout(Duration::from_secs(timeout_secs));
        if let Some(k) = self.api_key() {
            req = match self.transport {
                // Anthropic authenticates with x-api-key, not a Bearer token,
                // and pins the API version per request.
                Transport::AnthropicApi => req
                    .set("x-api-key", &k)
                    .set("anthropic-version", "2023-06-01"),
                _ => req.set("Authorization", &format!("Bearer {k}")),
            };
        }
        req.send_json(body.clone())
            .map_err(|e| e.to_string())?
            .into_json::<Value>()
            .map_err(|e| e.to_string())
    }

    /// Run the subscription CLI headlessly. Slow (seconds), so it is only ever
    /// used off the streaming path.
    fn run_cli(&self, system: &str, user: &str) -> Option<String> {
        let bin = crate::daemons::find_bin(self.bin, &[])?;
        let prompt = format!("{system}\n\n---\n\n{user}");
        let out = match self.bin {
            // `codex exec` refuses to run outside a git repo without this, and
            // Vox's cwd is wherever the app was launched from.
            "codex" => Command::new(&bin)
                .args(["exec", "--skip-git-repo-check", &prompt])
                .stdin(Stdio::null())
                .output(),
            // `--` so a prompt starting with '-' isn't parsed as a flag.
            _ => Command::new(&bin)
                .args(["--print", "--", &prompt])
                .stdin(Stdio::null())
                .output(),
        }
        .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            eprintln!("[vox] {} produced nothing: {}", self.bin, err.chars().take(200).collect::<String>());
            return None;
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_provider_resolves() {
        for p in PROVIDERS {
            let b = resolve(p, "");
            assert_eq!(&b.id, p, "provider {p} must resolve to itself");
            assert!(!b.model.is_empty(), "{p} needs a default model");
        }
    }

    /// An unknown value must fall back to local, never to a paid endpoint.
    #[test]
    fn unknown_provider_falls_back_to_local() {
        let b = resolve("nonsense", "");
        assert_eq!(b.id, "ollama");
        assert!(b.url.contains("localhost"));
        assert!(b.key_env.is_empty());
    }

    #[test]
    fn api_providers_declare_their_key_and_cli_providers_declare_a_binary() {
        assert_eq!(resolve("anthropic", "").key_env, "ANTHROPIC_API_KEY");
        assert_eq!(resolve("openai", "").key_env, "OPENAI_API_KEY");
        assert_eq!(resolve("claude-cli", "").bin, "claude");
        assert_eq!(resolve("codex-cli", "").bin, "codex");
    }

    #[test]
    fn model_ids_do_not_transfer_between_providers() {
        assert!(!model_fits("anthropic", "qwen3:8b"));
        assert!(!model_fits("ollama", "claude-haiku-4-5"));
        assert!(!model_fits("ollama", "subscription"));
        assert!(!model_fits("claude-cli", "qwen3:8b"));
        assert!(model_fits("claude-cli", "subscription"));
        assert!(model_fits("anthropic", "claude-haiku-4-5"));
        assert!(model_fits("openai", "gpt-4o-mini"));
        assert!(model_fits("ollama", "qwen2.5:3b"));
    }

    /// Every provider's own default must satisfy its own check, or settings
    /// would be reset on every single load.
    #[test]
    fn defaults_are_self_consistent() {
        for p in PROVIDERS {
            assert!(model_fits(p, default_model(p)), "{p} default must fit {p}");
        }
    }

    /// The brain runs on every voice turn — defaults must be the small ones.
    #[test]
    fn defaults_are_small_fast_models() {
        assert_eq!(default_model("anthropic"), "claude-haiku-4-5");
        assert_eq!(default_model("ollama"), "qwen2.5:3b");
    }

    /// Anthropic is NOT an OpenAI-compatible shim.
    #[test]
    fn anthropic_uses_its_own_messages_endpoint() {
        let b = resolve("anthropic", "");
        assert!(b.url.ends_with("/v1/messages"));
        assert_eq!(b.transport, Transport::AnthropicApi);
    }
}
