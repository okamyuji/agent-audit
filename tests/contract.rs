use std::fs;

fn schema() -> serde_json::Value {
    let raw = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/schema/event.schema.json"
    ))
    .unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn fixtures() -> Vec<(String, serde_json::Value)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let vals: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        for (i, v) in vals.into_iter().enumerate() {
            out.push((format!("{}#{}", p.display(), i), v));
        }
    }
    out
}

#[test]
fn all_fixtures_match_schema() {
    let validator = jsonschema::validator_for(&schema()).unwrap();
    for (name, v) in fixtures() {
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn schema_rejects_empty_session_and_empty_response() {
    let validator = jsonschema::validator_for(&schema()).unwrap();
    let mut bad = fixtures()[0].1.clone();
    bad["session_id"] = serde_json::json!("");
    assert!(!validator.is_valid(&bad));
    let resp = serde_json::json!({
        "v":1,"id":"1b4e28ba-2fa1-11d2-883f-0016d3cca427","session_id":"s","run_id":"1b4e28ba-2fa1-11d2-883f-0016d3cca428",
        "seq":0,"ts":"2026-08-31T10:00:00Z","kind":"llm_response","provider":"p","model":"m","payload":{}
    });
    assert!(!validator.is_valid(&resp));
}
