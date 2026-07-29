//! The four models, against **real captured daemon responses**.
//!
//! The fixtures in `tests/fixtures/` are verbatim bodies and artifacts from a
//! live `litrpg-daemon` (2026-07-29), not hand-written approximations. Testing
//! against the real thing is the whole point: a fixture I wrote myself would
//! agree with whatever I assumed.
//!
//! # Two manifest shapes, and why both are kept
//!
//! The daemon's `sentence_manifest` landed mid-development, so the corpus exists
//! in two granularities and the fixtures capture both. The parser reads nothing
//! but `start_ms`/`end_ms`, so it is indifferent — and these fixtures are what
//! prove that rather than assert it.
//!
//! **Turn-level** (`chapter1.json`, `manifest0002/0003.json`) — what was served
//! until 2026-07-29, and what returns if `sentence_manifest` is ever disabled:
//!
//! | chapter | segments | longest segment | longest `text` |
//! |---|---|---|---|
//! | 1 | 7 | 203 s | 3,665 B |
//! | 2 | 58 | 66.9 s | 1,263 B |
//! | 3 | 7 | **570.8 s** | **7,506 B** |
//!
//! **Sentence-level** (`sentences0002/0003/0004.json`) — the served shape now,
//! measured after re-render:
//!
//! | chapter | segments | duration | longest segment | max `text` |
//! |---|---|---|---|---|
//! | 1 | 50 | 7.6 min | 14.1 s | 200 B |
//! | 2 | **92** | 9.9 min | 13.6 s | 199 B |
//! | 3 | 83 | **17.7 min** | **20.4 s** | 200 B |
//! | 4 | 65 | 14.2 min | 17.9 s | 200 B |
//! | 5 | 65 | 9.7 min | 14.0 s | 200 B |
//!
//! **Budget against chapter 3, never chapter 1.** Segment count tracks sentence
//! count rather than duration (chapter 2 is 9.9 min with 92 entries; chapter 3 is
//! 17.7 min with 83), so the worst case for the cap and the worst case for length
//! are different chapters. Sizing on chapter 1 alone has produced a wrong cap
//! twice: 32 was "4.5x" its seven turns and would have dropped 26 of chapter 2's.
//!
//! **Chapters 3 and 4's timings are volatile** (they re-rendered with the wrong
//! narrator, issue #15). Their *counts* are stable and safe to size against;
//! their durations and byte offsets are not, so nothing here treats a 3 or 4
//! timing as a fact about the story — only as bytes the parser must handle.
//!
//! Every payload is parsed three ways — whole, in 512-byte windows (the actual
//! socket read size), and one byte at a time — because the socket decides where
//! the boundaries fall, not us.

use story_proto::model::*;
use story_proto::*;

const CHAPTERS: &[u8] = include_bytes!("fixtures/chapters.json");
const CHAPTER1: &[u8] = include_bytes!("fixtures/chapter1.json");
const CHARACTER: &[u8] = include_bytes!("fixtures/character.json");
const PROGRESS: &[u8] = include_bytes!("fixtures/progress.json");

/// Parse `src` through `S`, in pieces of `chunk` bytes.
fn parse<S: EventSink + Default>(src: &[u8], chunk: usize) -> (S, bool, bool) {
    let mut r = Reader::new(S::default());
    for piece in src.chunks(chunk.max(1)) {
        r.feed(piece);
    }
    let (err, complete) = (r.error(), r.complete());
    (r.into_sink(), err, complete)
}

/// The read sizes that matter: whole, the real socket window, and the
/// pathological byte-at-a-time case.
const CHUNKS: [usize; 4] = [usize::MAX, 512, 64, 1];

// ---------------------------------------------------------------------------
// GET /api/chapters
// ---------------------------------------------------------------------------

