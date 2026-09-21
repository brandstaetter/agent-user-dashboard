use crate::domain::{Availability, LimitKind, Provider, Quality, WindowKind};

use super::{
    AntigravityParseError, MAX_STATUS_INPUT_BYTES, ParsedAntigravityStatus, parse_status_line,
};

const OBSERVED_AT: i64 = 1_700_000_000_123;

fn parse(document: &str) -> ParsedAntigravityStatus {
    parse_status_line(document.as_bytes(), OBSERVED_AT).expect("fixture should be accepted")
}

fn one_bucket(scope: &str, body: &str) -> ParsedAntigravityStatus {
    parse(&format!(r#"{{"quota":{{"{scope}":{body}}}}}"#))
}

fn assert_sanitized(error: AntigravityParseError, sentinels: &[&str]) {
    let rendered = format!("{error}");
    let debug = format!("{error:?}");
    for sentinel in sentinels {
        assert!(!rendered.contains(sentinel));
        assert!(!debug.contains(sentinel));
    }
}

#[test]
fn parses_multiple_dynamic_buckets_into_exact_normalized_fields() {
    let parsed = parse(
        r#"{
            "product":"antigravity",
            "version":"agy-1.2.3+stable",
            "quota":{
                "gemini-weekly":{
                    "remaining_fraction":0.25,
                    "reset_time":"2023-11-14T22:15:20Z"
                },
                "flash_daily":{
                    "remaining_fraction":0.75,
                    "reset_in_seconds":120
                }
            }
        }"#,
    );

    assert_eq!(parsed.rejected_buckets, 0);
    assert_eq!(parsed.snapshots.len(), 2);
    assert_eq!(parsed.quality(), Quality::Partial);

    let daily = &parsed.snapshots[0];
    assert_eq!(daily.provider, Provider::GoogleAntigravity);
    assert_eq!(daily.scope_key, "flash_daily");
    assert_eq!(daily.window_kind, WindowKind::Other);
    assert_eq!(daily.limit_kind, LimitKind::Limited);
    assert_eq!(daily.window_duration_seconds, None);
    assert_eq!(daily.used_percent, 25.0);
    assert_eq!(daily.resets_at, Some(1_700_000_120));
    assert_eq!(daily.observed_at, OBSERVED_AT);
    assert_eq!(daily.availability, Availability::Allowed);
    assert_eq!(daily.source_version.as_deref(), Some("agy-1.2.3+stable"));
    assert_eq!(daily.quality, Quality::Partial);

    let weekly = &parsed.snapshots[1];
    assert_eq!(weekly.scope_key, "gemini-weekly");
    assert_eq!(weekly.used_percent, 75.0);
    assert_eq!(weekly.resets_at, Some(1_700_000_120));
    assert_eq!(weekly.source_sequence, daily.source_sequence);
}

#[test]
fn accepts_exact_size_limit_and_rejects_one_byte_over_before_parsing() {
    let prefix = br#"{"quota":{},"padding":""#;
    let suffix = br#""}"#;
    let padding_len = MAX_STATUS_INPUT_BYTES - prefix.len() - suffix.len();
    let mut exact = Vec::with_capacity(MAX_STATUS_INPUT_BYTES);
    exact.extend_from_slice(prefix);
    exact.extend(std::iter::repeat_n(b'a', padding_len));
    exact.extend_from_slice(suffix);
    assert_eq!(exact.len(), MAX_STATUS_INPUT_BYTES);

    let parsed = parse_status_line(&exact, OBSERVED_AT).expect("the byte limit is inclusive");
    assert!(parsed.snapshots.is_empty());
    assert_eq!(parsed.rejected_buckets, 0);

    let oversized_invalid = vec![b'S'; MAX_STATUS_INPUT_BYTES + 1];
    assert_eq!(
        parse_status_line(&oversized_invalid, OBSERVED_AT),
        Err(AntigravityParseError::Oversized)
    );
}

#[test]
fn rejects_invalid_json_and_non_object_top_levels_with_sanitized_errors() {
    for document in [b"not-json".as_slice(), br#"[]"#, br#""string""#] {
        let error = parse_status_line(document, OBSERVED_AT).unwrap_err();
        assert_eq!(error, AntigravityParseError::InvalidDocument);
        assert_sanitized(error, &["not-json", "string"]);
    }
}

#[test]
fn product_absent_null_and_exact_are_accepted() {
    for document in [
        r#"{"quota":{}}"#,
        r#"{"product":null,"quota":{}}"#,
        r#"{"product":"antigravity","quota":{}}"#,
    ] {
        let parsed = parse(document);
        assert!(parsed.snapshots.is_empty());
        assert_eq!(parsed.rejected_buckets, 0);
    }
}

#[test]
fn wrong_and_type_invalid_products_are_sanitized_document_errors() {
    let secret = "private-product-sentinel@example.invalid";
    let wrong = parse_status_line(
        format!(r#"{{"product":"{secret}","quota":{{}}}}"#).as_bytes(),
        OBSERVED_AT,
    )
    .unwrap_err();
    assert_eq!(wrong, AntigravityParseError::UnexpectedProduct);
    assert_sanitized(wrong, &[secret]);

    let invalid = parse_status_line(br#"{"product":42,"quota":{}}"#, OBSERVED_AT).unwrap_err();
    assert_eq!(invalid, AntigravityParseError::InvalidDocument);
    assert_sanitized(invalid, &["42"]);
}

#[test]
fn quota_absent_null_and_empty_are_successful_empty_observations() {
    for document in [r#"{}"#, r#"{"quota":null}"#, r#"{"quota":{}}"#] {
        let parsed = parse(document);
        assert!(parsed.snapshots.is_empty());
        assert_eq!(parsed.rejected_buckets, 0);
        assert_eq!(parsed.quality(), Quality::Partial);
    }
}

#[test]
fn scalar_and_array_quota_values_are_document_errors() {
    for document in [r#"{"quota":1}"#, r#"{"quota":[]}"#] {
        assert_eq!(
            parse_status_line(document.as_bytes(), OBSERVED_AT),
            Err(AntigravityParseError::InvalidDocument)
        );
    }
}

#[test]
fn bucket_key_allowlist_accepts_boundary_lengths_and_safe_punctuation() {
    let sixty_four = "a".repeat(64);
    for key in ["a", "a.b-c_d", sixty_four.as_str()] {
        let parsed = one_bucket(key, r#"{"remaining_fraction":0.5}"#);
        assert_eq!(parsed.snapshots.len(), 1);
        assert_eq!(parsed.snapshots[0].scope_key, key);
        assert_eq!(parsed.rejected_buckets, 0);
    }
}

#[test]
fn bucket_key_allowlist_rejects_empty_long_bad_prefix_bad_character_and_unicode() {
    let sixty_five = "a".repeat(65);
    for key in ["", sixty_five.as_str(), "-leading", "bad!char", "café"] {
        let parsed = one_bucket(key, r#"{"remaining_fraction":0.5}"#);
        assert!(parsed.snapshots.is_empty());
        assert_eq!(parsed.rejected_buckets, 1);
    }
}

#[test]
fn fraction_boundaries_map_to_used_percentage() {
    let parsed = parse(
        r#"{"quota":{
            "none-left":{"remaining_fraction":0},
            "all-left":{"remaining_fraction":1}
        }}"#,
    );
    assert_eq!(parsed.snapshots.len(), 2);
    assert_eq!(parsed.snapshots[0].scope_key, "all-left");
    assert_eq!(parsed.snapshots[0].used_percent, 0.0);
    assert_eq!(parsed.snapshots[0].availability, Availability::Allowed);
    assert_eq!(parsed.snapshots[1].scope_key, "none-left");
    assert_eq!(parsed.snapshots[1].used_percent, 100.0);
    assert_eq!(parsed.snapshots[1].availability, Availability::Blocked);
}

#[test]
fn invalid_fractions_are_rejected_independently() {
    let parsed = parse(
        r#"{"quota":{
            "good":{"remaining_fraction":0.5},
            "missing":{},
            "wrong":{"remaining_fraction":"private-value"},
            "overflow":{"remaining_fraction":1e400},
            "negative":{"remaining_fraction":-0.01},
            "above":{"remaining_fraction":1.01}
        }}"#,
    );
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].scope_key, "good");
    assert_eq!(parsed.rejected_buckets, 5);
    assert!(!format!("{parsed:?}").contains("private-value"));
}

