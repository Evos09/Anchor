//! The published verifier contract (`docs/verifier/`) cannot drift from
//! the code (spec §14 Phase 8): the fixtures and Anchor's own requests
//! validate against the request schema, the responses Anchor accepts
//! validate against the response schema, and in both every field the code
//! sends or accepts is in the schema, and every field in the schema is one
//! the code knows.
//!
//! The validator covers exactly the JSON Schema subset the two schemas use
//! (type, const, enum, required, properties, additionalProperties, items,
//! minItems, maxItems, maxLength, minimum, oneOf, $ref to $defs, and
//! patterns of the form `^[class]{m,n}$`), so no dependency is needed. A
//! schema keyword outside that subset fails the test.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use anchor_protocol::verifier::{
    self, ErrorPayload, Info, Request, Response, ResponseMsg, Verdicts,
};
use anchor_protocol::verify::{ClaimVerdict, ReviewInput, Verdict};
use serde_json::{Value, json};

const REQUEST: &str = include_str!("../../../docs/verifier/schema/request.schema.json");
const RESPONSE: &str = include_str!("../../../docs/verifier/schema/response.schema.json");
const FIXTURES: &[(&str, &str)] = &[
    (
        "passing",
        include_str!("../../../docs/verifier/fixtures/passing.json"),
    ),
    (
        "failing-test",
        include_str!("../../../docs/verifier/fixtures/failing-test.json"),
    ),
    (
        "debug-leftover",
        include_str!("../../../docs/verifier/fixtures/debug-leftover.json"),
    ),
    (
        "truncated",
        include_str!("../../../docs/verifier/fixtures/truncated.json"),
    ),
];

const KNOWN: &[&str] = &[
    "$schema",
    "$id",
    "title",
    "description",
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "maxLength",
    "minimum",
    "oneOf",
    "$ref",
    "$defs",
    "pattern",
];

struct Validator<'a> {
    root: &'a Value,
    /// Also require every schema property to be present (fully populated
    /// samples), and treat a missing `additionalProperties` as false.
    exact: bool,
}

/// `^[class]{m,n}$`, where class is literal characters and `a-z` ranges.
fn pattern_matches(pattern: &str, s: &str) -> bool {
    let inner = pattern
        .strip_prefix("^[")
        .and_then(|p| p.strip_suffix('$'))
        .unwrap_or_else(|| panic!("unsupported pattern {pattern}"));
    let (class, count) = inner
        .split_once("]{")
        .unwrap_or_else(|| panic!("unsupported pattern {pattern}"));
    let (lo, hi) = count
        .strip_suffix('}')
        .and_then(|c| c.split_once(','))
        .unwrap_or_else(|| panic!("unsupported pattern {pattern}"));
    let (lo, hi): (usize, usize) = (lo.parse().unwrap(), hi.parse().unwrap());
    let chars: Vec<char> = class.chars().collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '-' {
            ranges.push((chars[i], chars[i + 2]));
            i += 3;
        } else {
            ranges.push((chars[i], chars[i]));
            i += 1;
        }
    }
    let n = s.chars().count();
    (lo..=hi).contains(&n)
        && s.chars()
            .all(|c| ranges.iter().any(|(a, b)| (*a..=*b).contains(&c)))
}