#[test]
fn chapter_index_parses_at_every_read_size() {
    for chunk in CHUNKS {
        let (list, err, complete): (ChapterList, _, _) = parse(CHAPTERS, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");
        assert_eq!(list.rows.len(), 3, "chunk {chunk}");
        assert_eq!(list.dropped, 0);

        let r0 = &list.rows[0];
        assert_eq!(r0.number, 1);
        assert_eq!(r0.title.as_str(), "Collecting the Divine Shard");
        assert_eq!(r0.duration_ms, 452_729);
        assert!(r0.has_audio);
        assert_eq!(r0.total_bytes, Some(14_487_328));

        // An apostrophe in a title, which the daemon does NOT escape.
        assert_eq!(list.rows[2].title.as_str(), "Collecting Oren's Debt");
    }
}

#[test]
fn the_live_identity_holds_for_every_row_the_watch_parsed() {
    // The claim the whole Range design rests on, checked against what the
    // parser actually produced rather than against the numbers I read by eye.
    let (list, _, _): (ChapterList, _, _) = parse(CHAPTERS, 512);
    for r in list.rows.iter() {
        if let Some(total) = r.total_bytes {
            assert_eq!(
                ms_to_bytes(r.duration_ms),
                total,
                "chapter {} breaks duration_ms x 32 == total_bytes",
                r.number
            );
        }
    }
}

#[test]
fn a_chapter_without_audio_is_not_playable() {
    // Synthetic because all three live chapters have audio now — the daemon
    // rendered chapter 3 while this was being written.
    let src = br#"[{"number":9,"title":"Pending","duration_ms":0,"has_audio":false,"total_bytes":null}]"#;
    let (list, err, _): (ChapterList, _, _) = parse(src, 7);
    assert!(!err);
    let r = &list.rows[0];
    assert_eq!(r.total_bytes, None);
    assert!(!r.has_audio);
    assert!(!r.playable());
    // And no Range request can be formed for it.
    assert!(window_at(0, r.total_bytes.unwrap_or(0)).is_none());
}

#[test]
fn the_row_cap_drops_oldest_and_reports_it_rather_than_lying() {
    let mut src = Vec::from(*b"[");
    for n in 1..=(MAX_CHAPTERS + 5) {
        if n > 1 {
            src.push(b',');
        }
        src.extend_from_slice(
            format!(r#"{{"number":{n},"title":"C{n}","duration_ms":1000,"has_audio":true,"total_bytes":32000}}"#)
                .as_bytes(),
        );
    }
    src.push(b']');

    let (list, err, complete): (ChapterList, _, _) = parse(&src, 512);
    assert!(!err);
    assert!(complete);
    assert_eq!(list.rows.len(), MAX_CHAPTERS);
    assert_eq!(list.dropped, 5, "the cap must be reported, never silent");
    // The newest chapters are the ones kept.
    assert_eq!(list.rows.last().unwrap().number, (MAX_CHAPTERS + 5) as u16);
    assert_eq!(list.rows[0].number, 6);
}

// ---------------------------------------------------------------------------
// GET /api/chapters/{n} — the segment index
// ---------------------------------------------------------------------------

#[test]
fn segment_index_parses_the_real_18kb_chapter_at_every_read_size() {
    for chunk in CHUNKS {
        let (idx, err, complete): (SegmentIndex, _, _) = parse(CHAPTER1, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");

        assert_eq!(idx.chapter, 1);
        assert_eq!(idx.title.as_str(), "Collecting the Divine Shard");
        assert_eq!(idx.duration_ms, 452_729);
        assert_eq!(idx.bytes_per_ms, 32, "chunk {chunk}");
        assert_eq!(idx.sample_rate, 16_000);
        assert_eq!(idx.segments.len(), 7, "chunk {chunk}");
        assert_eq!(idx.dropped, 0);
    }
}

#[test]
fn the_real_segments_are_speaker_turns_not_sentences() {
    // This test documents the finding that changed the UI design: a "segment"
    // is a speaker turn up to 3m23s long, so it cannot drive sentence-level
    // highlighting. If the daemon ever splits per sentence, this test fails and
    // that is the signal to revisit the playback screen.
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);

    let longest = idx.segments.iter().map(|s| s.duration_ms()).max().unwrap();
    assert_eq!(longest, 203_027, "longest live segment is 3m23s");
    assert!(
        longest > 60_000,
        "segments are speaker turns; a sentence-level manifest would be far shorter"
    );
    assert_eq!(idx.segments.len(), 7, "7 segments across 7m33s of audio");
}

#[test]
fn segment_boundaries_match_the_live_manifest_exactly() {
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);
    let want: [(u16, u32, u32, SegKind, &str); 7] = [
        (0, 0, 112_120, SegKind::Narrator, "narrator"),
        (1, 112_120, 128_524, SegKind::System, "SYSTEM"),
        (2, 128_524, 331_551, SegKind::Narrator, "narrator"),
        (3, 331_551, 339_622, SegKind::System, "SYSTEM"),
        (4, 339_622, 418_359, SegKind::Narrator, "narrator"),
        (5, 418_359, 438_475, SegKind::System, "SYSTEM"),
        (6, 438_475, 452_729, SegKind::Narrator, "narrator"),
    ];
    for (i, (wi, ws, we, wk, wspk)) in want.iter().enumerate() {
        let s = &idx.segments[i];
        assert_eq!(s.idx, *wi);
        assert_eq!(s.start_ms, *ws);
        assert_eq!(s.end_ms, *we);
        assert_eq!(s.kind, *wk);
        assert_eq!(idx.speaker_of(s), *wspk);
    }
}

#[test]
fn the_index_is_contiguous_and_totals_the_chapter() {
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);
    assert!(idx.contiguous(), "segments must tile with no gap or overlap");
    assert_eq!(idx.segments.last().unwrap().end_ms, idx.duration_ms);
    assert_eq!(idx.total_bytes(), 14_487_328);
    // Byte offsets are derived, exact, and sample-aligned.
    for s in idx.segments.iter() {
        assert_eq!(s.start_byte(), s.start_ms * 32);
        assert_eq!(align_sample(s.start_byte()), s.start_byte());
    }
}

#[test]
fn prose_is_never_retained_anywhere() {
    // The reason this is a streaming parser at all. The payload contains an
    // 8,312-byte `text_md` and a 3,665-byte segment `text`; nothing that long
    // may survive into the model.
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);
    assert!(idx.title.as_str().len() < 64);
    for name in idx.speakers.iter() {
        assert!(name.as_str().len() <= MAX_SPEAKER, "speaker grew past its bound");
    }
    // And the whole retained model is orders of magnitude smaller than the
    // document it was parsed from.
    assert!(
        core::mem::size_of::<SegmentIndex>() < CHAPTER1.len() / 4,
        "SegmentIndex is {} B against an 18 KB payload",
        core::mem::size_of::<SegmentIndex>()
    );
}

