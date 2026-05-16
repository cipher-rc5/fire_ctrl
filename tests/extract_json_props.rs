//! file: tests/extract_json_props.rs
//! description: Property-based tests for the LLM `extract_json` helper.
//!
//! Strategy: generate (a) a small well-formed JSON object, (b) random
//! surrounding prose, and (c) an optional code-fence wrapper. Assert that
//! `extract_json` recovers a string that parses as serde_json regardless of
//! the surrounding noise.

use fire_ctrl::config::LlmConfig;
use fire_ctrl::llm::LlmClient;
use proptest::prelude::*;

fn make_client() -> LlmClient {
    let cfg = LlmConfig {
        base_url: "http://127.0.0.1:11434/v1".to_string(),
        api_key: "test".to_string(),
        model_name: "test-model".to_string(),
        embedding_model: None,
        ollama_base_url: None,
        timeout_seconds: 5,
        max_retries: 1,
        max_input_chars: 1_000,
    };
    LlmClient::new(&cfg).expect("client construction must not fail")
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn leaf_strategy() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        (-1_000_i64..1_000).prop_map(|n| serde_json::Value::Number(n.into())),
        "[a-zA-Z0-9 _-]{0,20}".prop_map(serde_json::Value::String),
    ]
}

/// Generates a serde_json object — always an object, with 1..=4 leaf-valued
/// keys. Kept shallow so the regression surface is the JSON-locator logic,
/// not the serde parser.
fn json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
    prop::collection::hash_map("[a-zA-Z][a-zA-Z0-9_]{0,8}", leaf_strategy(), 1..4)
        .prop_map(|m| serde_json::Value::Object(m.into_iter().collect()))
}

/// Generates a random ASCII prose string with no `{`, `}`, `[`, `]`, or
/// backticks — anything that contains those tokens could ambiguously look
/// like JSON to a heuristic locator.
fn prose_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,!?]{0,40}".prop_map(String::from)
}

#[derive(Debug, Clone)]
enum Wrapping {
    Bare,
    PrefixedProse,
    JsonFence,
    BareFence,
}

fn wrapping_strategy() -> impl Strategy<Value = Wrapping> {
    prop_oneof![
        Just(Wrapping::Bare),
        Just(Wrapping::PrefixedProse),
        Just(Wrapping::JsonFence),
        Just(Wrapping::BareFence),
    ]
}

fn wrap(value: &serde_json::Value, prose: &str, wrapping: &Wrapping) -> String {
    let raw = serde_json::to_string(value).expect("serialise json");
    match wrapping {
        Wrapping::Bare => raw,
        Wrapping::PrefixedProse => format!("{prose}\n{raw}"),
        Wrapping::JsonFence => format!("{prose}\n```json\n{raw}\n```\n"),
        Wrapping::BareFence => format!("{prose}\n```\n{raw}\n```\n"),
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// For every (value, prose, wrapping) combination, `extract_json` must
    /// return a string that itself parses back to a `serde_json::Value`.
    #[test]
    fn extract_json_recovers_a_parseable_value(
        value in json_value_strategy(),
        prose in prose_strategy(),
        wrapping in wrapping_strategy(),
    ) {
        let client = make_client();
        let input = wrap(&value, &prose, &wrapping);

        let extracted = client
            .extract_json(&input)
            .map_err(|e| TestCaseError::fail(format!("extract_json returned Err for input {input:?}: {e}")))?;

        let parsed: serde_json::Value = serde_json::from_str(&extracted)
            .map_err(|e| TestCaseError::fail(format!(
                "extract_json returned `{extracted}` for input `{input}` but it does not parse: {e}"
            )))?;

        prop_assert!(parsed.is_object(), "expected object, got {parsed:?}");
    }

    /// Truncated/malformed JSON must either return Err OR return a string
    /// that, when fed to serde_json, fails. It must NOT panic.
    #[test]
    fn extract_json_does_not_panic_on_malformed_input(
        prose in "[^`{}\\[\\]]{0,60}",
        broken in r#"\{"key"\s*:"#,
    ) {
        let client = make_client();
        let input = format!("{prose}{broken}");
        // We don't assert success here — just absence of panic.
        let _ = client.extract_json(&input);
    }
}

// ---------------------------------------------------------------------------
// Targeted regressions for the wrapping shapes called out in the task brief.
// ---------------------------------------------------------------------------

#[test]
fn extract_json_handles_code_fence_json_marker() {
    let client = make_client();
    let raw = client
        .extract_json("```json\n{\"k\":1}\n```")
        .expect("must extract");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["k"], 1);
}

#[test]
fn extract_json_handles_bare_code_fence() {
    let client = make_client();
    let raw = client
        .extract_json("```\n{\"k\":2}\n```")
        .expect("must extract");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["k"], 2);
}

#[test]
fn extract_json_handles_prefix_prose() {
    let client = make_client();
    let raw = client
        .extract_json("Sure! Here is your JSON: {\"k\":3}")
        .expect("must extract");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["k"], 3);
}

#[test]
fn extract_json_returns_err_on_no_json_at_all() {
    let client = make_client();
    // No braces, no fences, no JSON shape.
    assert!(client.extract_json("hello world no json here").is_err());
}
