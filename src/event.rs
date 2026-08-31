use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
    Usage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuditEvent {
    pub v: u32,
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: Kind,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported event version {0}")]
    Version(u32),
    #[error("invalid event json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DecodeStats {
    pub accepted: usize,
    pub skipped: usize,
}

pub fn decode(raw: &[u8]) -> Result<AuditEvent, DecodeError> {
    #[derive(Deserialize)]
    struct Head {
        v: u32,
    }
    let head: Head = serde_json::from_slice(raw)?;
    if head.v != 1 {
        return Err(DecodeError::Version(head.v));
    }
    Ok(serde_json::from_slice(raw)?)
}

pub fn decode_all<'a>(lines: impl Iterator<Item = &'a [u8]>) -> (Vec<AuditEvent>, DecodeStats) {
    let mut out = Vec::new();
    let mut stats = DecodeStats::default();
    for l in lines {
        match decode(l) {
            Ok(e) => {
                out.push(e);
                stats.accepted += 1;
            }
            Err(_) => stats.skipped += 1,
        }
    }
    (out, stats)
}

/// 同一 run 内は seq 昇順。run 同士は届いている最小 seq のイベントの ts で並べる。
/// ts を第 1 キーにしないのは、時計の後退で tool_call と tool_result が逆転するのを避けるため
pub fn order_and_dedup(events: Vec<AuditEvent>) -> Vec<AuditEvent> {
    let mut seen = HashSet::new();
    let mut out: Vec<AuditEvent> = events
        .into_iter()
        .filter(|e| seen.insert((e.session_id.clone(), e.run_id.clone(), e.seq)))
        .collect();
    let mut first_ts: HashMap<String, (u64, DateTime<Utc>)> = HashMap::new();
    for e in &out {
        let entry = first_ts.entry(e.run_id.clone()).or_insert((e.seq, e.ts));
        if e.seq < entry.0 {
            *entry = (e.seq, e.ts);
        }
    }
    out.sort_by(|a, b| {
        let ka = (first_ts[&a.run_id].1, a.run_id.as_str(), a.seq);
        let kb = (first_ts[&b.run_id].1, b.run_id.as_str(), b.seq);
        ka.cmp(&kb)
    });
    out
}

