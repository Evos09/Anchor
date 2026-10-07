//! Verifier plugin protocol, version 1 (spec §14 Phase 8, ADR-0009).
//!
//! A verifier plugin is a separate program. For each review, Anchor starts
//! it in the command sandbox (no network, no workspace), writes **one**
//! request line to its stdin and closes it; the plugin writes **one**
//! response line to stdout and exits 0. The published contract, with JSON
//! Schemas, is `docs/verifier/`; this module is Anchor's side of it.
//!
//! Every frame is `{"v": 1, "id": N, "msg": {"kind": K, "payload": P}}`:
//!
//! | request `kind` | payload | answered with |
//! |---|---|---|
//! | `describe` | `{}` | `info` |
//! | `verify` | a [`ReviewInput`] | `verdicts` |
//!
//! Either may instead be answered with `error`. Anchor is strict about
//! responses: [`check_info`] and [`check_verdicts`] turn anything other than
//! a well-formed answer into an error, which the caller treats as every
//! claim unverifiable (fail closed).
//!
//! Stability: within version 1, requests may gain optional fields (plugins
//! must ignore fields they do not know); responses never change. Anything
//! else is version 2.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::verify::{ClaimVerdict, ReviewInput, Verdict, validate_verdicts};

/// The verifier protocol version Anchor speaks.
pub const VERIFIER_PROTOCOL: u32 = 1;
/// A response line longer than this is refused.
pub const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// A request Anchor sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub msg: RequestMsg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RequestMsg {
    /// Ask the plugin who it is and which protocol versions it speaks.
    Describe(Describe),
    /// Judge the claims in the input.
    Verify(Box<ReviewInput>),
}

/// The (empty) payload of `describe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Describe {}

impl Request {
    pub fn describe(id: u64) -> Request {
        Request {
            v: VERIFIER_PROTOCOL,
            id,
            msg: RequestMsg::Describe(Describe {}),
        }
    }

    pub fn verify(id: u64, input: ReviewInput) -> Request {
        Request {
            v: VERIFIER_PROTOCOL,
            id,
            msg: RequestMsg::Verify(Box::new(input)),
        }
    }

    /// The request as one line, newline included.
    pub fn encode(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_default();
        line.push(b'\n');
        line
    }
}

/// A response a plugin sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    pub msg: ResponseMsg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResponseMsg {
    Info(Info),
    Verdicts(Verdicts),
    Error(ErrorPayload),
}

/// The answer to `describe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Info {
    /// 1..=64 bytes of `[a-z0-9_.-]`.
    pub name: String,
    /// The plugin's own version, 1..=64 printable ASCII bytes.
    pub version: String,
    /// Protocol versions the plugin speaks.
    pub protocol: Vec<u32>,
}

/// The answer to `verify`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdicts {
    pub verdicts: Vec<ClaimVerdict>,
}

/// A plugin that cannot answer says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPayload {
    /// 1..=64 bytes of `[a-z0-9_]`.
    pub code: String,
    /// Advisory; never parsed.
    #[serde(default)]
    pub message: String,
}

fn is_name(s: &str, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || extra.contains(&b))
}

/// Parse one response line: exactly one non-empty line, within
/// [`MAX_RESPONSE_BYTES`], version 1, answering request `id`.
pub fn parse_response(id: u64, out: &[u8]) -> Result<Response, String> {
    if out.len() > MAX_RESPONSE_BYTES {
        return Err(format!("response exceeds {MAX_RESPONSE_BYTES} bytes"));
    }
    let text = std::str::from_utf8(out).map_err(|_| "response is not UTF-8".to_string())?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next().ok_or("no response line")?;
    if lines.next().is_some() {
        return Err("more than one response line".into());
    }
    #[derive(Deserialize)]
    struct VersionOnly {
        v: u32,
    }
    let v: VersionOnly =
        serde_json::from_str(line).map_err(|e| format!("malformed response: {e}"))?;
    if v.v != VERIFIER_PROTOCOL {
        return Err(format!(
            "response is protocol version {}, expected {VERIFIER_PROTOCOL}",
            v.v
        ));
    }
    let r: Response = serde_json::from_str(line).map_err(|e| format!("malformed response: {e}"))?;
    if r.id != id {
        return Err(format!("response answers request {}, not {id}", r.id));
    }
    if let ResponseMsg::Error(e) = &r.msg {
        if !is_name(&e.code, b"_") || e.message.len() > 2048 {
            return Err("malformed error payload".into());
        }
        return Err(format!("plugin error {}", e.code));
    }
    Ok(r)
}

