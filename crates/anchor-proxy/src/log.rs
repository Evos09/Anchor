//! The proxy's only per-request record (Update #1 §4.5): metadata, never
//! content. The type has exactly these fields and nothing else can be
//! added without changing it here, where the test below pins the list.
//! No prompt, no response, no excerpt of either, no path, no code.

use anchor_inference::Path;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogRecord {
    pub user_id: String,
    pub timestamp_ms: u64,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub http_status: u16,
    pub latency_ms: u64,
    pub path: Path,
}

/// The record's fields, in order: the store's `log` table has exactly
/// these columns.
pub const FIELDS: [&str; 9] = [
    "user_id",
    "timestamp_ms",
    "provider",
    "model",
    "input_tokens",
    "output_tokens",
    "http_status",
    "latency_ms",
    "path",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_record_holds_metadata_only() {
        let r = LogRecord {
            user_id: "u1".into(),
            timestamp_ms: 1,
            provider: "p".into(),
            model: "m".into(),
            input_tokens: 2,
            output_tokens: 3,
            http_status: 200,
            latency_ms: 4,
            path: Path::A,
        };
        let v = serde_json::to_value(&r).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = FIELDS.to_vec();
        want.sort_unstable();
        let mut got = keys.clone();
        got.sort_unstable();
        assert_eq!(got, want);
        // Every field is an identifier, a number or the path: the type
        // cannot carry text from a prompt or a response.
        let LogRecord {
            user_id: _,
            timestamp_ms: _,
            provider: _,
            model: _,
            input_tokens: _,
            output_tokens: _,
            http_status: _,
            latency_ms: _,
            path: _,
        } = r;
    }
}
