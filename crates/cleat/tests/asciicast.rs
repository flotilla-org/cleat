use std::time::Duration;

use cleat::asciicast::{decode_event, decode_header, encode_event, encode_header, CleatMeta, Event, EventCode, Header};

#[test]
fn header_round_trips_with_cleat_metadata() {
    let header = Header {
        cols: 120,
        rows: 40,
        timestamp: Some(1_700_000_000),
        term_type: Some("xterm-256color".to_string()),
        title: Some("My Session".to_string()),
        cleat: Some(CleatMeta { version: "0.1.0".to_string(), build: Some("abc1234".to_string()), engine: "native".to_string() }),
    };

    let encoded = encode_header(&header);
    let decoded = decode_header(&encoded).expect("decode header");

    assert_eq!(decoded.cols, 120);
    assert_eq!(decoded.rows, 40);
    assert_eq!(decoded.timestamp, Some(1_700_000_000));
    assert_eq!(decoded.term_type.as_deref(), Some("xterm-256color"));
    assert_eq!(decoded.title.as_deref(), Some("My Session"));

    let cleat_meta = decoded.cleat.expect("cleat meta present");
    assert_eq!(cleat_meta.version, "0.1.0");
    assert_eq!(cleat_meta.build.as_deref(), Some("abc1234"));
    assert_eq!(cleat_meta.engine, "native");
}

#[test]
fn output_event_round_trips() {
    let event = Event { time: Duration::from_millis(1234), code: EventCode::Output, data: "\x1b[32mHello\x1b[0m".to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    assert_eq!(prev, Duration::from_millis(1234));

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode output event");

    assert_eq!(decoded.time, Duration::from_millis(1234));
    assert_eq!(decoded.code, EventCode::Output);
    assert_eq!(decoded.data, "\x1b[32mHello\x1b[0m");
}

#[test]
fn input_event_round_trips() {
    let event = Event { time: Duration::from_millis(500), code: EventCode::Input, data: "hello".to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode input event");

    assert_eq!(decoded.time, Duration::from_millis(500));
    assert_eq!(decoded.code, EventCode::Input);
    assert_eq!(decoded.data, "hello");
}

#[test]
fn resize_event_round_trips() {
    let event = Event { time: Duration::from_millis(300), code: EventCode::Resize, data: "100x40".to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode resize event");

    assert_eq!(decoded.time, Duration::from_millis(300));
    assert_eq!(decoded.code, EventCode::Resize);
    assert_eq!(decoded.data, "100x40");
}

#[test]
fn marker_event_round_trips() {
    let event = Event { time: Duration::from_millis(750), code: EventCode::Marker, data: "test-start".to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode marker event");

    assert_eq!(decoded.time, Duration::from_millis(750));
    assert_eq!(decoded.code, EventCode::Marker);
    assert_eq!(decoded.data, "test-start");
}

#[test]
fn exit_event_round_trips() {
    let event = Event { time: Duration::from_millis(5000), code: EventCode::Exit, data: "0".to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode exit event");

    assert_eq!(decoded.time, Duration::from_millis(5000));
    assert_eq!(decoded.code, EventCode::Exit);
    assert_eq!(decoded.data, "0");
}

#[test]
fn custom_event_code_round_trips() {
    let event = Event { time: Duration::from_millis(2000), code: EventCode::Custom('S'), data: r#"{"key":"value"}"#.to_string() };

    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);

    let mut prev2 = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut prev2).expect("decode custom event");

    assert_eq!(decoded.time, Duration::from_millis(2000));
    assert_eq!(decoded.code, EventCode::Custom('S'));
    assert_eq!(decoded.data, r#"{"key":"value"}"#);
}

#[test]
fn v3_timing_uses_deltas_not_absolute() {
    let event1 = Event { time: Duration::from_millis(1000), code: EventCode::Output, data: "first".to_string() };
    let event2 = Event { time: Duration::from_millis(2500), code: EventCode::Output, data: "second".to_string() };

    let mut prev = Duration::ZERO;
    let line1 = encode_event(&event1, &mut prev);
    let line2 = encode_event(&event2, &mut prev);

    // Parse the raw JSON to check the delta values
    let arr1: serde_json::Value = serde_json::from_str(&line1).expect("parse event1 json");
    let arr2: serde_json::Value = serde_json::from_str(&line2).expect("parse event2 json");

    let delta1 = arr1[0].as_f64().expect("delta1 is f64");
    let delta2 = arr2[0].as_f64().expect("delta2 is f64");

    // delta1 should be 1.0 (1000ms from start)
    assert!((delta1 - 1.0).abs() < 0.001, "expected delta1=1.0, got {delta1}");
    // delta2 should be 1.5 (1500ms since event1), NOT 2.5
    assert!((delta2 - 1.5).abs() < 0.001, "expected delta2=1.5, got {delta2}");
}

#[test]
fn unknown_event_code_decoded_as_custom() {
    let line = r#"[0.5, "Z", "some data"]"#;
    let mut prev = Duration::ZERO;
    let event = decode_event(line, &mut prev).expect("decode unknown event code");

    assert_eq!(event.time, Duration::from_millis(500));
    assert_eq!(event.code, EventCode::Custom('Z'));
    assert_eq!(event.data, "some data");
}