#[test]
fn absolute_reset_is_parsed_to_unix_seconds() {
    let parsed = one_bucket(
        "weekly",
        r#"{"remaining_fraction":0.5,"reset_time":"2023-11-14T22:15:20Z"}"#,
    );
    assert_eq!(parsed.snapshots[0].resets_at, Some(1_700_000_120));
    assert_eq!(parsed.snapshots[0].quality, Quality::Fresh);
}

#[test]
fn relative_reset_uses_observed_at_milliseconds_and_invalid_absolute_falls_back() {
    let parsed = one_bucket(
        "weekly",
        r#"{
            "remaining_fraction":0.5,
            "reset_time":"not-a-timestamp",
            "reset_in_seconds":90
        }"#,
    );
    assert_eq!(parsed.snapshots[0].resets_at, Some(1_700_000_090));
    assert_eq!(parsed.snapshots[0].quality, Quality::Partial);
}

#[test]
fn negative_and_overflowing_relative_resets_are_rejected() {
    let negative = one_bucket(
        "negative",
        r#"{"remaining_fraction":0.5,"reset_in_seconds":-1}"#,
    );
    assert!(negative.snapshots.is_empty());
    assert_eq!(negative.rejected_buckets, 1);

    let overflow = parse_status_line(
        br#"{"quota":{"overflow":{"remaining_fraction":0.5,"reset_in_seconds":9223372036854775807}}}"#,
        i64::MAX,
    )
    .unwrap();
    assert!(overflow.snapshots.is_empty());
    assert_eq!(overflow.rejected_buckets, 1);
}

