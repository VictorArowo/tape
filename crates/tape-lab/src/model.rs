//! Talking to a language model, and paying for it.
//!
//! [`Model`] is the only thing the research desk needs from a provider:
//! send a system prompt and a user message, get back JSON that matches a
//! schema, and report what it cost. [`OpenAi`] implements it over the
//! Responses API; [`Scripted`] replays canned answers for tests and dry
//! runs.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// One request: instructions, the task, and the shape the answer must take.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub system: String,
    pub user: String,
    pub schema_name: &'static str,
    /// A JSON Schema in the strict subset: every object sets
    /// `additionalProperties: false` and lists all its fields as required.
    pub schema: Value,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// Included in `output_tokens` and billed as output.
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub json: Value,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("the spending cap of ${cap:.2} would be exceeded (spent ${spent:.4})")]
    BudgetExhausted { cap: f64, spent: f64 },
    #[error(
        "no OpenAI API key: set OPENAI_API_KEY, or add OPENAI_API_KEY=... to a .env file in the project root"
    )]
    MissingKey,
    #[error("model API returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("couldn't reach the model API: {0}")]
    Transport(String),
    #[error("response was cut off ({0})")]
    Incomplete(String),
    #[error("the model declined: {0}")]
    Refusal(String),
    #[error("unexpected response: {0}")]
    Malformed(String),
}

pub trait Model: Sync {
    fn name(&self) -> &str;

    /// The most a prompt could cost, used to check the budget before
    /// sending it.
    fn worst_case_cost(&self, prompt: &Prompt) -> f64;

    fn complete(&self, prompt: &Prompt) -> Result<Completion, ModelError>;
}

/// Prices in US dollars per million tokens.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

impl Pricing {
    /// Published standard-tier prices for models the lab knows.
    pub fn for_model(model: &str) -> Option<Self> {
        match model {
            // developers.openai.com/api/docs/models/gpt-6-luna, September 2026.
            "gpt-6-luna" => Some(Self {
                input: 0.10,
                cached_input: 0.01,
                output: 0.50,
            }),
            // developers.openai.com/api/docs/models/gpt-6-sol, September 2026.
            "gpt-6-sol" => Some(Self {
                input: 2.00,
                cached_input: 0.20,
                output: 10.00,
            }),
            _ => None,
        }
    }

    pub fn cost(&self, usage: &Usage) -> f64 {
        let uncached = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
        (uncached as f64 * self.input
            + usage.cached_input_tokens as f64 * self.cached_input
            + usage.output_tokens as f64 * self.output)
            / 1e6
    }
}

/// A hard spending cap shared by every call in a run, including calls
/// made at the same time from different threads.
pub struct Budget {
    cap: f64,
    /// Money spent, and money reserved for calls in flight.
    state: Mutex<(f64, f64)>,
}

impl Budget {
    pub fn new(cap_usd: f64) -> Self {
        Self {
            cap: cap_usd,
            state: Mutex::new((0.0, 0.0)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (f64, f64)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn spent(&self) -> f64 {
        self.lock().0
    }

    pub fn cap(&self) -> f64 {
        self.cap
    }

    /// Sends `prompt` if its worst-case cost fits in what's left after
    /// other calls' reservations, and records what it actually cost.
    pub fn complete(&self, model: &dyn Model, prompt: &Prompt) -> Result<Completion, ModelError> {
        let reserve = model.worst_case_cost(prompt);
        {
            let mut state = self.lock();
            let (spent, reserved) = *state;
            if spent + reserved + reserve > self.cap {
                return Err(ModelError::BudgetExhausted {
                    cap: self.cap,
                    spent,
                });
            }
            state.1 += reserve;
        }
        let result = model.complete(prompt);
        let mut state = self.lock();
        state.1 -= reserve;
        if let Ok(completion) = &result {
            state.0 += completion.cost_usd;
        }
        result
    }
}

/// An OpenAI model over the Responses API, with structured outputs.
pub struct OpenAi {
    agent: ureq::Agent,
    api_key: String,
    model: String,
    effort: String,
    pricing: Pricing,
    max_output_tokens: u64,
}

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

/// Reads `OPENAI_API_KEY` from the environment, or from a `.env` file.
fn api_key(project_root: &Path) -> Option<String> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().to_owned());
    }
    let dotenv = fs::read_to_string(project_root.join(".env")).ok()?;
    dotenv.lines().find_map(|line| {
        let value = line.trim().strip_prefix("OPENAI_API_KEY=")?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_owned())
    })
}