impl Validator<'_> {
    fn resolve<'s>(&'s self, schema: &'s Value) -> &'s Value {
        match schema.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                let name = r.strip_prefix("#/$defs/").expect("local $ref");
                &self.root["$defs"][name]
            }
            None => schema,
        }
    }

    fn check(&self, schema: &Value, v: &Value, at: &str) -> Result<(), String> {
        let schema = self.resolve(schema);
        let obj = schema.as_object().expect("schema object");
        for k in obj.keys() {
            assert!(
                KNOWN.contains(&k.as_str()),
                "schema keyword {k} at {at} is outside the subset"
            );
        }
        if let Some(c) = obj.get("const")
            && c != v
        {
            return Err(format!("{at}: expected {c}, got {v}"));
        }
        if let Some(e) = obj.get("enum").and_then(Value::as_array)
            && !e.contains(v)
        {
            return Err(format!("{at}: {v} not in {e:?}"));
        }
        if let Some(alts) = obj.get("oneOf").and_then(Value::as_array) {
            let ok = alts.iter().filter(|a| self.check(a, v, at).is_ok()).count();
            if ok != 1 {
                let why: Vec<String> = alts
                    .iter()
                    .filter_map(|a| self.check(a, v, at).err())
                    .collect();
                return Err(format!(
                    "{at}: {ok} of the oneOf alternatives match ({why:?})"
                ));
            }
        }
        if let Some(t) = obj.get("type").and_then(Value::as_str) {
            let ok = match t {
                "object" => v.is_object(),
                "array" => v.is_array(),
                "string" => v.is_string(),
                "integer" => v.is_i64() || v.is_u64(),
                "boolean" => v.is_boolean(),
                "null" => v.is_null(),
                other => panic!("type {other}"),
            };
            if !ok {
                return Err(format!("{at}: expected {t}, got {v}"));
            }
        }
        if let Some(s) = v.as_str() {
            if let Some(max) = obj.get("maxLength").and_then(Value::as_u64)
                && s.chars().count() as u64 > max
            {
                return Err(format!("{at}: longer than {max}"));
            }
            if let Some(p) = obj.get("pattern").and_then(Value::as_str)
                && !pattern_matches(p, s)
            {
                return Err(format!("{at}: {s:?} does not match {p}"));
            }
        }
        if let (Some(min), Some(n)) = (obj.get("minimum").and_then(Value::as_i64), v.as_i64())
            && n < min
        {
            return Err(format!("{at}: {n} < {min}"));
        }
        if let Some(items) = v.as_array() {
            if let Some(max) = obj.get("maxItems").and_then(Value::as_u64)
                && items.len() as u64 > max
            {
                return Err(format!("{at}: more than {max} items"));
            }
            if let Some(min) = obj.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                return Err(format!("{at}: fewer than {min} items"));
            }
            if let Some(s) = obj.get("items") {
                for (i, item) in items.iter().enumerate() {
                    self.check(s, item, &format!("{at}[{i}]"))?;
                }
            }
        }
        if let Some(map) = v.as_object() {
            let props = obj.get("properties").and_then(Value::as_object);
            for r in obj
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let r = r.as_str().unwrap();
                if !map.contains_key(r) {
                    return Err(format!("{at}: missing required {r}"));
                }
            }
            if self.exact
                && let Some(props) = props
            {
                for k in props.keys() {
                    if !map.contains_key(k) {
                        return Err(format!("{at}: schema field {k} is not in the sample"));
                    }
                }
            }
            for (k, val) in map {
                match props.and_then(|p| p.get(k)) {
                    Some(s) => self.check(s, val, &format!("{at}.{k}"))?,
                    None => match obj.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            return Err(format!("{at}: field {k} is not in the schema"));
                        }
                        Some(s @ Value::Object(_)) => self.check(s, val, &format!("{at}.{k}"))?,
                        _ if self.exact && props.is_some() => {
                            return Err(format!("{at}: field {k} is not in the schema"));
                        }
                        _ => {}
                    },
                }
            }
        }
        Ok(())
    }
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn validate(schema: &Value, v: &Value, exact: bool) -> Result<(), String> {
    Validator {
        root: schema,
        exact,
    }
    .check(schema, v, "$")
}

fn fixture(name: &str) -> ReviewInput {
    let (_, t) = FIXTURES.iter().find(|(n, _)| *n == name).unwrap();
    serde_json::from_str(t).unwrap()
}