#[test]
fn segment_lookup_finds_the_right_speaker_for_a_playback_position() {
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);

    // Boundaries are half-open: a segment owns its start, not its end.
    assert_eq!(idx.segment_idx_at(0), Some(0));
    assert_eq!(idx.segment_idx_at(112_119), Some(0));
    assert_eq!(idx.segment_idx_at(112_120), Some(1));
    assert_eq!(idx.segment_idx_at(128_523), Some(1));
    assert_eq!(idx.segment_idx_at(128_524), Some(2));
    assert_eq!(idx.segment_idx_at(438_475), Some(6));

    // The speaker chip the playback screen shows, mid-chapter.
    assert_eq!(idx.segment_at(200_000).unwrap().kind, SegKind::Narrator);
    assert_eq!(idx.segment_at(120_000).unwrap().kind, SegKind::System);

    // Past the end clamps to the last segment rather than blanking the screen.
    assert_eq!(idx.segment_idx_at(452_729), Some(6));
    assert_eq!(idx.segment_idx_at(999_999_999), Some(6));
}

#[test]
fn segment_lookup_agrees_with_a_linear_scan_across_the_whole_chapter() {
    // Binary search is easy to get subtly wrong at boundaries, so check it
    // against the obvious implementation at every second of the chapter.
    let (idx, _, _): (SegmentIndex, _, _) = parse(CHAPTER1, 512);
    for ms in (0..idx.duration_ms).step_by(1000) {
        let want = idx
            .segments
            .iter()
            .position(|s| ms >= s.start_ms && ms < s.end_ms);
        assert_eq!(idx.segment_idx_at(ms), want, "disagreement at {ms} ms");
    }
}