impl OpenAi {
    pub fn new(project_root: &Path, model: &str, effort: &str) -> Result<Self, ModelError> {
        let pricing = Pricing::for_model(model).ok_or_else(|| {
            ModelError::Malformed(format!(
                "no pricing known for {model}; add it to Pricing::for_model"
            ))
        })?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Ok(Self {
            agent,
            api_key: api_key(project_root).ok_or(ModelError::MissingKey)?,
            model: model.to_owned(),
            effort: effort.to_owned(),
            pricing,
            max_output_tokens: 32_000,
        })
    }

    fn send(&self, body: &Value) -> Result<Value, ModelError> {
        let mut delay = Duration::from_secs(2);
        for attempt in 0..4 {
            let response = self
                .agent
                .post(RESPONSES_URL)
                .header("Authorization", &format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .send_json(body);
            let mut response = match response {
                Ok(response) => response,
                Err(err) if attempt < 3 => {
                    let _ = err;
                    std::thread::sleep(delay);
                    delay *= 2;
                    continue;
                }
                Err(err) => return Err(ModelError::Transport(err.to_string())),
            };
            let status = response.status().as_u16();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|err| ModelError::Transport(err.to_string()))?;
            if status == 200 {
                return serde_json::from_str(&text)
                    .map_err(|err| ModelError::Malformed(err.to_string()));
            }
            let retryable = status == 429 || status >= 500;
            if retryable && attempt < 3 {
                std::thread::sleep(delay);
                delay *= 2;
                continue;
            }
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(ModelError::Http { status, message });
        }
        unreachable!("the loop returns on its last attempt")
    }
}

/// Pulls the answer text and usage out of a Responses API response.
pub(crate) fn parse_response(response: &Value) -> Result<(String, Usage), ModelError> {
    if response["status"] == "incomplete" {
        let reason = response["incomplete_details"]["reason"]
            .as_str()
            .unwrap_or("unknown reason");
        return Err(ModelError::Incomplete(reason.to_owned()));
    }
    let mut text = None;
    for item in response["output"].as_array().into_iter().flatten() {
        if item["type"] != "message" {
            continue;
        }
        for part in item["content"].as_array().into_iter().flatten() {
            match part["type"].as_str() {
                Some("output_text") => text = part["text"].as_str().map(str::to_owned),
                Some("refusal") => {
                    return Err(ModelError::Refusal(
                        part["refusal"].as_str().unwrap_or_default().to_owned(),
                    ));
                }
                _ => {}
            }
        }
    }
    let usage = &response["usage"];
    let count = |v: &Value| v.as_u64().unwrap_or(0);
    let usage = Usage {
        input_tokens: count(&usage["input_tokens"]),
        cached_input_tokens: count(&usage["input_tokens_details"]["cached_tokens"]),
        output_tokens: count(&usage["output_tokens"]),
        reasoning_tokens: count(&usage["output_tokens_details"]["reasoning_tokens"]),
    };
    let text =
        text.ok_or_else(|| ModelError::Malformed("no output_text in the response".into()))?;
    Ok((text, usage))
}

impl Model for OpenAi {
    fn name(&self) -> &str {
        &self.model
    }

    fn worst_case_cost(&self, prompt: &Prompt) -> f64 {
        // About four characters per token; assume three to stay safe.
        let input_tokens = (prompt.system.len() + prompt.user.len()) as f64 / 3.0 + 500.0;
        (input_tokens * self.pricing.input + self.max_output_tokens as f64 * self.pricing.output)
            / 1e6
    }

    fn complete(&self, prompt: &Prompt) -> Result<Completion, ModelError> {
        let body = json!({
            "model": self.model,
            "input": [
                {"role": "system", "content": prompt.system},
                {"role": "user", "content": prompt.user},
            ],
            "reasoning": {"effort": self.effort},
            "text": {"format": {
                "type": "json_schema",
                "name": prompt.schema_name,
                "schema": prompt.schema,
                "strict": true,
            }},
            "max_output_tokens": self.max_output_tokens,
            "store": false,
        });
        let response = self.send(&body)?;
        let (text, usage) = parse_response(&response)?;
        let json = serde_json::from_str(&text)
            .map_err(|err| ModelError::Malformed(format!("answer isn't valid JSON: {err}")))?;
        Ok(Completion {
            json,
            cost_usd: self.pricing.cost(&usage),
            usage,
        })
    }
}

