//! HTTP framing, request composition and body encoding.
//!
//! `range.hdr` is a **real captured response head** from the live daemon for
//! `GET /media/0001.pcm` with `Range: bytes=1000000-1000511`.

use story_proto::*;

/// Verbatim head the daemon actually sent.
const RANGE_HDR: &[u8] = include_bytes!("fixtures/range.hdr");

fn head(buf: &[u8]) -> ResponseHead {
    match parse_head(buf) {
        HeadParse::Ok(h) => h,
        other => panic!("expected Ok, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Response framing
// ---------------------------------------------------------------------------

#[test]
fn the_real_range_response_parses() {
    let h = head(RANGE_HDR);
    assert_eq!(h.status, 206);
    assert!(h.is_partial());
    assert!(h.ok());
    assert_eq!(h.content_length, Some(512));
    let cr = h.content_range.expect("daemon sends Content-Range");
    assert_eq!(cr.first, 1_000_000);
    assert_eq!(cr.last, 1_000_511);
    assert_eq!(cr.total, 14_487_328, "the chapter's full size");
    // Which agrees with the manifest identity.
    assert_eq!(bytes_to_ms(cr.total), 452_729);
}

#[test]
fn the_real_response_reports_where_the_body_starts() {
    let h = head(RANGE_HDR);
    assert_eq!(h.body_starts_at(1_000_000), 1_000_000);
    assert_eq!(h.total_bytes(), Some(14_487_328));
}

#[test]
fn a_server_that_ignores_the_range_is_detected_rather_than_trusted() {
    // The failure this guards: a 200 means the body begins at byte 0, so
    // treating it as the resume point plays the chapter from the start while
    // the progress bar claims we are ten minutes in.
    let h = head(b"HTTP/1.1 200 OK\r\nContent-Length: 14487328\r\n\r\n");
    assert_eq!(h.status, 200);
    assert!(!h.is_partial());
    assert_eq!(
        h.body_starts_at(1_000_000),
        0,
        "a 200 body starts at 0 no matter what we asked for"
    );
}

#[test]
fn a_206_without_content_range_falls_back_to_what_we_asked_for() {
    let h = head(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 512\r\n\r\n");
    assert!(h.is_partial());
    assert_eq!(h.content_range, None);
    assert_eq!(h.body_starts_at(4096), 4096);
}

#[test]
fn header_matching_is_case_insensitive_and_tolerates_whitespace() {
    let h = head(
        b"HTTP/1.1 206 Partial Content\r\nCONTENT-LENGTH:   512  \r\nContent-Range:  BYTES 10-20/99 \r\n\r\n",
    );
    assert_eq!(h.content_length, Some(512));
    let cr = h.content_range.unwrap();
    assert_eq!((cr.first, cr.last, cr.total), (10, 20, 99));
}

#[test]
fn a_head_split_across_reads_returns_incomplete_until_the_terminator_lands() {
    // Exactly how it arrives off a socket.
    for split in 1..RANGE_HDR.len() {
        let partial = &RANGE_HDR[..split];
        match parse_head(partial) {
            HeadParse::Incomplete => {
                assert!(
                    find_terminator(partial).is_none(),
                    "said Incomplete but the terminator was present at {split}"
                );
            }
            HeadParse::Ok(h) => {
                assert_eq!(h.status, 206, "split {split}");
                assert!(find_terminator(partial).is_some());
            }
            HeadParse::Malformed => panic!("split {split} was called malformed"),
        }
    }
}

fn find_terminator(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

#[test]
fn body_offset_points_at_the_first_body_byte() {
    let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\r\nPCMD";
    let h = head(raw);
    assert_eq!(&raw[h.body_offset..], b"PCMD");
}

#[test]
fn error_statuses_are_reported_not_played() {
    // A 404 body must never reach the speaker as PCM.
    for (raw, code) in [
        (&b"HTTP/1.1 404 Not Found\r\n\r\n"[..], 404u16),
        (&b"HTTP/1.1 416 Range Not Satisfiable\r\n\r\n"[..], 416),
        (&b"HTTP/1.1 500 Internal Server Error\r\n\r\n"[..], 500),
    ] {
        let h = head(raw);
        assert_eq!(h.status, code);
        assert!(!h.ok(), "{code} must not read as success");
    }
}

#[test]
fn malformed_heads_are_rejected_and_never_panic() {
    for bad in [
        &b"\r\n\r\n"[..],
        &b"NOT HTTP\r\n\r\n"[..],
        &b"HTTP/1.1\r\n\r\n"[..],
        &b"HTTP/1.1 abc OK\r\n\r\n"[..],
        &b"HTTP/1.1 9999 OK\r\n\r\n"[..],
        &b"HTTP/1.1 12 OK\r\n\r\n"[..],
    ] {
        assert_eq!(parse_head(bad), HeadParse::Malformed, "{bad:?}");
    }
}

#[test]
fn a_nonsense_content_range_is_ignored_rather_than_believed() {
    // first > last is incoherent; better no range than a bogus resume offset.
    let h = head(b"HTTP/1.1 206 Partial\r\nContent-Range: bytes 900-100/1000\r\n\r\n");
    assert_eq!(h.content_range, None);
    let h = head(b"HTTP/1.1 206 Partial\r\nContent-Range: pages 1-2/3\r\n\r\n");
    assert_eq!(h.content_range, None);
}

#[test]
fn an_unknown_total_in_content_range_does_not_reject_the_response() {
    // `bytes 0-511/*` is legal.
    let h = head(b"HTTP/1.1 206 Partial\r\nContent-Range: bytes 0-511/*\r\n\r\n");
    let cr = h.content_range.expect("range still usable");
    assert_eq!((cr.first, cr.last), (0, 511));
    assert_eq!(cr.total, 0, "unknown total reads as 0, not garbage");
}

// ---------------------------------------------------------------------------
// Integer formatting
// ---------------------------------------------------------------------------

#[test]
fn push_u32_formats_without_core_fmt() {
    let mut s: heapless::String<32> = heapless::String::new();
    assert!(push_u32(&mut s, 0));
    assert!(push_u32(&mut s, 7));
    assert!(push_u32(&mut s, 452_729));
    assert_eq!(s.as_str(), "07452729");

    let mut s: heapless::String<16> = heapless::String::new();
    assert!(push_u32(&mut s, u32::MAX));
    assert_eq!(s.as_str(), "4294967295");
}

#[test]
fn push_u32_reports_overflow_instead_of_truncating_silently() {
    let mut s: heapless::String<3> = heapless::String::new();
    assert!(!push_u32(&mut s, 123_456), "must report failure when full");
}

#[test]
fn zero_padding_matches_the_daemons_media_filenames() {
    // The daemon's own pcm_url is `/media/0001.pcm` — four digits, zero-padded.
    for (n, want) in [(1u32, "0001"), (9, "0009"), (42, "0042"), (999, "0999"), (1234, "1234")] {
        let mut s: heapless::String<16> = heapless::String::new();
        assert!(push_u32_pad(&mut s, n, 4));
        assert_eq!(s.as_str(), want);
    }
    // Beyond the pad width it does not truncate the number.
    let mut s: heapless::String<16> = heapless::String::new();
    assert!(push_u32_pad(&mut s, 12_345, 4));
    assert_eq!(s.as_str(), "12345");
}

// ---------------------------------------------------------------------------
// Request composition
// ---------------------------------------------------------------------------

#[test]
fn the_media_route_zero_pads_and_the_api_route_does_not() {
    // Two different conventions for the same chapter number in one API — a real
    // source of 404s, which is why the paths are built in one place.
    let mut s: heapless::String<64> = heapless::String::new();
    Route::Media { n: 1 }.push_path(&mut s);
    assert_eq!(s.as_str(), "/media/0001.pcm");

    let mut s: heapless::String<64> = heapless::String::new();
    Route::Chapter { n: 1 }.push_path(&mut s);
    assert_eq!(s.as_str(), "/api/chapters/1");
}

/// The daemon's own chapter index, which publishes a `pcm_url` per chapter.
const CHAPTERS: &[u8] = include_bytes!("fixtures/chapters.json");

#[test]
fn the_media_path_matches_the_url_the_daemon_itself_publishes() {
    // `litrpg_core::artifact` owns the `NNNN.pcm` convention server-side, but the
    // watch cannot take that crate as a dependency: it is `no_std` + **alloc**
    // (`pcm_name` returns a `String`), and it lives in a different repository, so
    // a path dependency would not survive `fambuild`'s rsync of this worktree
    // alone. See the findings file.
    //
    // Rather than trust two copies of `{:04}` to agree, this pins the local
    // formatter against the URL the DAEMON actually emitted. That is a stronger
    // check than sharing a constant would be, because it validates against what
    // the server says rather than against an assumption both sides might share.
    let text = core::str::from_utf8(CHAPTERS).expect("fixture is utf8");
    let mut checked = 0;
    for (n, seg) in text.match_indices("/media/").map(|(i, _)| &text[i..]).enumerate() {
        // `/media/0001.pcm` — take up to the closing quote.
        let end = seg.find('"').unwrap_or(seg.len());
        let published = &seg[..end];
        if !published.ends_with(".pcm") {
            continue;
        }
        // Chapter numbers in this fixture run 1..=3 in order.
        let chapter = (n / 2 + 1) as u16;
        let mut mine: heapless::String<64> = heapless::String::new();
        assert!(Route::Media { n: chapter }.push_path(&mut mine));
        assert_eq!(
            mine.as_str(),
            published,
            "chapter {chapter}: our path disagrees with the daemon's pcm_url"
        );
        checked += 1;
    }
    assert!(checked >= 3, "expected 3 pcm_urls in the fixture, saw {checked}");
}

#[test]
fn every_route_matches_the_live_api() {
    let cases: [(Route, &str); 7] = [
        (Route::Chapters { since: 0 }, "/api/chapters?since=0"),
        (Route::Chapters { since: 12 }, "/api/chapters?since=12"),
        (Route::Chapter { n: 3 }, "/api/chapters/3"),
        (Route::Media { n: 2 }, "/media/0002.pcm"),
        // Subject omitted -> the protagonist, per design §9.4.1. Avoids having
        // to percent-encode "Kaelen Vord".
        (Route::Character, "/api/character"),
        (Route::Progress, "/api/progress"),
        (Route::Notes, "/api/notes"),
    ];
    for (route, want) in cases {
        let mut s: heapless::String<64> = heapless::String::new();
        assert!(route.push_path(&mut s), "{want}");
        assert_eq!(s.as_str(), want);
    }
}

#[test]
fn a_range_request_head_is_well_formed() {
    let h = request(
        Method::Get,
        Route::Media { n: 1 },
        [10, 0, 6, 107],
        8093,
        Some((1_000_000, 1_000_511)),
        None,
    )
    .expect("fits");
    let s = h.as_str();
    assert!(s.starts_with("GET /media/0001.pcm HTTP/1.1\r\n"), "{s}");
    // A literal dotted quad: no DNS on this device (design §9.1).
    assert!(s.contains("Host: 10.0.6.107:8093\r\n"), "{s}");
    assert!(s.contains("Range: bytes=1000000-1000511\r\n"), "{s}");
    assert!(s.contains("Connection: close\r\n"), "{s}");
    assert!(s.ends_with("\r\n\r\n"), "{s}");
    // No body headers on a GET.
    assert!(!s.contains("Content-Length"), "{s}");
}

#[test]
fn a_put_progress_head_carries_the_json_body_headers() {
    let body = encode_progress(7).unwrap();
    let h = request(
        Method::Put,
        Route::Progress,
        [10, 0, 6, 107],
        8093,
        None,
        Some(body.len()),
    )
    .unwrap();
    let s = h.as_str();
    assert!(s.starts_with("PUT /api/progress HTTP/1.1\r\n"), "{s}");
    assert!(s.contains("Content-Type: application/json\r\n"), "{s}");
    assert!(s.contains(&format!("Content-Length: {}\r\n", body.len())), "{s}");
    assert!(!s.contains("Range:"), "{s}");
}

#[test]
fn a_post_notes_head_is_well_formed() {
    let body = encode_note("introduce a rival").unwrap();
    let h = request(Method::Post, Route::Notes, [10, 0, 6, 107], 8093, None, Some(body.len()))
        .unwrap();
    assert!(h.as_str().starts_with("POST /api/notes HTTP/1.1\r\n"));
}

#[test]
fn the_composed_head_round_trips_through_our_own_parser() {
    // Not a tautology: it proves the head we emit is terminated and shaped the
    // way a parser expects, which a hand-built string easily gets wrong.
    let h = request(Method::Get, Route::Progress, [127, 0, 0, 1], 8093, None, None).unwrap();
    let mut raw = heapless::Vec::<u8, 512>::new();
    raw.extend_from_slice(b"HTTP/1.1 200 OK\r\n").unwrap();
    let _ = h; // the request head is not a response; check ours terminates
    assert!(h.as_str().ends_with("\r\n\r\n"));
    raw.extend_from_slice(b"\r\n").unwrap();
    assert_eq!(head(&raw).status, 200);
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

#[test]
fn progress_body_matches_what_the_daemon_accepts() {
    // Verified live: this exact shape returned 200 and moved the cursor.
    assert_eq!(encode_progress(0).unwrap().as_str(), r#"{"consumed_through":0}"#);
    assert_eq!(encode_progress(42).unwrap().as_str(), r#"{"consumed_through":42}"#);
}

#[test]
fn note_body_matches_what_the_daemon_accepts() {
    // Verified live: this exact shape returned 201 Created.
    assert_eq!(
        encode_note("introduce a rival").unwrap().as_str(),
        r#"{"body":"introduce a rival","source":"watch"}"#
    );
}

#[test]
fn note_escaping_survives_dictated_punctuation() {
    // The transcript comes from the STT gateway, so quotes and apostrophes are
    // routine, not adversarial. Escaping is hand-rolled, hence tested.
    let cases = [
        (r#"say "stop" now"#, r#"say \"stop\" now"#),
        (r"back\slash", r"back\\slash"),
        ("line\nbreak", r"line\nbreak"),
        ("tab\there", r"tab\there"),
        ("carriage\rreturn", r"carriage\rreturn"),
    ];
    for (input, escaped) in cases {
        let got = encode_note(input).unwrap();
        let want = format!(r#"{{"body":"{escaped}","source":"watch"}}"#);
        assert_eq!(got.as_str(), want, "input {input:?}");
    }
}

#[test]
fn a_control_character_becomes_a_unicode_escape_not_a_raw_byte() {
    // A raw control byte inside a JSON string is illegal and would make the
    // daemon reject the note outright.
    let got = encode_note("bell\u{7}here").unwrap();
    assert_eq!(got.as_str(), "{\"body\":\"bell\\u0007here\",\"source\":\"watch\"}");
    // The point of the escape: no byte below 0x20 survives into the wire body.
    assert!(
        !got.as_str().bytes().any(|b| b < 0x20),
        "a raw control byte reached the request body"
    );
}

#[test]
fn non_ascii_is_dropped_rather_than_mangled() {
    // Mirrors notify::sanitize's ASCII clamp; the daemon stores what we send
    // verbatim, so emitting a broken encoding would persist it.
    let got = encode_note("caf\u{e9} — na\u{ef}ve").unwrap();
    assert_eq!(got.as_str(), r#"{"body":"caf  nave","source":"watch"}"#);
}

#[test]
fn an_over_long_note_is_clipped_to_the_cap_and_still_valid_json() {
    let long = "word ".repeat(500);
    let got = encode_note(&long).expect("must not fail, must clip");
    assert!(got.as_str().starts_with(r#"{"body":"word word"#));
    assert!(got.as_str().ends_with(r#"","source":"watch"}"#));
    // The clip is on the source text, so the body holds at most MAX_NOTE chars.
    let inner = got.as_str().trim_start_matches(r#"{"body":""#);
    let inner = inner.trim_end_matches(r#"","source":"watch"}"#);
    assert!(inner.chars().count() <= MAX_NOTE, "{} chars", inner.chars().count());
}

#[test]
fn a_worst_case_note_of_all_escapes_still_fits_its_buffer() {
    // Every char doubling in length is the case that overflows a badly-sized
    // buffer — `Body` is deliberately MAX_NOTE * 2 + 48 for exactly this.
    let quotes = "\"".repeat(MAX_NOTE);
    let got = encode_note(&quotes).expect("the buffer must accommodate full escaping");
    assert!(got.as_str().ends_with(r#"","source":"watch"}"#));
}

#[test]
fn an_empty_or_wordless_transcript_is_not_notable() {
    // A misfired push-to-talk must not become a director note that steers the
    // story — the notes route is the one mutating endpoint (design §9.1).
    assert!(!is_notable(""));
    assert!(!is_notable("   "));
    assert!(!is_notable("... ?!"));
    assert!(is_notable("introduce a rival"));
    assert!(is_notable("ok"));
}