#[test]
fn a_position_before_the_first_segment_has_no_answer() {
    let mut src = Vec::from(*br#"{"manifest":{"segments":[{"idx":0,"start_ms":500,"end_ms":900,"kind":"narrator","speaker":"n"}]}}"#);
    src.shrink_to_fit();
    let (idx, err, _): (SegmentIndex, _, _) = parse(&src, 13);
    assert!(!err);
    assert_eq!(idx.segment_idx_at(0), None);
    assert_eq!(idx.segment_idx_at(500), Some(0));
}

#[test]
fn an_unknown_segment_kind_degrades_instead_of_vanishing() {
    let src = br#"{"manifest":{"segments":[{"idx":0,"start_ms":0,"end_ms":10,"kind":"chorus","speaker":"Sera"}]}}"#;
    let (idx, err, _): (SegmentIndex, _, _) = parse(src, 512);
    assert!(!err);
    assert_eq!(idx.segments.len(), 1, "a new kind must not drop the segment");
    assert_eq!(idx.segments[0].kind, SegKind::Other);
    assert_eq!(idx.speaker_of(&idx.segments[0]), "Sera");
}

// ---------------------------------------------------------------------------
// The other two live chapters — they disagree with chapter 1 violently
// ---------------------------------------------------------------------------
//
// `manifest000N.json` are the daemon's own manifest artifacts from `media/`,
// which is byte-for-byte what it embeds under `"manifest"` in the API response.
// The segment objects are identical; only the envelope differs, and the envelope
// is already covered by `chapter1.json`. Kept as separate fixtures rather than
// synthesised into an envelope, because a fixture I assembled myself would agree
// with whatever I assumed.

const MANIFEST2: &[u8] = include_bytes!("fixtures/manifest0002.json");
const MANIFEST3: &[u8] = include_bytes!("fixtures/manifest0003.json");

#[test]
fn the_dialogue_heavy_chapter_has_fifty_eight_segments_and_all_are_kept() {
    // Chapter 2 is why MAX_SEGMENTS is 96 and not 32. An earlier revision of that
    // constant was sized on chapter 1's SEVEN segments; against this chapter it
    // would have dropped 26 and blanked the speaker chip two-thirds of the way
    // through the most dialogue-rich chapter in the story — where it is most
    // wanted, and where nobody would have thought to look.
    for chunk in CHUNKS {
        let (idx, err, complete): (SegmentIndex, _, _) = parse(MANIFEST2, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");
        assert_eq!(idx.segments.len(), 58, "chunk {chunk}");
        assert_eq!(idx.dropped, 0, "no segment may be silently dropped");
        assert!(idx.contiguous());
        assert_eq!(idx.duration_ms, 594_544);
        assert_eq!(idx.total_bytes(), 19_025_408);
    }
}

#[test]
fn speaker_interning_holds_the_whole_cast_of_the_dialogue_chapter() {
    let (idx, _, _): (SegmentIndex, _, _) = parse(MANIFEST2, 512);

    // Five distinct speakers across 58 segments — the ratio that makes interning
    // worth it.
    assert_eq!(idx.speakers.len(), 5, "got {:?}", idx.speakers);
    assert_eq!(idx.speakers_dropped, 0);
    let names: Vec<&str> = idx.speakers.iter().map(|s| s.as_str()).collect();
    for want in ["narrator", "Kaelen", "Sera", "SYSTEM", "Shadow"] {
        assert!(names.contains(&want), "missing {want} from {names:?}");
    }

    // Every segment resolves to a real name.
    for s in idx.segments.iter() {
        assert!(!idx.speaker_of(s).is_empty(), "segment {} has no speaker", s.idx);
    }

    // And the live `kind` distribution: 17 narrator, 37 character, 4 system.
    let n = |k: SegKind| idx.segments.iter().filter(|s| s.kind == k).count();
    assert_eq!(n(SegKind::Narrator), 17);
    assert_eq!(n(SegKind::Dialogue), 37, "`character` must map to Dialogue");
    assert_eq!(n(SegKind::System), 4);
}

#[test]
fn the_speaker_chip_resolves_a_name_at_any_position_in_the_dialogue_chapter() {
    let (idx, _, _): (SegmentIndex, _, _) = parse(MANIFEST2, 512);
    // Every second of the chapter names somebody — this is the playback screen's
    // one job, checked exhaustively rather than at a couple of spot positions.
    for ms in (0..idx.duration_ms).step_by(1000) {
        assert!(!idx.speaker_at(ms).is_empty(), "no speaker at {ms} ms");
    }
}

#[test]
fn a_single_segment_can_be_nine_and_a_half_minutes_long() {
    // Chapter 3: 18 minutes of narration in SEVEN segments, the longest being
    // 570.8 s. This is the case that kills sentence-level highlighting from this
    // manifest — a highlight would sit motionless for 9.5 minutes — and it is
    // also why the playback screen shows a speaker chip and progress rather than
    // pretending to track prose.
    let (idx, err, complete): (SegmentIndex, _, _) = parse(MANIFEST3, 512);
    assert!(!err);
    assert!(complete);
    assert_eq!(idx.segments.len(), 7);
    let longest = idx.segments.iter().map(|s| s.duration_ms()).max().unwrap();
    assert_eq!(longest, 570_800);
    assert!(longest > 9 * 60 * 1000, "9.5 minutes in one segment");
    assert_eq!(idx.duration_ms, 1_085_050);
    assert_eq!(idx.total_bytes(), 34_721_600);
}

#[test]
fn the_identity_holds_on_all_three_live_chapters() {
    for (src, ms, bytes) in [
        (CHAPTER1, 452_729u32, 14_487_328u32),
        (MANIFEST2, 594_544, 19_025_408),
        (MANIFEST3, 1_085_050, 34_721_600),
    ] {
        let (idx, _, _): (SegmentIndex, _, _) = parse(src, 512);
        assert_eq!(idx.duration_ms, ms);
        assert_eq!(ms_to_bytes(ms), bytes);
        assert_eq!(idx.total_bytes(), bytes);
        // Every segment's derived offset is exact and sample-aligned.
        for s in idx.segments.iter() {
            assert_eq!(s.start_byte(), s.start_ms * 32);
            assert_eq!(align_sample(s.start_byte()), s.start_byte());
        }
    }
}

// ---------------------------------------------------------------------------
// The per-sentence manifest shape, as ACTUALLY SERVED
// ---------------------------------------------------------------------------
//
// `sentence_manifest` defaults on, so the daemon re-rendered its corpus at
// sentence granularity mid-task. These are the real served artifacts, captured
// after that re-render. They matter because the numbers first relayed to me were
// from a tool that *simulated* the split without re-rendering — so this is the
// difference between what a re-render would produce and what the server sends.
//
// Measured across the live corpus after re-render: 50 / 92 / 7 / 65 / 65
// segments. **92 is the worst**, and chapter 3 is still turn-level (7 segments,
// one of them 570.8 s), so both shapes coexist in one corpus and the parser has
// to handle them side by side.

const SENTENCES2: &[u8] = include_bytes!("fixtures/sentences0002.json");
/// Chapter 3: the **longest** chapter (17.7 min, 83 entries, 20.4 s longest
/// span). The one to budget against — chapter 1 is not the worst case for
/// anything.
const SENTENCES3: &[u8] = include_bytes!("fixtures/sentences0003.json");
const SENTENCES4: &[u8] = include_bytes!("fixtures/sentences0004.json");

#[test]
fn the_worst_real_per_sentence_chapter_fits_the_cap_with_room() {
    // 92 is the largest the daemon has actually served. This is the test that
    // justifies MAX_SEGMENTS: if a future chapter exceeds it, this stays green
    // while `a_manifest_over_the_cap_refuses_rather_than_truncating` covers the
    // behaviour — but the assertion below is what says the cap was sized against
    // reality rather than against a projection.
    for chunk in CHUNKS {
        let (idx, err, complete): (SegmentIndex, _, _) = parse(SENTENCES2, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");
        assert_eq!(idx.segments.len(), 92, "chunk {chunk}");
        assert_eq!(idx.dropped, 0, "the worst real chapter must not overflow");
        assert!(idx.usable(), "and must be trusted to drive highlighting");
    }
}

/// The worst chapter the daemon has actually served, in segments (chapter 2).
///
/// A **compile-time** assertion rather than a runtime one: if someone trims
/// `MAX_SEGMENTS` below what production serves, that should fail to build, not
/// fail a test run somebody might not have running. Same idiom the firmware uses
/// to tie `story_proto::PLAY_CHUNK` to `audio_out::PLAY_CHUNK`.
const WORST_SERVED_SEGMENTS: usize = 92;
const _: () = assert!(MAX_SEGMENTS > WORST_SERVED_SEGMENTS);

#[test]
fn a_full_length_chapter_is_what_the_budgets_must_hold_for() {
    // Chapter 3: 83 entries. Every budget in this app — the segment cap, the paint
    // gate, the Range windowing — has to hold at a full-length chapter, not at
    // chapter 1's 7.6 minutes.
    //
    // **Nothing here hardcodes a timing.** Chapters 3 and 4 are re-rendered
    // whenever the cast changes (issue #15 moved chapter 3 from 1,064,350 ms to
    // 804,975 ms mid-task), so `start_ms` and every byte offset derived from it
    // are volatile. An earlier version of this test asserted `windows == 18` from
    // the old duration and broke on the re-render — after I had been told
    // explicitly that counts are stable and timings are not. Segment COUNT is
    // asserted; everything time-derived is computed from the manifest itself.
    let (idx, err, complete): (SegmentIndex, _, _) = parse(SENTENCES3, 512);
    assert!(!err);
    assert!(complete);
    assert_eq!(idx.segments.len(), 83);
    assert!(idx.usable());
    assert_eq!(idx.dropped, 0);

    // A chapter's PCM is larger than the watch's ENTIRE 16 MB flash (which is
    // split into two 6 MB OTA slots anyway). Caching a chapter is not merely
    // unwise, it is arithmetically impossible — so Range-streaming is the only
    // design, not a preference. Asserted rather than remarked, because if a
    // chapter ever fit in flash that claim would deserve re-examining.
    let total = idx.total_bytes();
    assert!(
        total > 16 * 1024 * 1024,
        "chapter is {total} B; the streaming-is-mandatory claim needs re-checking"
    );

    // Which is why it is streamed in windows: 60 s each, tiling exactly.
    let mut pos = 0u32;
    let mut windows = 0u32;
    let mut covered = 0u64;
    while let Some((first, last)) = window_at(pos, total) {
        assert_eq!(first, pos);
        covered += (last - first + 1) as u64;
        pos = last + 1;
        windows += 1;
        assert!(windows < 100, "runaway");
    }
    // The substance: every byte covered exactly once, no gap, no overlap. The
    // count is DERIVED so a re-render cannot break it.
    assert_eq!(covered, total as u64, "windows must tile the whole chapter");
    let expected = total.div_ceil(WINDOW_BYTES);
    assert_eq!(windows, expected, "one window per 60 s of audio, remainder included");
    assert!(windows > 1, "a full-length chapter must need several windows");
}

#[test]
fn per_sentence_spans_are_short_enough_to_highlight() {
    // The point of the per-sentence manifest: turn-level chapter 3 has a single
    // 570.8 s segment, which cannot drive a highlight. Sentence-level spans are
    // two orders of magnitude shorter.
    for (name, src, want) in
        [("ch2", SENTENCES2, 92usize), ("ch3", SENTENCES3, 83), ("ch4", SENTENCES4, 65)]
    {
        let (idx, _, _): (SegmentIndex, _, _) = parse(src, 512);
        assert_eq!(idx.segments.len(), want, "{name}");
        let longest = idx.segments.iter().map(|s| s.duration_ms()).max().unwrap();
        // 20.4 s is the real maximum (chapter 3); the point is seconds, not the
        // 570 s a turn-level segment could reach.
        assert!(
            longest < 25_000,
            "{name}: longest sentence span {longest} ms should be seconds, not minutes"
        );
        assert!(idx.contiguous(), "{name}");
    }
}

#[test]
fn the_identity_still_holds_on_the_re_rendered_corpus() {
    // `loudnorm` alters stream length, so a re-render is exactly when
    // `duration_ms x 32 == total_bytes` could quietly stop holding.
    //
    // **Nothing here asserts a duration.** These fixtures get refreshed whenever
    // the cast changes, and hardcoding a timing against a refreshed fixture has
    // now broken this suite twice — chapter 3 moved 1,064,350 -> 804,975 ms and
    // chapter 4 moved 856,300 -> 637,151 ms, both after I was told plainly that
    // counts are stable and timings are not. So the invariants are checked
    // structurally, against the payload itself:
    //
    //   * the segment array and the top-level duration agree
    //   * segments tile with no gap or overlap
    //   * every byte offset is exact and sample-aligned
    //   * the manifest's own `bytes_per_ms` is one this hardware can play
    //
    // All four are stronger than a literal, and none can be invalidated by a
    // re-render. Segment COUNT stays asserted elsewhere — that is the stable fact.
    for (name, src) in
        [("ch2", SENTENCES2), ("ch3", SENTENCES3), ("ch4", SENTENCES4)]
    {
        let (idx, err, complete): (SegmentIndex, _, _) = parse(src, 512);
        assert!(!err, "{name}");
        assert!(complete, "{name}");

        assert_eq!(idx.bytes_per_ms, 32, "{name}");
        assert!(idx.rate_matches(), "{name}");
        assert!(idx.contiguous(), "{name}: segments must tile");
        assert!(idx.usable(), "{name}");

        // The segments array and the chapter duration must agree — a real
        // cross-check between two independently-serialised parts of the payload.
        assert_eq!(
            idx.segments.last().unwrap().end_ms,
            idx.duration_ms,
            "{name}: last segment must end where the chapter does"
        );

        // The 32-B/ms contract, end to end.
        assert_eq!(idx.total_bytes(), idx.duration_ms * 32, "{name}");
        for s in idx.segments.iter() {
            assert_eq!(s.start_byte(), s.start_ms * 32, "{name}");
            assert_eq!(s.end_byte(), s.end_ms * 32, "{name}");
            assert_eq!(align_sample(s.start_byte()), s.start_byte(), "{name}");
            assert!(s.end_ms >= s.start_ms, "{name}: segment runs backwards");
        }
    }
}

#[test]
fn the_speaker_table_holds_the_cast_of_the_worst_chapter() {
    // Five distinct speakers is the observed maximum across the whole corpus, so
    // MAX_SPEAKERS has ample room — including for the #14 aliasing case where one
    // character could appear under two spellings and consume two slots.
    let (idx, _, _): (SegmentIndex, _, _) = parse(SENTENCES2, 512);
    assert_eq!(idx.speakers.len(), 5, "got {:?}", idx.speakers);
    assert_eq!(idx.speakers_dropped, 0);
    assert!(
        idx.speakers.len() * 2 <= MAX_SPEAKERS,
        "the table must survive every name appearing under two spellings (#14)"
    );
    for ms in (0..idx.duration_ms).step_by(1000) {
        assert!(!idx.speaker_at(ms).is_empty(), "no speaker at {ms} ms");
    }
}

#[test]
fn both_manifest_shapes_coexist_in_one_corpus() {
    // Chapter 3 is still turn-level while 2 and 4 are sentence-level, so the
    // parser must not assume a granularity. It does not — it only reads
    // start_ms/end_ms — and this pins that.
    let (turns, _, _): (SegmentIndex, _, _) = parse(MANIFEST3, 512);
    let (sentences, _, _): (SegmentIndex, _, _) = parse(SENTENCES2, 512);
    assert_eq!(turns.segments.len(), 7);
    assert_eq!(sentences.segments.len(), 92);
    assert!(turns.usable() && sentences.usable(), "both shapes are usable");
    // And the difference is stark enough to justify the UI decision.
    let t = turns.segments.iter().map(|s| s.duration_ms()).max().unwrap();
    let s = sentences.segments.iter().map(|s| s.duration_ms()).max().unwrap();
    assert!(t > 40 * s, "turn spans dwarf sentence spans ({t} ms vs {s} ms)");
}

#[test]
fn all_three_live_manifests_are_usable() {
    for (name, src) in [("ch1", CHAPTER1), ("ch2", MANIFEST2), ("ch3", MANIFEST3)] {
        let (idx, _, _): (SegmentIndex, _, _) = parse(src, 512);
        assert!(idx.usable(), "{name} should drive highlighting");
        assert!(idx.rate_matches(), "{name}");
    }
}

#[test]
fn a_manifest_over_the_cap_refuses_rather_than_truncating() {
    // The failure being prevented: a truncated manifest is not a smaller
    // manifest, it is a highlight that silently desynchronises partway through
    // the chapter. So overflow must make the index unusable, not shorter.
    let mut src = Vec::from(*br#"{"duration_ms":999999,"manifest":{"segments":["#);
    let n = MAX_SEGMENTS + 20;
    for i in 0..n {
        if i > 0 {
            src.push(b',');
        }
        src.extend_from_slice(
            format!(
                r#"{{"idx":{i},"start_ms":{},"end_ms":{},"kind":"narrator","speaker":"narrator"}}"#,
                i * 100,
                (i + 1) * 100
            )
            .as_bytes(),
        );
    }
    src.extend_from_slice(b"]}}");

    let (idx, err, complete): (SegmentIndex, _, _) = parse(&src, 512);
    assert!(!err, "an oversized manifest is not malformed JSON");
    assert!(complete);
    assert_eq!(idx.segments.len(), MAX_SEGMENTS);
    assert_eq!(idx.dropped, 20, "overflow must be counted, never silent");
    assert!(
        !idx.usable(),
        "a truncated index must refuse to drive highlighting"
    );
    // But the chapter is still playable: playback reads total_bytes from the
    // chapter row, never the manifest.
    assert!(window_at(0, 1_000_000).is_some());
}

#[test]
fn a_manifest_at_a_rate_this_hardware_cannot_play_is_declined() {
    // The watch cannot honour a different bytes_per_ms — audio_out is 16 kHz
    // mono s16le with no resampler — so it must DETECT and decline rather than
    // silently compute every offset against 32.
    let src = br#"{"manifest":{"bytes_per_ms":48,"sample_rate":24000,"segments":[
        {"idx":0,"start_ms":0,"end_ms":1000,"kind":"narrator","speaker":"narrator"}]}}"#;
    let (idx, err, _): (SegmentIndex, _, _) = parse(src, 512);
    assert!(!err);
    assert_eq!(idx.bytes_per_ms, 48);
    assert!(!idx.rate_matches(), "48 B/ms is not playable here");
    assert!(!idx.usable(), "and so the index must not drive highlighting");
}

#[test]
fn a_manifest_with_a_gap_is_declined() {
    // The daemon asserts is_contiguous() before publishing; if that ever breaks,
    // offsets have drifted and the right answer is no highlight rather than a
    // wrong one.
    let src = br#"{"manifest":{"segments":[
        {"idx":0,"start_ms":0,"end_ms":1000,"kind":"narrator","speaker":"n"},
        {"idx":1,"start_ms":5000,"end_ms":6000,"kind":"narrator","speaker":"n"}]}}"#;
    let (idx, err, _): (SegmentIndex, _, _) = parse(src, 512);
    assert!(!err);
    assert_eq!(idx.segments.len(), 2);
    assert!(!idx.contiguous());
    assert!(!idx.usable());
}