#[test]
fn reset_sources_accept_sixty_seconds_as_fresh_and_sixty_one_as_partial() {
    let accepted = one_bucket(
        "accepted",
        r#"{
            "remaining_fraction":0.5,
            "reset_time":"2023-11-14T22:15:20Z",
            "reset_in_seconds":60
        }"#,
    );
    assert_eq!(accepted.snapshots.len(), 1);
    assert_eq!(accepted.snapshots[0].resets_at, Some(1_700_000_120));
    assert_eq!(accepted.snapshots[0].quality, Quality::Fresh);

    let partial = one_bucket(
        "partial",
        r#"{
            "remaining_fraction":0.5,
            "reset_time":"2023-11-14T22:15:21Z",
            "reset_in_seconds":60
        }"#,
    );
    assert_eq!(partial.snapshots.len(), 1);
    assert_eq!(partial.rejected_buckets, 0);
    assert_eq!(partial.snapshots[0].resets_at, Some(1_700_000_121));
    assert_eq!(partial.snapshots[0].quality, Quality::Partial);
}

#[test]
fn missing_resets_are_accepted_as_partial() {
    let parsed = one_bucket("weekly", r#"{"remaining_fraction":0.5}"#);
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.snapshots[0].resets_at, None);
    assert_eq!(parsed.snapshots[0].quality, Quality::Partial);
    assert_eq!(parsed.quality(), Quality::Partial);
}

#[test]
fn valid_version_boundaries_are_preserved() {
    let sixty_four = format!("v{}", "1".repeat(63));
    for version in ["v1.2.3-beta+7", sixty_four.as_str()] {
        let parsed = parse(&format!(
            r#"{{"version":"{version}","quota":{{"weekly":{{"remaining_fraction":0.5}}}}}}"#
        ));
        assert_eq!(parsed.snapshots[0].source_version.as_deref(), Some(version));
    }
}

#[test]
fn invalid_versions_are_discarded_without_rejecting_good_buckets() {
    let sixty_five = "v".repeat(65);
    for version in ["", sixty_five.as_str(), "bad version", "vérsion"] {
        let parsed = parse(&format!(
            r#"{{"version":"{version}","quota":{{"weekly":{{"remaining_fraction":0.5}}}}}}"#
        ));
        assert_eq!(parsed.snapshots.len(), 1);
        assert_eq!(parsed.rejected_buckets, 0);
        assert_eq!(parsed.snapshots[0].source_version, None);
    }
}

