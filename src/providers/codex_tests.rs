use std::{collections::VecDeque, time::Duration};

use serde_json::{Value, json};

use super::*;
use crate::domain::{Availability, WindowKind};

const NOW: i64 = 1_700_000_000_000;

#[derive(Default)]
struct FakeTransport {
    incoming: VecDeque<Result<Vec<u8>, TransportError>>,
    sent: Vec<Value>,
    shutdown: bool,
}

impl FakeTransport {
    fn with_messages(messages: impl IntoIterator<Item = Value>) -> Self {
        Self {
            incoming: messages
                .into_iter()
                .map(|message| Ok(serde_json::to_vec(&message).unwrap()))
                .collect(),
            ..Self::default()
        }
    }
}

impl LineTransport for FakeTransport {
    fn send_line(&mut self, line: &[u8]) -> Result<(), TransportError> {
        self.sent.push(serde_json::from_slice(line).unwrap());
        Ok(())
    }

    fn receive_line(&mut self, _timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.incoming
            .pop_front()
            .unwrap_or(Err(TransportError::Timeout))
    }

    fn shutdown(&mut self) {
        self.shutdown = true;
    }
}

fn complete_result() -> Value {
    json!({"rateLimits":{"ordinaryUsageAllowed":true,"primary":{
        "usedPercent":25,"windowDurationMins":300,"resetsAt":1800000000
    }}})
}

#[test]
fn handshake_order_is_exact_and_only_read_only_methods_are_sent() {
    let mut transport = FakeTransport::with_messages([
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":complete_result()}),
    ]);
    let result =
        query_with_transport(&mut transport, NOW, Some("0.154.0"), Duration::from_secs(1)).unwrap();
    assert_eq!(result.snapshots.len(), 1);
    let methods = transport
        .sent
        .iter()
        .map(|message| message["method"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["initialize", "initialized", "account/rateLimits/read"]
    );
    assert_eq!(transport.sent[0]["id"], 1);
    assert!(transport.sent[1].get("id").is_none());
    assert!(transport.sent[2].get("params").is_none());
    assert!(transport.shutdown);
    for forbidden in ["login", "auth", "email", "credit", "reset", "consume"] {
        assert!(!methods.iter().any(|method| method.contains(forbidden)));
    }
}

#[test]
fn correlates_ids_and_ignores_notifications_and_other_responses() {
    let mut transport = FakeTransport::with_messages([
        json!({"method":"unrelated/notification","params":{"secret":"discard"}}),
        json!({"id":99,"result":{"secret":"discard"}}),
        json!({"id":"1","result":{}}),
        json!({"id":77,"result":{}}),
        json!({"id":"2","result":complete_result()}),
    ]);
    let result = query_with_transport(&mut transport, NOW, None, Duration::from_secs(1)).unwrap();
    assert_eq!(result.snapshots[0].used_percent, 25.0);
}

#[test]
fn timeout_oversize_and_rpc_errors_are_sanitized_and_shutdown() {
    let mut timeout = FakeTransport::default();
    let error = query_with_transport(&mut timeout, NOW, None, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(error, CodexError::Timeout { request_id: 1, .. }));
    assert!(timeout.shutdown);

    let mut oversized = FakeTransport {
        incoming: [Err(TransportError::Oversized)].into(),
        ..FakeTransport::default()
    };
    let error =
        query_with_transport(&mut oversized, NOW, None, Duration::from_secs(1)).unwrap_err();
    assert!(!error.to_string().contains("payload"));

    let mut rpc = FakeTransport::with_messages([json!({"id":1,"error":{
        "message":"token secret", "data":{"accountId":"private"}
    }})]);
    let error = query_with_transport(&mut rpc, NOW, None, Duration::from_secs(1)).unwrap_err();
    let display = error.to_string();
    assert!(!display.contains("secret"));
    assert!(!display.contains("private"));
}

#[test]
fn parses_multiple_buckets_classifies_duration_and_sanitizes_scope() {
    let result = json!({"rateLimitsByLimitId":{
        "team / unsafe":{
            "ordinaryUsageAllowed":false,
            "primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":100},
            "secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":200}
        },
        "other":{
            "primary":{"usedPercent":30,"windowDurationMins":60,"resetsAt":300}
        }
    }});
    let parsed = parse_rate_limits_result(&result, NOW, Some("0.154.0"));
    assert_eq!(parsed.snapshots.len(), 3);
    let five_hour = parsed
        .snapshots
        .iter()
        .find(|row| row.window_kind == WindowKind::Rolling5h)
        .unwrap();
    let weekly = parsed
        .snapshots
        .iter()
        .find(|row| row.window_kind == WindowKind::Rolling7d)
        .unwrap();
    let other = parsed
        .snapshots
        .iter()
        .find(|row| row.window_kind == WindowKind::Other)
        .unwrap();
    assert_eq!(five_hour.scope_key, "team___unsafe");
    assert_eq!(weekly.scope_key, "team___unsafe");
    assert_eq!(other.window_duration_seconds, Some(3600));
    assert_eq!(five_hour.availability, Availability::Blocked);
}

#[test]
fn malformed_siblings_survive_and_legacy_is_used_only_as_fallback() {
    let result = json!({
        "rateLimitsByLimitId":{"bucket":{
            "primary":{"usedPercent":101,"windowDurationMins":300,"resetsAt":1},
            "secondary":{"usedPercent":22,"windowDurationMins":10080,"resetsAt":2}
        }},
        "rateLimits":{"primary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":3}}
    });
    let parsed = parse_rate_limits_result(&result, NOW, None);
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].scope_key, "bucket");
    assert_eq!(parsed.snapshots[0].used_percent, 22.0);
    assert_eq!(parsed.rejected_windows, 1);

    let fallback = json!({
        "rateLimitsByLimitId":{"bad":"malformed"},
        "rateLimits":{"primary":{"usedPercent":7,"windowDurationMins":300,"resetsAt":4}}
    });
    let parsed = parse_rate_limits_result(&fallback, NOW, None);
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].scope_key, "subscription");
}

#[test]
fn null_windows_and_empty_results_are_successful_partial_observations() {
    let parsed = parse_rate_limits_result(
        &json!({"rateLimits":{"primary":null,"secondary":null}}),
        NOW,
        None,
    );
    assert!(parsed.snapshots.is_empty());
    assert_eq!(parsed.rejected_windows, 0);
    assert_eq!(parsed.quality(), Quality::Partial);
}

#[test]
fn malformed_optional_fields_are_isolated_and_unknown_sensitive_fields_do_not_surface() {
    let result = json!({"accountId":"private-account","creditDetails":{"token":"secret"},
    "rateLimits":{"ordinaryUsageAllowed":true,
        "primary":{"usedPercent":9,"windowDurationMins":300,"resetsAt":null,
            "prompt":"sensitive"},
        "secondary":{"usedPercent":8,"windowDurationMins":-1,"resetsAt":2}
    }});
    let parsed = parse_rate_limits_result(&result, NOW, None);
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].resets_at, None);
    assert_eq!(parsed.snapshots[0].quality, Quality::Partial);
    assert_eq!(parsed.rejected_windows, 1);
    let debug = format!("{parsed:?}");
    assert!(!debug.contains("private-account"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("sensitive"));
}