#[test]
fn an_ensemble_beyond_the_speaker_table_degrades_visibly() {
    // More distinct speakers than MAX_SPEAKERS: the extras must resolve to an
    // empty chip AND be counted, never silently aliased onto another character —
    // showing the wrong speaker's name would be worse than showing none.
    let mut src = Vec::from(*br#"{"segments":["#);
    for i in 0..(MAX_SPEAKERS + 4) {
        if i > 0 {
            src.push(b',');
        }
        src.extend_from_slice(
            format!(
                r#"{{"idx":{i},"start_ms":{},"end_ms":{},"kind":"character","speaker":"P{i}"}}"#,
                i * 1000,
                (i + 1) * 1000
            )
            .as_bytes(),
        );
    }
    src.extend_from_slice(b"]}");

    let (idx, err, _): (SegmentIndex, _, _) = parse(&src, 64);
    assert!(!err);
    assert_eq!(idx.segments.len(), MAX_SPEAKERS + 4, "segments are still kept");
    assert_eq!(idx.speakers.len(), MAX_SPEAKERS);
    assert_eq!(idx.speakers_dropped, 4, "overflow must be reported");
    // The first MAX_SPEAKERS resolve; the overflow resolves to "" not to P0.
    assert_eq!(idx.speaker_of(&idx.segments[0]), "P0");
    let last = idx.segments.last().unwrap();
    assert_eq!(idx.speaker_of(last), "", "must not alias onto another speaker");
}

// ---------------------------------------------------------------------------
// GET /api/character
// ---------------------------------------------------------------------------

#[test]
fn character_parses_at_every_read_size() {
    for chunk in CHUNKS {
        let (c, err, complete): (Character, _, _) = parse(CHARACTER, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");

        assert_eq!(c.subject.as_str(), "Kaelen Vord");
        assert!(c.known);
        assert_eq!(c.level, Some(3));
        assert_eq!(c.xp, Some(150));
        assert_eq!(c.max_hp, Some(110));
        assert_eq!(c.status.as_ref().unwrap().as_str(), "Authority (Passive) unlocked");
    }
}

#[test]
fn nulls_stay_none_rather_than_becoming_zero() {
    // On the live ledger `hp`, `gold` and `location` are all null. A zero here
    // would render an empty HP bar and assert the protagonist is dead.
    let (c, _, _): (Character, _, _) = parse(CHARACTER, 512);
    assert_eq!(c.hp, None);
    assert_eq!(c.gold, None);
    assert!(c.location.is_none());
    assert_eq!(
        c.hp_fraction(),
        None,
        "an unknown hp must yield no bar, not a zero-width one"
    );
}

#[test]
fn hp_fraction_only_exists_when_both_ends_are_known() {
    let src = br#"{"subject":"K","known":true,"hp":55,"max_hp":110}"#;
    let (c, _, _): (Character, _, _) = parse(src, 3);
    assert_eq!(c.hp_fraction(), Some(0.5));

    // Degenerate max_hp must not divide by zero.
    let src = br#"{"hp":5,"max_hp":0}"#;
    let (c, _, _): (Character, _, _) = parse(src, 512);
    assert_eq!(c.hp_fraction(), None);

    // Over-full hp clamps rather than overflowing the bar.
    let src = br#"{"hp":900,"max_hp":100}"#;
    let (c, _, _): (Character, _, _) = parse(src, 512);
    assert_eq!(c.hp_fraction(), Some(1.0));
}

#[test]
fn inventory_comes_through_with_names_and_counts() {
    let (c, _, _): (Character, _, _) = parse(CHARACTER, 512);
    assert_eq!(c.inventory.len(), 3);
    assert_eq!(c.items_dropped, 0);
    let names: Vec<&str> = c.inventory.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"Coin of the Forgotten"));
    assert!(names.contains(&"Shard of Divine Foundation"));
    assert!(names.contains(&"Tattered Contract Page"));
    assert!(c.inventory.iter().all(|i| i.count == 1));
}

