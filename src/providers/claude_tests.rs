use super::*;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn parses_recognized_windows_and_discards_sensitive_fields() {
    let input = br#"{
        "session_id":"secret-session", "transcript_path":"private/path",
        "rate_limits":{
            "five_hour":{"used_percentage":42.5,"resets_at":1780000000,"token":"secret"},
            "seven_day":{"used_percentage":17,"resets_at":1780500000},
            "future_limit":{"used_percentage":99}
        }
    }"#;
    let parsed = parse_status_line(input, NOW).unwrap();
    assert_eq!(parsed.snapshots.len(), 2);
    assert_eq!(parsed.snapshots[0].window_kind, WindowKind::Rolling5h);
    assert_eq!(parsed.snapshots[0].used_percent, 42.5);
    let debug = format!("{parsed:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("private/path"));
}

#[test]
fn missing_limits_and_unknown_fields_create_no_zero_rows() {
    let parsed = parse_status_line(br#"{"model":"claude","cost":99}"#, NOW).unwrap();
    assert!(parsed.snapshots.is_empty());
    assert_eq!(parsed.quality(), Quality::Partial);
}

#[test]
fn malformed_siblings_are_isolated() {
    let input = br#"{"rate_limits":{
        "five_hour":{"used_percentage":101,"resets_at":10},
        "seven_day":{"used_percentage":12,"resets_at":20},
        "spend_limit":{"used_percentage":"not-a-number"}
    }}"#;
    let parsed = parse_status_line(input, NOW).unwrap();
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].window_kind, WindowKind::Rolling7d);
    assert_eq!(parsed.rejected_windows, 2);
}

#[test]
fn spend_may_exceed_one_hundred_but_subscriptions_may_not() {
    let input = br#"{"rate_limits":{
        "five_hour":{"used_percentage":100.1},
        "spend_limit":{"used_percentage":145.25}
    }}"#;
    let parsed = parse_status_line(input, NOW).unwrap();
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].window_kind, WindowKind::Spend);
    assert_eq!(parsed.snapshots[0].used_percent, 145.25);
}

#[test]
fn nullable_or_missing_reset_is_partial_and_negative_is_rejected() {
    let input = br#"{"rate_limits":{
        "five_hour":{"used_percentage":1,"resets_at":null},
        "seven_day":{"used_percentage":2},
        "spend_limit":{"used_percentage":3,"resets_at":-1}
    }}"#;
    let parsed = parse_status_line(input, NOW).unwrap();
    assert_eq!(parsed.snapshots.len(), 2);
    assert!(parsed.snapshots.iter().all(|row| row.resets_at.is_none()));
    assert!(
        parsed
            .snapshots
            .iter()
            .all(|row| row.quality == Quality::Partial)
    );
    assert_eq!(parsed.rejected_windows, 1);
}

#[test]
fn rejects_malformed_multiple_and_oversized_documents_without_echoing_them() {
    assert_eq!(
        parse_status_line(br#"{"rate_limits":{}} trailing"#, NOW),
        Err(ClaudeParseError::InvalidDocument)
    );
    assert_eq!(
        parse_status_line(&vec![b'x'; MAX_STATUS_INPUT_BYTES + 1], NOW),
        Err(ClaudeParseError::Oversized)
    );
    assert!(
        !ClaudeParseError::InvalidDocument
            .to_string()
            .contains("trailing")
    );
}