/// Check the answer to `describe`: a valid name and version, and a
/// protocol list that includes [`VERIFIER_PROTOCOL`].
pub fn check_info(id: u64, out: &[u8]) -> Result<Info, String> {
    let ResponseMsg::Info(info) = parse_response(id, out)?.msg else {
        return Err("describe was not answered with info".into());
    };
    if !is_name(&info.name, b"_.-") {
        return Err("info name must be 1..=64 bytes of [a-z0-9_.-]".into());
    }
    if info.version.is_empty()
        || info.version.len() > 64
        || !info.version.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("info version must be 1..=64 printable ASCII bytes".into());
    }
    if info.protocol.is_empty() || info.protocol.len() > 8 {
        return Err("info protocol must list 1..=8 versions".into());
    }
    if !info.protocol.contains(&VERIFIER_PROTOCOL) {
        return Err(format!(
            "plugin speaks protocol {:?}, not {VERIFIER_PROTOCOL}",
            info.protocol
        ));
    }
    Ok(info)
}

/// Check the answer to `verify` against its input, and return one verdict
/// per claim, in claim order.
///
/// * Each verdict must be valid, name a claim of the input, and name it
///   once; otherwise the whole response is refused.
/// * A claim the plugin did not answer is `unverifiable`
///   (`verifier_no_verdict`).
/// * Over a truncated input, `supported` becomes `unverifiable`
///   (`input_truncated`): nothing cut short can be supported.
pub fn check_verdicts(
    id: u64,
    input: &ReviewInput,
    out: &[u8],
) -> Result<Vec<ClaimVerdict>, String> {
    let ResponseMsg::Verdicts(Verdicts { verdicts }) = parse_response(id, out)?.msg else {
        return Err("verify was not answered with verdicts".into());
    };
    validate_verdicts(&verdicts)?;
    let claims: BTreeSet<&str> = input.claims.iter().map(|c| c.id.as_str()).collect();
    let mut by_id: BTreeMap<String, ClaimVerdict> = BTreeMap::new();
    for v in verdicts {
        if !claims.contains(v.claim_id.as_str()) {
            return Err(format!("verdict for unknown claim {:?}", v.claim_id));
        }
        if by_id.contains_key(&v.claim_id) {
            return Err(format!("two verdicts for claim {:?}", v.claim_id));
        }
        by_id.insert(v.claim_id.clone(), v);
    }
    Ok(input
        .claims
        .iter()
        .map(|c| match by_id.remove(&c.id) {
            Some(v) if input.truncated && v.verdict == Verdict::Supported => ClaimVerdict {
                verdict: Verdict::Unverifiable,
                reason_code: "input_truncated".into(),
                ..v
            },
            Some(v) => v,
            None => ClaimVerdict {
                claim_id: c.id.clone(),
                verdict: Verdict::Unverifiable,
                reason_code: "verifier_no_verdict".into(),
                evidence_refs: Vec::new(),
                note: "the plugin gave no verdict for this claim".into(),
            },
        })
        .collect())
}