#[test]
fn every_equipment_slot_and_appearance_trait_is_empty_on_the_live_ledger() {
    // Not a bug in the screen — the ledger has accumulated no `equip:`/`appear:`
    // deltas yet. Recorded so that "the character screen is all dashes" is a
    // known state rather than a mystery when JP first opens it.
    let (c, _, _): (Character, _, _) = parse(CHARACTER, 512);
    assert_eq!(c.equipped_count(), 0);
    assert_eq!(c.appearance_count(), 0);
    for (i, slot) in EQUIP_SLOTS.iter().enumerate() {
        assert_eq!(c.equip_at(i), None, "slot {slot}");
    }
    for (i, trait_) in APPEAR_TRAITS.iter().enumerate() {
        assert_eq!(c.appear_at(i), None, "trait {trait_}");
    }
}

#[test]
fn equipment_and_appearance_land_in_their_whitelisted_slots() {
    let src = br#"{"subject":"K","equipment":{"main_hand":"Ledger of Oaths","head":null,
        "ring2":"Band of Debts","third_arm":"nonsense"},
        "appearance":{"hair":"black, cropped","eyes":"grey","notable":"ash-scarred hands"}}"#;
    let (c, err, _): (Character, _, _) = parse(src, 11);
    assert!(!err);

    let main = EQUIP_SLOTS.iter().position(|s| *s == "main_hand").unwrap();
    let ring2 = EQUIP_SLOTS.iter().position(|s| *s == "ring2").unwrap();
    let head = EQUIP_SLOTS.iter().position(|s| *s == "head").unwrap();
    assert_eq!(c.equip_at(main), Some("Ledger of Oaths"));
    assert_eq!(c.equip_at(ring2), Some("Band of Debts"));
    assert_eq!(c.equip_at(head), None);
    assert_eq!(c.equipped_count(), 2);

    let hair = APPEAR_TRAITS.iter().position(|s| *s == "hair").unwrap();
    assert_eq!(c.appear_at(hair), Some("black, cropped"));
    assert_eq!(c.appearance_count(), 3);

    // An invented slot is ignored, not rendered and not crashed on — the
    // whitelist is what keeps the renderer free of defensive layout (§9.4.1).
    assert_eq!(c.equipped_count(), 2, "third_arm must not create a row");
}

