//! Request encoding + bridge response framing + the 16 kHz chunk math.

use tts_proto::{
    bytes_to_ms, chunk_count, encode_json_request, ms_to_bytes, parse_response_head, HeadParse,
    BYTES_PER_SEC, PLAY_CHUNK,
};

// ---------------------------------------------------------------------------
// JSON request encoding
// ---------------------------------------------------------------------------

#[test]
fn encodes_a_plain_request() {
    let b = encode_json_request("Garage door left open.").unwrap();
    assert_eq!(b.as_str(), r#"{"text":"Garage door left open."}"#);
}

#[test]
fn escapes_the_characters_that_would_break_the_document() {
    // A notification body is attacker-influenced (retained MQTT). An unescaped
    // quote would end the JSON string early and the bridge would 400.
    let b = encode_json_request(r#"He said "hi" \ bye"#).unwrap();
    assert_eq!(b.as_str(), r#"{"text":"He said \"hi\" \\ bye"}"#);
}

#[test]
fn escapes_control_characters() {
    let b = encode_json_request("a\nb\tc\rd\u{0b}e").unwrap();
    assert_eq!(b.as_str(), r#"{"text":"a\nb\tc\rd\u000be"}"#);
}

#[test]
fn a_json_injection_attempt_stays_inside_the_string() {
    // The classic: try to close the string and inject a sibling key.
    let evil = r#"x","voice":"evil-voice","junk":"#;
    let b = encode_json_request(evil).unwrap();
    // Exactly one unescaped quote pair delimits the value: the payload's own
    // quotes are all backslash-escaped.
    assert_eq!(b.as_str(), r#"{"text":"x\",\"voice\":\"evil-voice\",\"junk\":"}"#);
    // Sanity: no bare `","voice"` sequence survived.
    assert!(!b.as_str().contains(r#"","voice":"#));
}

#[test]
fn refuses_to_emit_a_truncated_document() {
    // Better a clean local skip than a half-written JSON doc the bridge 400s on.
    let huge = "a".repeat(10_000);
    assert!(encode_json_request(&huge).is_none());
}

#[test]
fn empty_text_still_encodes_validly() {
    assert_eq!(encode_json_request("").unwrap().as_str(), r#"{"text":""}"#);
}

// ---------------------------------------------------------------------------
// Response head framing
// ---------------------------------------------------------------------------

#[test]
fn parses_the_bridges_success_head() {
    let raw = b"HTTP/1.1 200 OK\r\n\
                Content-Type: application/octet-stream\r\n\
                Content-Length: 86400\r\n\
                X-Audio-Format: raw-16khz-16bit-mono-pcm\r\n\
                Connection: close\r\n\r\nPCMPCM";
    let HeadParse::Ok(h) = parse_response_head(raw) else {
        panic!("expected Ok");
    };
    assert_eq!(h.status, 200);
    assert_eq!(h.content_length, Some(86_400));
    assert_eq!(&raw[h.body_offset..], b"PCMPCM");
    // 86400 B of 16 kHz mono s16le = 2.7 s — matches the measured Azure clip.
    assert_eq!(h.duration_ms(), 2700);
}

#[test]
fn head_arriving_in_pieces_reports_incomplete_until_the_terminator() {
    let full = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nABCD";
    // Every strict prefix that lacks the CRLFCRLF must say Incomplete, never
    // Malformed — the socket read loop depends on that distinction.
    for n in 0..full.len() - 4 {
        assert_eq!(
            parse_response_head(&full[..n]),
            HeadParse::Incomplete,
            "prefix of {n} bytes"
        );
    }
    assert!(matches!(parse_response_head(full), HeadParse::Ok(_)));
}

#[test]
fn accepts_the_http_1_0_status_line_the_bridge_actually_sends() {
    // Captured from the real bridge: Python's BaseHTTPRequestHandler defaults to
    // HTTP/1.0, NOT 1.1. Every other test here was written against 1.1, so this
    // one exists because live traffic disagreed with the assumption.
    let raw = b"HTTP/1.0 200 OK\r\n\
                Server: BaseHTTP/0.6 Python/3.12.3\r\n\
                Content-Type: application/octet-stream\r\n\
                Content-Length: 126400\r\n\
                X-Audio-Format: raw-16khz-16bit-mono-pcm\r\n\r\n";
    let HeadParse::Ok(h) = parse_response_head(raw) else {
        panic!("HTTP/1.0 head rejected");
    };
    assert_eq!(h.status, 200);
    assert_eq!(h.content_length, Some(126_400));
    // 126400 B = 3.95 s — the measured length of the real notification clip.
    assert_eq!(h.duration_ms(), 3950);
}

#[test]
fn header_name_matching_is_case_insensitive() {
    let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 64\r\n\r\n";
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.content_length, Some(64));

    let raw = b"HTTP/1.1 200 OK\r\nCONTENT-LENGTH:   128  \r\n\r\n";
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.content_length, Some(128));
}

#[test]
fn surfaces_error_statuses_rather_than_treating_them_as_audio() {
    // The bridge reports Azure failures as 502 with a JSON body. If we mistook
    // that for PCM we would play ~40 ms of noise into the speaker.
    let raw = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 30\r\n\r\n{\"error\":\"azure: timeout\"}";
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.status, 502);

    let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 9\r\n\r\n";
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.status, 400);
}

#[test]
fn missing_content_length_means_read_until_close() {
    let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.content_length, None);
    assert_eq!(h.duration_ms(), 0);
}

#[test]
fn rejects_garbage_status_lines() {
    for raw in [
        &b"NOT-HTTP 200 OK\r\n\r\n"[..],
        &b"HTTP/1.1\r\n\r\n"[..],
        &b"HTTP/1.1 abc OK\r\n\r\n"[..],
        &b"\r\n\r\n"[..],
    ] {
        assert_eq!(parse_response_head(raw), HeadParse::Malformed, "raw={raw:?}");
    }
}

#[test]
fn absurd_content_length_does_not_wrap_or_panic() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999999\r\n\r\n";
    // Overflow -> None (unknown), never a wrapped small number that would make
    // us stop reading early.
    let HeadParse::Ok(h) = parse_response_head(raw) else { panic!() };
    assert_eq!(h.content_length, None);
}

#[test]
fn head_parsing_never_panics_on_arbitrary_bytes() {
    let samples: [&[u8]; 6] = [
        b"",
        b"\r\n\r\n",
        b"\0\0\0\0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n:\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
    ];
    for s in samples {
        let _ = parse_response_head(s);
    }
}

// ---------------------------------------------------------------------------
// Chunk math — the 32 B/ms constant every budget decision derives from
// ---------------------------------------------------------------------------

#[test]
fn audio_math_matches_the_project_format_contract() {
    assert_eq!(BYTES_PER_SEC, 32_000);
    // One PLAY_CHUNK is exactly 16 ms, matching audio_out::PLAY_CHUNK's docs.
    assert_eq!(bytes_to_ms(PLAY_CHUNK), 16);
    // The 8-slot playback queue is exactly 128 ms.
    assert_eq!(bytes_to_ms(PLAY_CHUNK * 8), 128);
    assert_eq!(ms_to_bytes(1000), 32_000);
    assert_eq!(ms_to_bytes(16), PLAY_CHUNK);
    // Round-trip.
    for ms in [0u32, 1, 16, 100, 1000, 10_000] {
        assert_eq!(bytes_to_ms(ms_to_bytes(ms)), ms);
    }
}

#[test]
fn chunk_count_covers_partial_tails() {
    assert_eq!(chunk_count(0), 0);
    assert_eq!(chunk_count(1), 1);
    assert_eq!(chunk_count(PLAY_CHUNK), 1);
    assert_eq!(chunk_count(PLAY_CHUNK + 1), 2);
    // The measured 2.7 s Azure clip.
    assert_eq!(chunk_count(86_400), 169);
}

#[test]
fn ms_to_bytes_always_yields_whole_samples() {
    for ms in 0..500u32 {
        assert_eq!(ms_to_bytes(ms) % 2, 0, "odd byte count at {ms} ms");
    }
}