#[test]
fn non_string_versions_are_discarded_without_rejecting_good_buckets() {
    for version in ["42", "false", r#"{"private":"value"}"#] {
        let parsed = parse(&format!(
            r#"{{"version":{version},"quota":{{"weekly":{{"remaining_fraction":0.5}}}}}}"#
        ));
        assert_eq!(parsed.snapshots.len(), 1);
        assert_eq!(parsed.rejected_buckets, 0);
        assert_eq!(parsed.snapshots[0].source_version, None);
        assert!(!format!("{parsed:?}").contains("private"));
    }
}

#[test]
fn malformed_siblings_are_isolated_with_common_observation_identity() {
    let parsed = parse(
        r#"{"quota":{
            "alpha":{"remaining_fraction":0.2},
            "broken":"private-malformed-value",
            "beta":{"remaining_fraction":0.8,"reset_in_seconds":15},
            "also-broken":{"remaining_fraction":false}
        }}"#,
    );
    assert_eq!(parsed.snapshots.len(), 2);
    assert_eq!(parsed.rejected_buckets, 2);
    assert!(
        parsed
            .snapshots
            .iter()
            .all(|snapshot| snapshot.observed_at == OBSERVED_AT)
    );
    assert_eq!(
        parsed.snapshots[0].source_sequence,
        parsed.snapshots[1].source_sequence
    );
    assert!(!format!("{parsed:?}").contains("private-malformed-value"));
}

#[test]
fn aggregate_quality_requires_nonempty_all_fresh_and_no_rejections() {
    let fresh = one_bucket(
        "fresh",
        r#"{"remaining_fraction":0.5,"reset_time":"2023-11-14T22:15:20Z"}"#,
    );
    assert_eq!(fresh.quality(), Quality::Fresh);

    let partial_row = one_bucket("partial", r#"{"remaining_fraction":0.5}"#);
    assert_eq!(partial_row.quality(), Quality::Partial);

    let rejected_sibling = parse(
        r#"{"quota":{
            "fresh":{"remaining_fraction":0.5,"reset_in_seconds":1},
            "bad":null
        }}"#,
    );
    assert_eq!(rejected_sibling.quality(), Quality::Partial);

    assert_eq!(parse(r#"{"quota":{}}"#).quality(), Quality::Partial);
}

#[test]
fn privacy_sentinels_never_cross_the_parser_boundary_or_errors() {
    let sentinels = [
        "person@example.invalid",
        "account-12345",
        "enterprise-plan-secret",
        "C:/secret/workspace/project",
        "conversation-secret",
        "session-secret",
        "transcript-secret",
        "git-private-branch",
        "private-model-name",
        "private-context-window",
        "malformed-private-value",
    ];
    let parsed = parse(
        r#"{
            "email":"person@example.invalid",
            "account":"account-12345",
            "plan":"enterprise-plan-secret",
            "workspace":"C:/secret/workspace/project",
            "project":"C:/secret/workspace/project",
            "conversation":"conversation-secret",
            "session":"session-secret",
            "transcript":"transcript-secret",
            "vcs":"git-private-branch",
            "model":"private-model-name",
            "context_window":"private-context-window",
            "quota":{
                "safe":{"remaining_fraction":0.4,"unknown":"person@example.invalid"},
                "invalid":{"remaining_fraction":"malformed-private-value"}
            }
        }"#,
    );
    assert_eq!(parsed.snapshots.len(), 1);
    assert_eq!(parsed.rejected_buckets, 1);
    let normalized_debug = format!("{parsed:?}");
    for sentinel in sentinels {
        assert!(!normalized_debug.contains(sentinel));
    }

    let invalid_document = b"person@example.invalid{";
    let error = parse_status_line(invalid_document, OBSERVED_AT).unwrap_err();
    assert_eq!(error, AntigravityParseError::InvalidDocument);
    assert_sanitized(error, &sentinels);
}