pub fn is_truncated(payload: &serde_json::Value) -> Option<u64> {
    if payload.get("truncated")?.as_bool()? {
        payload.get("bytes")?.as_u64()
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolGroup {
    pub call_id: String,
    pub attempt: usize,
    pub response: Option<usize>,
    pub call: Option<usize>,
    pub result: Option<usize>,
}

/// call_id ごとに llm_response / tool_call / tool_result を結ぶ。tool_call が再び来たら新しい試行を始める
pub fn group_tool_calls(events: &[AuditEvent]) -> Vec<ToolGroup> {
    let mut groups: Vec<ToolGroup> = Vec::new();
    let mut attempts: HashMap<String, usize> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        let Some(cid) = e.call_id.as_deref() else {
            continue;
        };
        let open = groups
            .iter_mut()
            .rev()
            .find(|g| g.call_id == cid && g.result.is_none());
        match e.kind {
            Kind::LlmResponse => {
                if let Some(g) = open.filter(|g| g.response.is_none()) {
                    g.response = Some(i);
                } else {
                    let n = attempts.entry(cid.to_string()).or_insert(0);
                    *n += 1;
                    groups.push(ToolGroup {
                        call_id: cid.to_string(),
                        attempt: *n,
                        response: Some(i),
                        call: None,
                        result: None,
                    });
                }
            }
            Kind::ToolCall => {
                if let Some(g) = open.filter(|g| g.call.is_none()) {
                    g.call = Some(i);
                } else {
                    let n = attempts.entry(cid.to_string()).or_insert(0);
                    *n += 1;
                    groups.push(ToolGroup {
                        call_id: cid.to_string(),
                        attempt: *n,
                        response: None,
                        call: Some(i),
                        result: None,
                    });
                }
            }
            Kind::ToolResult => {
                if let Some(g) = open {
                    g.result = Some(i);
                } else {
                    let n = attempts.entry(cid.to_string()).or_insert(0);
                    *n += 1;
                    groups.push(ToolGroup {
                        call_id: cid.to_string(),
                        attempt: *n,
                        response: None,
                        call: None,
                        result: Some(i),
                    });
                }
            }
            _ => {}
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(name: &str) -> Vec<AuditEvent> {
        let raw = std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let vals: Vec<serde_json::Value> = serde_json::from_slice(&raw).unwrap();
        vals.iter()
            .map(|v| decode(v.to_string().as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn decodes_basic_fixture() {
        let evs = load("basic.json");
        assert_eq!(evs.len(), 7);
        assert_eq!(evs[0].kind, Kind::LlmRequest);
        assert_eq!(evs[1].call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn rejects_wrong_version_and_missing_fields() {
        assert!(matches!(
            decode(br#"{"v":2}"#),
            Err(DecodeError::Version(2))
        ));
        assert!(decode(br#"{"v":1,"id":"x"}"#).is_err());
        let (ok, stats) =
            decode_all([br#"{"v":1}"#.as_slice(), br#"not json"#.as_slice()].into_iter());
        assert!(ok.is_empty());
        assert_eq!(stats.skipped, 2);
    }

    #[test]
    fn orders_within_run_by_seq_and_runs_by_first_ts_and_dedups() {
        let evs = order_and_dedup(load("two_runs.json"));
        assert_eq!(evs.len(), 5, "duplicate (run A, seq 1) must be dropped");
        let run_a = evs[0].run_id.clone();
        assert!(
            evs[..3].iter().all(|e| e.run_id == run_a),
            "run A first (earlier first ts)"
        );
        assert_eq!(
            evs.iter()
                .filter(|e| e.run_id == run_a)
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            evs[3..].iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn detects_truncated_payload() {
        let evs = load("truncated.json");
        assert_eq!(is_truncated(&evs[0].payload), Some(70_000_000));
        assert_eq!(is_truncated(&serde_json::json!({"messages": []})), None);
    }

    #[test]
    fn groups_tool_calls_by_call_id_and_marks_missing_result() {
        let mut evs = order_and_dedup(load("basic.json"));
        let groups = group_tool_calls(&evs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].call_id, "c1");
        assert!(
            groups[0].response.is_some() && groups[0].call.is_some() && groups[0].result.is_some()
        );
        evs.retain(|e| e.kind != Kind::ToolResult);
        let groups = group_tool_calls(&evs);
        assert!(
            groups[0].result.is_none(),
            "missing tool_result must be visible"
        );
    }

    #[test]
    fn repeated_call_id_becomes_separate_attempts() {
        let base = order_and_dedup(load("basic.json"));
        let mut evs = base.clone();
        let mut again: Vec<AuditEvent> = base
            .iter()
            .filter(|e| e.call_id.as_deref() == Some("c1"))
            .cloned()
            .collect();
        for (i, e) in again.iter_mut().enumerate() {
            e.seq = 100 + i as u64;
            e.id = format!("dup-{i}");
        }
        evs.extend(again);
        let groups = group_tool_calls(&evs);
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].attempt, groups[1].attempt), (1, 2));
    }

    fn synth(kind: Kind, call_id: Option<&str>) -> AuditEvent {
        AuditEvent {
            v: 1,
            id: "id".to_string(),
            session_id: "s".to_string(),
            run_id: "r".to_string(),
            seq: 0,
            ts: Utc::now(),
            kind,
            provider: None,
            model: None,
            call_id: call_id.map(str::to_string),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn call_id_none_and_non_tool_kinds_are_skipped_tool_call_or_result_first_opens_a_group() {
        let evs = vec![
            // call_id が無いイベントは無視される
            synth(Kind::Usage, None),
            // call_id はあるが tool 関連でない種別は無視される
            synth(Kind::LlmRequest, Some("cx")),
            // 先に ToolCall が来る（LlmResponse なし）
            synth(Kind::ToolCall, Some("cy")),
            // 先に ToolResult が来る（LlmResponse も ToolCall もなし）
            synth(Kind::ToolResult, Some("cz")),
        ];
        let groups = group_tool_calls(&evs);
        assert_eq!(
            groups.len(),
            2,
            "cx は無視され、cy と cz のみグループになる"
        );
        assert_eq!(groups[0].call_id, "cy");
        assert!(groups[0].call.is_some() && groups[0].response.is_none());
        assert_eq!(groups[1].call_id, "cz");
        assert!(groups[1].result.is_some() && groups[1].call.is_none());
    }
}