/// Every claim `unverifiable` with `verifier_failed`: what a plugin that
/// crashed, timed out or answered badly contributes (fail closed).
pub fn failed(input: &ReviewInput, why: &str) -> Vec<ClaimVerdict> {
    let mut note = format!("verifier failed: {why}");
    if note.len() > 2048 {
        let mut i = 2048;
        while !note.is_char_boundary(i) {
            i -= 1;
        }
        note.truncate(i);
    }
    let note = note.replace('\0', "");
    input
        .claims
        .iter()
        .map(|c| ClaimVerdict {
            claim_id: c.id.clone(),
            verdict: Verdict::Unverifiable,
            reason_code: "verifier_failed".into(),
            evidence_refs: Vec::new(),
            note: note.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Claim;

    fn input(ids: &[&str]) -> ReviewInput {
        ReviewInput {
            claims: ids
                .iter()
                .map(|id| Claim {
                    id: (*id).into(),
                    text: "does a thing".into(),
                    files: vec!["m.py".into()],
                    tests: vec!["t.T.test".into()],
                })
                .collect(),
            changes: Vec::new(),
            commands: Vec::new(),
            tests: Vec::new(),
            coverage: None,
            claim_refs: BTreeMap::new(),
            truncated: false,
        }
    }

    fn verdicts(id: u64, vs: &str) -> Vec<u8> {
        format!(r#"{{"v":1,"id":{id},"msg":{{"kind":"verdicts","payload":{{"verdicts":{vs}}}}}}}"#)
            .into_bytes()
    }

    #[test]
    fn requests_have_the_documented_shape() {
        let d = String::from_utf8(Request::describe(1).encode()).unwrap();
        assert_eq!(
            d,
            "{\"v\":1,\"id\":1,\"msg\":{\"kind\":\"describe\",\"payload\":{}}}\n"
        );
        let v: serde_json::Value =
            serde_json::from_slice(&Request::verify(2, input(&["c1"])).encode()).unwrap();
        assert_eq!(v["msg"]["kind"], "verify");
        assert_eq!(v["msg"]["payload"]["claims"][0]["id"], "c1");
    }

    #[test]
    fn info_is_checked() {
        let ok = br#"{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"py-check","version":"0.1.0","protocol":[1]}}}"#;
        assert_eq!(check_info(1, ok).unwrap().name, "py-check");
        let wrong_id = check_info(2, ok).unwrap_err();
        assert!(wrong_id.contains("answers request 1"), "{wrong_id}");
        let v2 = br#"{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"x","version":"1","protocol":[2]}}}"#;
        assert!(check_info(1, v2).unwrap_err().contains("not 1"));
        let bad_name = br#"{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"Bad Name","version":"1","protocol":[1]}}}"#;
        assert!(check_info(1, bad_name).is_err());
        let extra = br#"{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"x","version":"1","protocol":[1],"more":1}}}"#;
        assert!(check_info(1, extra).is_err(), "responses are strict");
        let future = br#"{"v":2,"id":1,"msg":{}}"#;
        assert!(check_info(1, future).unwrap_err().contains("version 2"));
    }

    #[test]
    fn verdicts_are_checked_one_per_claim() {
        let inp = input(&["c1", "c2"]);
        let one = verdicts(
            5,
            r#"[{"claim_id":"c2","verdict":"refuted","reason_code":"bad","evidence_refs":["diff:m.py"]}]"#,
        );
        let vs = check_verdicts(5, &inp, &one).unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].claim_id, "c1");
        assert_eq!(vs[0].reason_code, "verifier_no_verdict");
        assert_eq!(vs[1].verdict, Verdict::Refuted);

        let unknown = verdicts(
            5,
            r#"[{"claim_id":"zz","verdict":"supported","reason_code":"ok"}]"#,
        );
        assert!(
            check_verdicts(5, &inp, &unknown)
                .unwrap_err()
                .contains("unknown claim")
        );
        let twice = verdicts(
            5,
            r#"[{"claim_id":"c1","verdict":"supported","reason_code":"ok"},{"claim_id":"c1","verdict":"refuted","reason_code":"no"}]"#,
        );
        assert!(
            check_verdicts(5, &inp, &twice)
                .unwrap_err()
                .contains("two verdicts")
        );
        let bad_code = verdicts(
            5,
            r#"[{"claim_id":"c1","verdict":"supported","reason_code":"Not OK"}]"#,
        );
        assert!(check_verdicts(5, &inp, &bad_code).is_err());
        let bad_verdict = verdicts(
            5,
            r#"[{"claim_id":"c1","verdict":"maybe","reason_code":"ok"}]"#,
        );
        assert!(check_verdicts(5, &inp, &bad_verdict).is_err());
    }

    #[test]
    fn truncated_input_cannot_be_supported() {
        let mut inp = input(&["c1"]);
        inp.truncated = true;
        let ok = verdicts(
            1,
            r#"[{"claim_id":"c1","verdict":"supported","reason_code":"ok"}]"#,
        );
        let vs = check_verdicts(1, &inp, &ok).unwrap();
        assert_eq!(vs[0].verdict, Verdict::Unverifiable);
        assert_eq!(vs[0].reason_code, "input_truncated");
    }

    #[test]
    fn bad_output_is_refused() {
        let inp = input(&["c1"]);
        for out in [
            &b""[..],
            b"not json",
            b"\xff\xfe",
            br#"{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"x","version":"1","protocol":[1]}}}"#,
            br#"{"v":1,"id":1,"msg":{"kind":"error","payload":{"code":"busy","message":"later"}}}"#,
        ] {
            assert!(check_verdicts(1, &inp, out).is_err(), "{}", String::from_utf8_lossy(out));
        }
        let mut two = verdicts(1, "[]");
        two.extend_from_slice(b"\n");
        two.extend_from_slice(&verdicts(1, "[]"));
        assert!(
            check_verdicts(1, &inp, &two)
                .unwrap_err()
                .contains("more than one")
        );
        let huge = vec![b' '; MAX_RESPONSE_BYTES + 1];
        assert!(
            check_verdicts(1, &inp, &huge)
                .unwrap_err()
                .contains("exceeds")
        );
        // A trailing newline (and blank lines) are fine.
        let mut nl = verdicts(1, "[]");
        nl.extend_from_slice(b"\n\n");
        assert_eq!(
            check_verdicts(1, &inp, &nl).unwrap()[0].reason_code,
            "verifier_no_verdict"
        );
    }

    #[test]
    fn failures_fail_closed() {
        let vs = failed(&input(&["a", "b"]), &"x".repeat(5000));
        assert_eq!(vs.len(), 2);
        assert!(vs.iter().all(|v| v.verdict == Verdict::Unverifiable
            && v.reason_code == "verifier_failed"
            && v.note.len() <= 2048));
        validate_verdicts(&vs).unwrap();
    }
}
