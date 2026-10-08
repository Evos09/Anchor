//! Token estimation before a request is forwarded (Update #1 §4.4,
//! ADR-0029): about four bytes per token. Crude and provider-agnostic, it
//! needs no tokenizer; the proxy replaces the estimate with the provider's
//! own count once the response reports usage.

/// Bytes per token assumed by the heuristic.
pub const BYTES_PER_TOKEN: u64 = 4;

/// Estimated tokens in `bytes` bytes of text, rounded up.
pub fn tokens_for_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(BYTES_PER_TOKEN)
}

/// Estimated input tokens of a chat completions request: its messages
/// and tool definitions, as sent.
pub fn request_input_tokens(body: &serde_json::Value) -> u64 {
    let part = |k: &str| body.get(k).map_or(0, |v| v.to_string().len());
    tokens_for_bytes(part("messages") + part("tools"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn four_bytes_a_token_rounded_up() {
        assert_eq!(tokens_for_bytes(0), 0);
        assert_eq!(tokens_for_bytes(1), 1);
        assert_eq!(tokens_for_bytes(4), 1);
        assert_eq!(tokens_for_bytes(5), 2);
        assert_eq!(tokens_for_bytes(4000), 1000);
    }

    #[test]
    fn only_messages_and_tools_count() {
        let small = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
        let bigger = json!({
            "model": "a-much-longer-model-name-that-should-not-count",
            "messages": [{ "role": "user", "content": "hi" }],
        });
        assert_eq!(request_input_tokens(&small), request_input_tokens(&bigger));
        let with_tools = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "read_file" } }],
        });
        assert!(request_input_tokens(&with_tools) > request_input_tokens(&small));
    }
}