#[test]
fn the_slot_and_trait_whitelists_match_the_spec_counts() {
    assert_eq!(EQUIP_SLOTS.len(), 11, "eleven equip:* slots");
    assert_eq!(APPEAR_TRAITS.len(), 6, "six whitelisted appear:* traits");
    assert_eq!(EQUIP_LABELS.len(), EQUIP_SLOTS.len());
    assert_eq!(APPEAR_LABELS.len(), APPEAR_TRAITS.len());
    // And they match the live payload's key set exactly.
    let live = [
        "amulet", "chest", "cloak", "feet", "hands", "head", "legs", "main_hand", "off_hand",
        "ring1", "ring2",
    ];
    for k in live {
        assert!(EQUIP_SLOTS.contains(&k), "live slot {k} missing from whitelist");
    }
    for k in ["build", "eyes", "hair", "height", "notable", "skin"] {
        assert!(APPEAR_TRAITS.contains(&k), "live trait {k} missing");
    }
}

// ---------------------------------------------------------------------------
// GET /api/progress
// ---------------------------------------------------------------------------

#[test]
fn progress_parses_at_every_read_size() {
    for chunk in CHUNKS {
        let (s, err, complete): (ProgressSink, _, _) = parse(PROGRESS, chunk);
        assert!(!err, "chunk {chunk}");
        assert!(complete, "chunk {chunk}");
        let p = s.progress;
        assert_eq!(p.consumed_through, 0);
        assert_eq!(p.latest_chapter, 3);
        assert_eq!(p.buffer_target, 3);
        assert_eq!(p.next_chapter, Some(1));
        assert_eq!(p.next_playable, Some(1));
        assert_eq!(p.unread(), 3);
    }
}

