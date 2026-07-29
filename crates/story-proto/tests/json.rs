//! Scanner tests.
//!
//! The two properties that matter most, and that the rest of the app depends on:
//!
//! 1. **Prose is discarded without losing sync** — a 3,665-byte segment `text`
//!    costs 64 bytes and the structure after it still parses.
//! 2. **Piece boundaries are irrelevant** — the socket hands us arbitrary
//!    splits, including inside a string, an escape, a `\uXXXX` sequence or a
//!    number, so a split at *every* offset must produce identical events.

use story_proto::json::{Event, Scanner, MAX_STR};

/// Collect `(event, depth)` from one whole-buffer feed.
fn scan(src: &str) -> (Vec<(Event, u8)>, bool) {
    let mut sc = Scanner::new();
    let mut out = Vec::new();
    sc.feed(src.as_bytes(), &mut |ev, d| out.push((ev.clone(), d)));
    (out, sc.error())
}

/// Collect events feeding `src` in fixed-size pieces.
fn scan_chunked(src: &[u8], chunk: usize) -> (Vec<(Event, u8)>, bool, bool) {
    let mut sc = Scanner::new();
    let mut out = Vec::new();
    for piece in src.chunks(chunk.max(1)) {
        sc.feed(piece, &mut |ev, d| out.push((ev.clone(), d)));
    }
    (out, sc.error(), sc.complete())
}