/// Replays answers keyed by schema name, in order. For tests and for
/// checking the pipeline end to end without spending anything.
pub struct Scripted {
    answers: Mutex<Vec<(&'static str, VecDeque<Value>)>>,
    cost_per_call: f64,
}

impl Scripted {
    pub fn new(cost_per_call: f64) -> Self {
        Self {
            answers: Mutex::new(Vec::new()),
            cost_per_call,
        }
    }

    /// Queues answers for prompts with this schema name. When a queue runs
    /// out, its last answer repeats.
    pub fn answer(
        self,
        schema_name: &'static str,
        answers: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.answers
            .lock()
            .expect("script lock")
            .push((schema_name, answers.into_iter().collect()));
        self
    }
}

impl Model for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn worst_case_cost(&self, _prompt: &Prompt) -> f64 {
        self.cost_per_call
    }

    fn complete(&self, prompt: &Prompt) -> Result<Completion, ModelError> {
        let mut answers = self.answers.lock().expect("script lock");
        let queue = answers
            .iter_mut()
            .find(|(name, _)| *name == prompt.schema_name)
            .map(|(_, queue)| queue)
            .ok_or_else(|| {
                ModelError::Malformed(format!("no scripted answer for {}", prompt.schema_name))
            })?;
        let json = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        }
        .ok_or_else(|| ModelError::Malformed("empty script".into()))?;
        Ok(Completion {
            json,
            usage: Usage::default(),
            cost_usd: self.cost_per_call,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_usage_including_cache_and_reasoning() {
        let pricing = Pricing::for_model("gpt-6-luna").unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 400_000,
            output_tokens: 200_000,
            reasoning_tokens: 150_000,
        };
        // 600k uncached x $0.10 + 400k cached x $0.01 + 200k output x $0.50
        let expected = 0.06 + 0.004 + 0.10;
        assert!((pricing.cost(&usage) - expected).abs() < 1e-12);
    }

    #[test]
    fn parses_a_responses_api_answer() {
        let response = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "{\"verdict\":\"pass\"}"}
                ]}
            ],
            "usage": {
                "input_tokens": 1200,
                "input_tokens_details": {"cached_tokens": 1024},
                "output_tokens": 300,
                "output_tokens_details": {"reasoning_tokens": 200}
            }
        });
        let (text, usage) = parse_response(&response).unwrap();
        assert_eq!(text, "{\"verdict\":\"pass\"}");
        assert_eq!(usage.cached_input_tokens, 1024);
        assert_eq!(usage.reasoning_tokens, 200);
    }

    #[test]
    fn reports_refusals_and_truncation() {
        let refusal = json!({"status": "completed", "output": [{"type": "message", "content": [
            {"type": "refusal", "refusal": "no"}
        ]}]});
        assert!(matches!(
            parse_response(&refusal),
            Err(ModelError::Refusal(_))
        ));
        let cut =
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}});
        assert!(
            matches!(parse_response(&cut), Err(ModelError::Incomplete(r)) if r == "max_output_tokens")
        );
    }

    #[test]
    fn the_budget_refuses_calls_it_cannot_cover() {
        let model = Scripted::new(0.4).answer("x", [json!({})]);
        let prompt = Prompt {
            system: String::new(),
            user: String::new(),
            schema_name: "x",
            schema: json!({}),
        };
        let budget = Budget::new(1.0);
        assert!(budget.complete(&model, &prompt).is_ok());
        assert!(budget.complete(&model, &prompt).is_ok());
        assert!(matches!(
            budget.complete(&model, &prompt),
            Err(ModelError::BudgetExhausted { .. })
        ));
        assert!((budget.spent() - 0.8).abs() < 1e-12);
    }

    #[test]
    fn reads_the_key_from_a_dotenv_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".env"),
            "OTHER=1\nOPENAI_API_KEY=\"test-key\"\n",
        )
        .unwrap();
        // Only meaningful when the variable isn't already set in the environment.
        if std::env::var("OPENAI_API_KEY").is_err() {
            assert_eq!(api_key(dir.path()).as_deref(), Some("test-key"));
        }
    }
}