#[test]
fn every_request_anchor_sends_matches_the_schema_exactly() {
    let s = schema(REQUEST);
    let d: Value = serde_json::from_slice(&Request::describe(0).encode()).unwrap();
    validate(&s, &d, true).unwrap();
    for (name, _) in FIXTURES {
        let r: Value = serde_json::from_slice(&Request::verify(9, fixture(name)).encode()).unwrap();
        validate(&s, &r, true).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

fn verdict(all_fields: bool) -> ClaimVerdict {
    ClaimVerdict {
        claim_id: "c1".into(),
        verdict: Verdict::Refuted,
        reason_code: "test_failed".into(),
        evidence_refs: if all_fields {
            vec!["test:t.T.test".into()]
        } else {
            vec![]
        },
        note: if all_fields {
            "the cited test failed".into()
        } else {
            String::new()
        },
    }
}

#[test]
fn every_response_anchor_accepts_matches_the_schema_exactly() {
    let s = schema(RESPONSE);
    let samples = [
        ResponseMsg::Info(Info {
            name: "py-hygiene".into(),
            version: "0.1.0".into(),
            protocol: vec![1],
        }),
        ResponseMsg::Verdicts(Verdicts {
            verdicts: vec![verdict(true)],
        }),
        ResponseMsg::Error(ErrorPayload {
            code: "not_ready".into(),
            message: "advisory".into(),
        }),
    ];
    for msg in samples {
        let r = Response { v: 1, id: 4, msg };
        let v = serde_json::to_value(&r).unwrap();
        validate(&s, &v, true).unwrap_or_else(|e| panic!("{v}: {e}"));
    }
    // And Anchor accepts what the schema calls valid: the optional fields
    // left out.
    let minimal = json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[
        {"claim_id":"c1","verdict":"supported","reason_code":"ok"}]}}});
    validate(&s, &minimal, false).unwrap();
    let mut input = fixture("passing");
    input.claims.truncate(1);
    verifier::check_verdicts(4, &input, minimal.to_string().as_bytes()).unwrap();
}

#[test]
fn schema_and_code_refuse_the_same_bad_responses() {
    let s = schema(RESPONSE);
    let mut input = fixture("passing");
    input.claims.truncate(1);
    let bad = [
        json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[
            {"claim_id":"c1","verdict":"supported","reason_code":"ok","extra":1}]}}}),
        json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[
            {"claim_id":"c1","verdict":"maybe","reason_code":"ok"}]}}}),
        json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[
            {"claim_id":"c1","verdict":"supported","reason_code":"Not OK"}]}}}),
        json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[
            {"claim_id":"c1","verdict":"supported"}]}}}),
        json!({"v":1,"id":4,"msg":{"kind":"verdicts","payload":{}}}),
        json!({"v":2,"id":4,"msg":{"kind":"verdicts","payload":{"verdicts":[]}}}),
        json!({"v":1,"id":4,"extra":true,"msg":{"kind":"verdicts","payload":{"verdicts":[]}}}),
        json!({"v":1,"id":4,"msg":{"kind":"verdict","payload":{"verdicts":[]}}}),
    ];
    for b in bad {
        assert!(validate(&s, &b, false).is_err(), "schema accepts {b}");
        assert!(
            verifier::check_verdicts(4, &input, b.to_string().as_bytes()).is_err(),
            "code accepts {b}"
        );
    }
}

#[test]
fn the_schemas_use_only_the_supported_subset() {
    // Walk both schemas: every keyword is known and every $ref resolves.
    fn walk(root: &Value, schema: &Value, refs: &mut BTreeSet<String>) {
        let m = schema.as_object().expect("a schema is an object");
        if let Some(r) = m.get("$ref").and_then(Value::as_str) {
            let name = r.strip_prefix("#/$defs/").expect("local $ref");
            assert!(root["$defs"].get(name).is_some(), "dangling {r}");
            refs.insert(name.into());
        }
        for (k, x) in m {
            assert!(KNOWN.contains(&k.as_str()), "keyword {k}");
            match k.as_str() {
                // Maps from names to schemas.
                "properties" | "$defs" => x
                    .as_object()
                    .unwrap()
                    .values()
                    .for_each(|s| walk(root, s, refs)),
                "items" => walk(root, x, refs),
                "additionalProperties" if x.is_object() => walk(root, x, refs),
                "oneOf" => x
                    .as_array()
                    .unwrap()
                    .iter()
                    .for_each(|s| walk(root, s, refs)),
                _ => {}
            }
        }
    }
    for text in [REQUEST, RESPONSE] {
        let s = schema(text);
        let mut refs = BTreeSet::new();
        walk(&s, &s, &mut refs);
        let defs: BTreeSet<String> = s["$defs"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(refs, defs, "every definition is used");
    }
    assert!(pattern_matches("^[a-z0-9_]{1,64}$", "no_diff"));
    assert!(!pattern_matches("^[a-z0-9_]{1,64}$", "No"));
    assert!(pattern_matches("^[!-~]{1,64}$", "0.1.0+build"));
    assert!(!pattern_matches("^[!-~]{1,64}$", "a b"));
}