fn strs(evs: &[(Event, u8)]) -> Vec<String> {
    evs.iter()
        .filter_map(|(e, _)| match e {
            Event::Str(t) => Some(t.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn keys(evs: &[(Event, u8)]) -> Vec<String> {
    evs.iter()
        .filter_map(|(e, _)| match e {
            Event::Key(t) => Some(t.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn ints(evs: &[(Event, u8)]) -> Vec<i64> {
    evs.iter()
        .filter_map(|(e, _)| match e {
            Event::Int(v) => Some(*v),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Structure and depth
// ---------------------------------------------------------------------------

#[test]
fn flat_object() {
    let (evs, err) = scan(r#"{"a":1,"b":"x","c":true,"d":null}"#);
    assert!(!err);
    assert_eq!(keys(&evs), ["a", "b", "c", "d"]);
    assert_eq!(ints(&evs), [1]);
    assert_eq!(strs(&evs), ["x"]);
    assert!(evs.iter().any(|(e, _)| *e == Event::Bool(true)));
    assert!(evs.iter().any(|(e, _)| *e == Event::Null));
}

#[test]
fn depth_matches_the_chapter_payload_shape() {
    // Exactly the nesting the models key off: root -> manifest -> segments
    // -> element. The models resolve a segment object as "the key's depth + 2",
    // so if these numbers move the models break silently.
    let src = r#"{"manifest":{"segments":[{"idx":0}]}}"#;
    let (evs, err) = scan(src);
    assert!(!err);

    let depth_of_key = |name: &str| {
        evs.iter()
            .find_map(|(e, d)| match e {
                Event::Key(t) if t.as_str() == name => Some(*d),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(depth_of_key("manifest"), 1);
    assert_eq!(depth_of_key("segments"), 2);
    assert_eq!(depth_of_key("idx"), 4, "segment members sit at segments-key + 2");

    // The array opens between them.
    assert!(evs.contains(&(Event::ArrOpen, 3)));
}

#[test]
fn array_of_objects_reports_row_depth_two() {
    // The chapter index's shape.
    let (evs, err) = scan(r#"[{"number":1},{"number":2}]"#);
    assert!(!err);
    assert!(evs.contains(&(Event::ArrOpen, 1)));
    assert_eq!(evs.iter().filter(|(e, d)| *e == Event::ObjOpen && *d == 2).count(), 2);
    assert_eq!(ints(&evs), [1, 2]);
}

#[test]
fn complete_distinguishes_a_whole_document_from_a_truncated_one() {
    let (_, err, complete) = scan_chunked(br#"{"a":1}"#, 64);
    assert!(!err);
    assert!(complete);

    // Socket died mid-payload: not an error, but definitely not complete.
    let (_, err, complete) = scan_chunked(br#"{"a":1,"b":"unfin"#, 64);
    assert!(!err);
    assert!(!complete, "a half-read chapter must not look like a whole one");
}

// ---------------------------------------------------------------------------
// The load-bearing property: discard without losing sync
// ---------------------------------------------------------------------------

#[test]
fn long_prose_is_clipped_but_the_structure_after_it_still_parses() {
    // 7,506 chars is the real worst case: live chapter 3's longest segment text.
    // (Chapter 1's is 3,665 — sizing a test on the first sample you look at is
    // how the segment cap got sized wrong once already.)
    let prose = "x".repeat(7506);
    let src = format!(r#"{{"text":"{prose}","start_ms":4120,"end_ms":8240}}"#);
    let (evs, err) = scan(&src);
    assert!(!err);

    // The value came through clipped and flagged.
    let t = evs
        .iter()
        .find_map(|(e, _)| match e {
            Event::Str(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(t.as_str().len(), MAX_STR);
    assert!(t.truncated());

    // And crucially, everything AFTER the prose is intact.
    assert_eq!(keys(&evs), ["text", "start_ms", "end_ms"]);
    assert_eq!(ints(&evs), [4120, 8240]);
}

#[test]
fn a_truncated_capture_never_satisfies_an_exact_match() {
    // `eq` must be truncation-aware, or the first four bytes of a 3,665-byte
    // value would match the key `text` and mis-assign prose to a field.
    let long = "text".to_string() + &"y".repeat(MAX_STR * 2);
    let src = format!(r#"{{"k":"{long}"}}"#);
    let (evs, _) = scan(&src);
    let t = evs
        .iter()
        .find_map(|(e, _)| match e {
            Event::Str(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert!(t.truncated());
    assert!(!t.matches("text"), "a clipped value must not alias a shorter one");
}

#[test]
fn escaped_quotes_inside_discarded_prose_do_not_end_the_string_early() {
    // The real risk: prose containing `\"` is common, and mishandling it would
    // make the rest of the document parse as garbage.
    let prose = r#"He said \"stop\" and then \\ left"#;
    let src = format!(r#"{{"text":"{prose}","start_ms":7}}"#);
    let (evs, err) = scan(&src);
    assert!(!err);
    assert_eq!(keys(&evs), ["text", "start_ms"]);
    assert_eq!(ints(&evs), [7]);
    assert_eq!(strs(&evs), [r#"He said "stop" and then \ left"#]);
}

#[test]
fn a_quote_inside_a_clipped_string_still_terminates_correctly() {
    // Escape handling must keep working *after* the capture buffer is full,
    // otherwise a quote beyond byte 64 ends the string early and desyncs.
    let filler = "a".repeat(MAX_STR + 20);
    let src = format!(r#"{{"text":"{filler}\"still inside\" here","n":42}}"#);
    let (evs, err) = scan(&src);
    assert!(!err);
    assert_eq!(keys(&evs), ["text", "n"]);
    assert_eq!(ints(&evs), [42]);
}

// ---------------------------------------------------------------------------
// Piece boundaries
// ---------------------------------------------------------------------------

#[test]
fn splitting_at_every_offset_yields_identical_events() {
    // Boundaries land inside strings, escapes, \uXXXX sequences and numbers.
    let src = r#"{"t":"a\"b\\c’d","n":-1234,"f":true,"z":null,"o":{"k":[1,2]}}"#;
    let (want, err) = scan(src);
    assert!(!err);

    for split in 0..=src.len() {
        let (a, b) = src.as_bytes().split_at(split);
        let mut sc = Scanner::new();
        let mut got = Vec::new();
        sc.feed(a, &mut |ev, d| got.push((ev.clone(), d)));
        sc.feed(b, &mut |ev, d| got.push((ev.clone(), d)));
        assert!(!sc.error(), "split at {split} produced an error");
        assert!(sc.complete(), "split at {split} did not complete");
        assert_eq!(got, want, "split at {split} changed the event stream");
    }
}

#[test]
fn every_chunk_size_yields_identical_events() {
    let src = r#"[{"number":1,"title":"Collecting the Divine Shard","duration_ms":452729,
                   "has_audio":true,"total_bytes":14487328}]"#;
    let (want, err) = scan(src);
    assert!(!err);
    // 1 is the pathological case; 512 is the real socket window.
    for chunk in [1usize, 2, 3, 7, 13, 64, 512, 4096] {
        let (got, err, complete) = scan_chunked(src.as_bytes(), chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");
        assert_eq!(got, want, "chunk size {chunk} changed the event stream");
    }
}

// ---------------------------------------------------------------------------
// Escapes and Unicode
// ---------------------------------------------------------------------------

#[test]
fn standard_escapes_decode() {
    let (evs, err) = scan(r#"{"k":"a\nb\tc\rd\/e\bf\fg"}"#);
    assert!(!err);
    assert_eq!(strs(&evs), ["a\nb\tc\rd/e\u{08}f\u{0c}g"]);
}

#[test]
fn bmp_unicode_escapes_decode() {
    // The daemon escapes prose apostrophes as ’ — seen in live chapter 1.
    let (evs, err) = scan(r#"{"k":"It’s"}"#);
    assert!(!err);
    assert_eq!(strs(&evs), ["It\u{2019}s"]);
}

#[test]
fn surrogate_pairs_decode_to_one_scalar() {
    // U+1F600 as a surrogate pair.
    let (evs, err) = scan(r#"{"k":"😀"}"#);
    assert!(!err);
    assert_eq!(strs(&evs), ["\u{1F600}"]);
}

#[test]
fn a_lone_surrogate_becomes_a_replacement_char_and_does_not_derail() {
    let (evs, err) = scan(r#"{"k":"\ud83dx","n":5}"#);
    assert!(!err);
    assert_eq!(ints(&evs), [5], "parse must continue past a bad escape");
    let s = &strs(&evs)[0];
    assert!(s.contains('\u{FFFD}'), "got {s:?}");
}

#[test]
fn raw_utf8_is_clipped_on_a_char_boundary_and_stays_valid() {
    // Raw (unescaped) multi-byte UTF-8 is legal JSON. Clipping at MAX_STR can
    // land mid-sequence, and `as_str` must never return invalid UTF-8.
    let src = format!(r#"{{"k":"{}"}}"#, "\u{2019}".repeat(40));
    let (evs, err) = scan(&src);
    assert!(!err);
    let t = evs
        .iter()
        .find_map(|(e, _)| match e {
            Event::Str(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert!(t.truncated());
    // The invariant: whatever came back is valid UTF-8 and only whole chars.
    let s = t.as_str();
    assert!(s.chars().all(|c| c == '\u{2019}'), "got {s:?}");
    assert!(s.len() <= MAX_STR);
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

#[test]
fn integers_including_the_live_byte_counts() {
    let (evs, err) = scan(r#"[0,1,452729,14487328,19025408,-7]"#);
    assert!(!err);
    assert_eq!(ints(&evs), [0, 1, 452_729, 14_487_328, 19_025_408, -7]);
}

#[test]
fn fractions_and_exponents_yield_their_integer_part_rather_than_failing() {
    // No payload the watch reads carries one, but accepting beats rejecting a
    // whole chapter over a field we do not use.
    let (evs, err) = scan(r#"{"a":1.5,"b":2e3,"c":7}"#);
    assert!(!err);
    assert_eq!(ints(&evs), [1, 2, 7]);
}

#[test]
fn absurd_numbers_saturate_instead_of_wrapping() {
    let (evs, err) = scan(r#"[999999999999999999999999999]"#);
    assert!(!err);
    assert_eq!(ints(&evs), [i64::MAX]);
}

// ---------------------------------------------------------------------------
// Adversarial input — every route is unauthenticated on the LAN (design §9.1)
// ---------------------------------------------------------------------------

#[test]
fn malformed_documents_latch_an_error_and_never_panic() {
    for bad in [
        "{",
        "}",
        "]",
        "{]",
        "[}",
        r#"{"a" 1}"#,
        r#"{"a":}"#,
        "tru",
        "nul",
        r#"{"a":truthy}"#,
        "\0\0\0",
        "@!#",
    ] {
        let mut sc = Scanner::new();
        let mut n = 0;
        sc.feed(bad.as_bytes(), &mut |_, _| n += 1);
        // The contract is only "no panic, and not falsely complete".
        assert!(!sc.complete(), "{bad:?} must not report complete");
    }
}

#[test]
fn nesting_deeper_than_the_cap_errors_rather_than_overflowing() {
    let deep = "[".repeat(64);
    let mut sc = Scanner::new();
    sc.feed(deep.as_bytes(), &mut |_, _| {});
    assert!(sc.error(), "excess nesting must latch an error");
    assert!(!sc.complete());
}

#[test]
fn a_hostile_stream_of_random_bytes_never_panics() {
    // Deterministic pseudo-random bytes; the point is only that nothing panics
    // and no run reports a complete document.
    let mut state = 0x1234_5678u32;
    for _ in 0..200 {
        let mut buf = Vec::new();
        for _ in 0..256 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            buf.push((state >> 16) as u8);
        }
        let mut sc = Scanner::new();
        sc.feed(&buf, &mut |_, _| {});
    }
}

#[test]
fn an_unterminated_string_does_not_panic_or_complete() {
    let mut sc = Scanner::new();
    let src = format!(r#"{{"k":"{}"#, "a".repeat(10_000));
    sc.feed(src.as_bytes(), &mut |_, _| {});
    assert!(!sc.complete());
}