#[test]
fn a_null_next_chapter_is_none_not_zero() {
    let src = br#"{"consumed_through":7,"latest_chapter":7,"next_chapter":null,"next_playable":null}"#;
    let (s, err, _): (ProgressSink, _, _) = parse(src, 5);
    assert!(!err);
    assert_eq!(s.progress.next_chapter, None);
    assert_eq!(s.progress.unread(), 0);
}

// ---------------------------------------------------------------------------
// The memory budget, made checkable
// ---------------------------------------------------------------------------

#[test]
fn retained_state_stays_inside_the_bss_budget() {
    // The firmware has **9,408 B** of stack-gap headroom (measured:
    // `tools/preflight.sh --builder fambuild`, gap 81,088 against a 71,680
    // floor). These four models are the app's largest statics, and the streaming
    // client needs ~2 KB more on top (1 KB socket RX + 512 B TX + one 512 B PCM
    // chunk), so the models must stay well under 5 KB for the budget to hold
    // with margin. Asserting it here is what stops the claim from being a
    // comment that quietly goes stale — .bss growth steals stack silently, and
    // this project has already had a stack overflow smash the WiFi globals.
    //
    // Note these are HOST sizes: `usize` is 8 bytes here and 4 on
    // riscv32imac, so every `heapless` length field is half the size on target.
    // The real figure is smaller than what this asserts, which is the safe
    // direction for a budget check to be wrong in.
    let sizes = [
        ("ChapterList", core::mem::size_of::<ChapterList>()),
        ("SegmentIndex", core::mem::size_of::<SegmentIndex>()),
        ("Character", core::mem::size_of::<Character>()),
        ("ProgressSink", core::mem::size_of::<ProgressSink>()),
    ];
    let total: usize = sizes.iter().map(|(_, s)| s).sum();
    // Report, not just gate: the numbers are the point, and `--nocapture` makes
    // them visible without having to break the build to read them.
    eprintln!("retained-state budget (host sizes):");
    for (name, size) in sizes {
        eprintln!("  {name:14} {size:5} B");
    }
    eprintln!("  {:14} {total:5} B", "TOTAL");
    for (name, size) in sizes {
        assert!(size < 2048, "{name} is {size} B — larger than expected");
    }
    assert!(
        total < 5120,
        "the four models total {total} B, over the 5 KB budget: {sizes:?}"
    );
}
