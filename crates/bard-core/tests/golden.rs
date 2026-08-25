//! bard (#300) host-side integration tests: the SBRD blob the firmware ships is the fixture.
//!
//! Gated on `hostsim` (the only feature that compiles the pure cores as a LIBRARY). Run with
//! `cargo test --no-default-features --features hostsim --target x86_64-unknown-linux-gnu
//!  --lib --test bard` — the bare `cargo test` form also builds the firmware BIN, which cannot
//! compile without the `hw` crates.
use bard_core::persona::{prompt, protagonist, PROTAGONISTS};
use bard_core::textflow::wrap_tail;
use bard_core::tokenizer::Tokenizer;
use bard_core::nano_llm::{
    cache_slot, live_slots, Bufs, Model, ParseErr, Session, StepOut, Story, KEEP, POS_MAX, SEQ_CAP,
    WINDOW,
};

pub const BLOB: &[u8] = include_bytes!("../model/stories260K-q8.bin");

#[test]
fn parses_real_blob() {
    let m = Model::parse(BLOB).expect("blob parses");
    assert_eq!(m.cfg.dim, 64);
    assert_eq!(m.cfg.hidden, 172);
    assert_eq!(m.cfg.n_layers, 5);
    assert_eq!(m.cfg.vocab, 512);
    assert_eq!(m.cfg.n_heads, 8);
    assert_eq!(m.cfg.n_kv_heads, 4);
    assert_eq!(m.cfg.gs, 64);
    assert!(m.cfg.shared_cls);
}

#[test]
fn rejects_corruption() {
    let mut bad = BLOB.to_vec();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    assert!(matches!(Model::parse(&bad), Err(ParseErr::Crc)));
    assert!(matches!(Model::parse(&BLOB[..40]), Err(ParseErr::Truncated)));
}

/// Re-stamp the trailing crc32 so a mutated blob fails on the field under test rather than
/// on integrity. Without this every mutant just returns `Crc` and proves nothing.
fn recrc(mut v: std::vec::Vec<u8>) -> std::vec::Vec<u8> {
    let n = v.len() - 4;
    let c = bard_core::nano_llm::crc32(&v[..n]);
    v[n..].copy_from_slice(&c.to_le_bytes());
    v
}

#[test]
fn rejects_bad_header_fields() {
    // Wrong magic.
    let mut v = BLOB.to_vec();
    v[0] ^= 0xFF;
    assert!(matches!(Model::parse(&recrc(v)), Err(ParseErr::Magic)));

    // Unknown format version.
    let mut v = BLOB.to_vec();
    v[4..8].copy_from_slice(&2u32.to_le_bytes());
    assert!(matches!(Model::parse(&recrc(v)), Err(ParseErr::Version)));

    // dim past MAX_DIM (65 > 64).
    let mut v = BLOB.to_vec();
    v[8..12].copy_from_slice(&65u32.to_le_bytes());
    assert!(matches!(Model::parse(&recrc(v)), Err(ParseErr::DimsTooBig)));

    // Header intact, payload 4 bytes short: this is the guard on the EXACT q-section length,
    // which nothing else covers — a blob whose header disagrees with its payload must not run.
    let mut v = BLOB[..BLOB.len() - 4].to_vec();
    v.truncate(v.len() - 4);
    v.extend_from_slice(&[0; 4]);
    assert!(matches!(Model::parse(&recrc(v)), Err(ParseErr::Truncated)));
}

/// Decode `ids[1..n]` exactly as a caller would, WITHOUT any trimming: the BOS-space strip
/// inside `decode` is what has to produce a clean string, so tolerating a leading space here
/// would hide a bug in it.
fn decode_all(t: &Tokenizer, ids: &[u16]) -> std::string::String {
    let mut out = std::vec::Vec::new();
    let mut prev = 1u16; // BOS
    for &id in &ids[1..] {
        out.extend_from_slice(t.decode(prev, id));
        prev = id;
    }
    std::string::String::from_utf8(out).expect("decoded bytes are valid utf8")
}

#[test]
fn tokenizer_roundtrip() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut ids = [0u16; 64];
    let n = t.encode("Once upon a time, there was a little dragon", &mut ids);
    // Exact upstream (llama2.c) tokenization: ' Once' ' upon' ' a' ' time' ',' ' there'
    // ' was' ' a' ' little' ' d' 'r' 'a' 'g' 'on' — "dragon" has no whole-word token here.
    assert_eq!(
        &ids[..n],
        &[1, 403, 407, 261, 378, 432, 383, 286, 261, 376, 279, 420, 412, 428, 289]
    );
    assert_eq!(
        decode_all(&t, &ids[..n]),
        "Once upon a time, there was a little dragon"
    );
}

#[test]
fn tokenizer_seeds_whole_codepoints() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut ids = [0u16; 64];
    // U+2019 RIGHT SINGLE QUOTATION MARK has its OWN token (id 468). Seeding per byte would
    // shred it into three `<0xXX>` fallbacks (229, 131, 156) that can never merge back.
    let n = t.encode("Lily’s cat", &mut ids);
    assert_eq!(&ids[..n], &[1, 317, 468, 419, 280, 294]);
    assert!(ids[..n].contains(&468), "the ’ token must be used");
    for bad in [229u16, 131, 156] {
        assert!(
            !ids[..n].contains(&bad),
            "byte-fallback id {bad} leaked: {:?}",
            &ids[..n]
        );
    }
    assert_eq!(decode_all(&t, &ids[..n]), "Lily’s cat");
}

#[test]
fn tokenizer_new_rejects_malformed_table() {
    let m = Model::parse(BLOB).unwrap();
    let v = m.cfg.vocab;
    // Truncated mid-entry: the walk runs past the end. Unreachable through Model::parse (the
    // CRC would fail first), so it has to be exercised directly.
    let cut = m.tok_table.len() - 1;
    assert!(Tokenizer::new(&m.tok_table[..cut], v).is_none());
    // Shorter than the leading max_token_len word.
    assert!(Tokenizer::new(&[], v).is_none());
    assert!(Tokenizer::new(&m.tok_table[..3], v).is_none());
    // Degenerate / oversized vocab.
    assert!(Tokenizer::new(m.tok_table, 0).is_none());
    assert!(Tokenizer::new(m.tok_table, 100_000).is_none());
    // Trailing junk means the table and the vocab count disagree.
    let mut padded = m.tok_table.to_vec();
    padded.push(0);
    assert!(Tokenizer::new(&padded, v).is_none());
}

#[test]
fn forward_is_deterministic_and_finite() {
    let m = Model::parse(BLOB).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let logits1 = {
        let mut s = Session::new();
        s.forward(&m, &mut bufs, 1, 0).to_vec()
    };
    let logits2 = {
        let mut s = Session::new();
        s.forward(&m, &mut bufs, 1, 0).to_vec()
    };
    assert_eq!(logits1, logits2);
    assert!(logits1.iter().all(|v| v.is_finite()));
    let spread = logits1.iter().cloned().fold(f32::MIN, f32::max)
        - logits1.iter().cloned().fold(f32::MAX, f32::min);
    assert!(spread > 1.0, "logits look degenerate: spread={spread}");
}

// ── #302 KV ring buffer: the arithmetic that lets a story continue ──────────────────────
//
// This is where the bugs would be, and it is pure — so it is tested exhaustively rather than
// sampled. Everything below holds for ANY (SEQ_CAP, KEEP) pair, so it keeps holding if the
// Embassy re-platform (#198) grows the window.

#[test]
fn cache_slot_is_the_identity_until_the_ring_fills() {
    // The golden token stream depends on this: a first chapter must write slots 0, 1, 2 … in
    // order, exactly as the pre-#302 flat cache did.
    for pos in 0..SEQ_CAP {
        assert_eq!(cache_slot(pos), pos, "pos {pos} should still be its own slot");
        assert_eq!(live_slots(pos), pos + 1, "attention should widen by one per token");
    }
    // And then the live set stops growing — every slot holds a token of the sliding window.
    for pos in SEQ_CAP..SEQ_CAP * 4 {
        assert_eq!(live_slots(pos), SEQ_CAP);
    }
}

#[test]
fn cache_slot_keeps_the_last_window_of_positions_addressable() {
    // THE load-bearing invariant: at any position, the SEQ_CAP most recent positions must live in
    // SEQ_CAP DISTINCT slots. If two of them ever collided, attention would read one key twice
    // and silently lose the other — a corruption that looks like bad prose, not like a crash.
    for pos in 0..SEQ_CAP * 5 {
        let live = live_slots(pos);
        let mut seen = [false; SEQ_CAP];
        for back in 0..live {
            let s = cache_slot(pos - back);
            assert!(s < SEQ_CAP, "slot {s} out of the cache at pos {pos}");
            assert!(!seen[s], "pos {} and {} collide in slot {s}", pos - back, pos);
            seen[s] = true;
        }
        // Every slot accounted for once the ring is full: no dead slot, no wasted RAM.
        assert_eq!(seen.iter().filter(|&&v| v).count(), live);
    }
}

#[test]
fn cache_slot_evicts_the_oldest_and_only_the_oldest() {
    // Writing `pos` must land on the slot holding `pos - WINDOW` (the oldest evictable token) and
    // on nothing else — that is what makes the window slide by exactly one.
    for pos in SEQ_CAP..SEQ_CAP * 4 {
        assert_eq!(
            cache_slot(pos),
            cache_slot(pos - WINDOW),
            "pos {pos} should reuse the slot of pos {}",
            pos - WINDOW
        );
        // The pinned prefix (KEEP) is never a write target once the ring is turning.
        assert!(cache_slot(pos) >= KEEP, "pos {pos} overwrote a pinned sink slot");
    }
    // Nothing addressable is outside the cache, all the way to the cursor's limit.
    for pos in [POS_MAX - 1, POS_MAX] {
        assert!(cache_slot(pos) < SEQ_CAP);
    }
}

/// The prompt the golden baseline was generated from (`tools/bard_golden_baseline.sh`).
const GOLDEN_PROMPT: &str = "Once upon a time, there was a little dragon";

/// Greedy-decode `steps` tokens after feeding `prompt`, returning the continuation text and
/// the generated ids. Mirrors `tools/bard_reference.py`'s loop exactly: feed ids[0..n-1], then
/// generate starting from ids[n-1] at position n-1.
fn greedy(
    m: &Model,
    t: &Tokenizer,
    bufs: &mut Bufs,
    prompt: &str,
    steps: usize,
) -> (std::string::String, std::vec::Vec<u16>) {
    let mut ids = [0u16; 64];
    let n = t.encode(prompt, &mut ids);
    let mut s = Session::new();
    for (i, &id) in ids[..n - 1].iter().enumerate() {
        s.forward(m, bufs, id, i);
    }
    let mut token = ids[n - 1];
    let mut text = std::string::String::new();
    let mut generated = std::vec::Vec::new();
    // Stop at the KV cache depth as well as the step budget: this helper drives `forward`
    // directly (it is not a `Story`), so it owes the cache the same discipline `Story` keeps.
    for pos in n - 1..steps.min(SEQ_CAP) {
        let logits = s.forward(m, bufs, token, pos);
        let mut best = 0usize;
        for (i, &v) in logits.iter().enumerate() {
            if v > logits[best] {
                best = i;
            }
        }
        let next = best as u16;
        if next == 1 || next == 2 {
            break;
        }
        generated.push(next);
        text.push_str(&std::string::String::from_utf8_lossy(t.decode(token, next)));
        token = next;
    }
    (text, generated)
}

/// Compare a generated id stream against the golden one, panicking with a DIAGNOSIS rather than
/// a bare inequality. Extracted so `golden_failure_names_the_divergent_token` exercises the real
/// code path instead of a copy of it.
fn assert_ids_match(generated: &[u16], golden: &[u16]) {
    if generated == golden {
        return;
    }
    match generated.iter().zip(golden).position(|(a, b)| a != b) {
        // Real numeric divergence: show a window of both streams around it.
        Some(at) => panic!(
            "token stream diverges at index {at} (rust {} ids, golden {} ids)\n  rust:   {:?}\n  golden: {:?}",
            generated.len(),
            golden.len(),
            &generated[at..generated.len().min(at + 8)],
            &golden[at..golden.len().min(at + 8)],
        ),
        // Every SHARED id matches and only the lengths differ — that is not a numerics bug, it is
        // a stale golden: SEQ_CAP or the step budget moved on one side only. Say that, because
        // "diverges at index None" sends the reader hunting for a rounding error.
        None => panic!(
            "token streams AGREE on all {} shared ids but lengths differ (rust {}, golden {}) \
             — SEQ_CAP or the step budget changed on one side only; rerun \
             tools/bard_golden_baseline.sh to regenerate the goldens",
            generated.len().min(golden.len()),
            generated.len(),
            golden.len(),
        ),
    }
}

#[test]
fn golden_matches_python_reference() {
    let golden = include_str!("../testdata/golden_ref.txt");
    // Bind the golden to the blob it was generated from. Without this, a re-exported model with
    // stale testdata fails as a mystifying prose diff instead of "you forgot to regenerate".
    let stamped = golden
        .lines()
        .next()
        .and_then(|l| l.split("crc32=").nth(1))
        .map(str::trim)
        .expect("golden header must carry crc32=<hex>; run tools/bard_golden_baseline.sh");
    let want_crc = bard_core::nano_llm::crc32(&BLOB[..BLOB.len() - 4]);
    assert_eq!(
        stamped,
        std::format!("{want_crc:08x}"),
        "golden was built for a DIFFERENT blob — rerun tools/bard_golden_baseline.sh"
    );

    let golden_story = golden.lines().skip(1).collect::<std::vec::Vec<_>>().join("\n");
    // The reference prints prompt+continuation as one story; the Rust below produces only the
    // continuation. Strip the prompt so both sides start at the same character.
    let golden_cont = golden_story
        .trim_start()
        .strip_prefix(GOLDEN_PROMPT)
        .expect("golden story must start with the prompt it was generated from");
    let golden_ids: std::vec::Vec<u16> = include_str!("../testdata/golden_tokens.txt")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().expect("token id"))
        .collect();
    // Guard the guard: an empty or truncated testdata file must not pass by comparing nothing.
    assert!(
        golden_ids.len() >= 32,
        "golden_tokens.txt looks empty or truncated: {} ids",
        golden_ids.len()
    );
    assert!(
        golden_cont.chars().count() >= 120,
        "golden continuation is too short to judge: {} chars",
        golden_cont.chars().count()
    );

    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let (text, generated) = greedy(&m, &t, &mut bufs, GOLDEN_PROMPT, 200);

    // FULL equality, no tolerance. T6 measured bit-for-bit agreement across the entire story
    // (186 ids / 446 chars) between two independent implementations in different languages, so
    // anything short of exact is a regression and should fail as loudly as possible. The
    // divergence index is reported first because it localises a numeric bug far better than a
    // prose diff — see the reference's docstring for the accumulation-order contract.
    assert_ids_match(&generated, &golden_ids);
    assert_eq!(text, golden_cont, "text differs despite identical token ids");
}

#[test]
fn greedy_from_bos_is_the_known_opening() {
    // The cheapest regression guard this numeric stack gets: bare BOS is fully deterministic,
    // so any drift in the parser, tokenizer or kernels shows up in the very first words.
    // Regenerate alongside the golden files if the blob ever changes.
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let mut s = Session::new();
    let mut text = std::string::String::new();
    let (mut token, mut prev) = (1u16, 1u16);
    for pos in 0..24 {
        let logits = s.forward(&m, &mut bufs, token, pos);
        let mut best = 0usize;
        for (i, &v) in logits.iter().enumerate() {
            if v > logits[best] {
                best = i;
            }
        }
        token = best as u16;
        if token == 1 || token == 2 {
            break;
        }
        text.push_str(&std::string::String::from_utf8_lossy(t.decode(prev, token)));
        prev = token;
    }
    assert!(
        text.starts_with("Once upon a time, there was a little girl named Lily"),
        "greedy-from-BOS opening changed: {text:?}"
    );
}

/// Drive a `Story` for at most `steps` passes (or until the tale ends), returning its text and
/// whether the tale ended. A bounded drive, not "to completion": since #302 nothing but the model
/// itself (or the POS_MAX cursor) ends a tale, so an unbounded loop is no longer a test — it is a
/// way to hang.
fn run_story(
    m: &Model,
    t: &Tokenizer,
    bufs: &mut Bufs,
    seed: u32,
    steps: u32,
) -> (std::string::String, bool) {
    let mut story = Story::new(t, GOLDEN_PROMPT, seed);
    let mut text = std::string::String::new();
    for _ in 0..steps {
        match story.step(m, t, bufs) {
            StepOut::Text(bytes) => text.push_str(core::str::from_utf8(bytes).unwrap()),
            StepOut::Working => {}
            StepOut::Done { .. } => return (text, true),
        }
    }
    (text, false)
}

#[test]
fn story_generates_and_keeps_going_past_the_cache() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let mut story = Story::new(&t, "Once upon a time, there was a little dragon", 0xC0FFEE);
    let mut text = std::string::String::new();
    // Three windows' worth of passes. The pre-#302 engine would have said `Done` at pass ~65; the
    // ring means the only thing that can stop this seed is the model choosing to.
    for _ in 0..SEQ_CAP * 3 {
        match story.step(&m, &t, &mut bufs) {
            StepOut::Text(bytes) => text.push_str(core::str::from_utf8(bytes).unwrap()),
            StepOut::Working => {}
            StepOut::Done { truncated } => {
                // Only ever the model's own ending — the cursor is nowhere near POS_MAX here.
                assert!(!truncated, "nothing but EOS may end a tale this early");
                break;
            }
        }
    }
    assert!(text.len() > 80, "story too short: {text}");
    assert!(text.is_ascii());
    assert!(story.pos() as usize > SEQ_CAP, "never left the first window");
}

#[test]
fn different_seeds_different_stories() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let (a, _) = run_story(&m, &t, &mut bufs, 1, 120);
    let (b, _) = run_story(&m, &t, &mut bufs, 2, 120);
    assert_ne!(a, b, "two seeds produced the same story");
    // Reusing one Bufs must not couple the runs: the same seed replays identically even after
    // another story has scribbled all over the KV cache — including past the ring's wrap.
    let (a_again, _) = run_story(&m, &t, &mut bufs, 1, 120);
    assert_eq!(a, a_again, "story is not reproducible from its seed");
}

#[test]
fn only_the_model_ends_a_tale() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let mut story = Story::new(&t, GOLDEN_PROMPT, 1);
    // Long enough to be sure: 8 windows is ~10× what the pre-#302 budget allowed.
    let mut ended = None;
    for _ in 0..SEQ_CAP * 8 {
        if let StepOut::Done { truncated } = story.step(&m, &t, &mut bufs) {
            ended = Some(truncated);
            break;
        }
    }
    // Whatever happened, it was not a budget or a cache depth — those are gone. Either the model
    // ended the tale (`truncated == false`) or it is still going.
    match ended {
        Some(truncated) => {
            assert!(!truncated, "only POS_MAX may report truncated, and it is 65,023 away");
            assert!(story.is_done());
            // Once a tale is done it stays done: the SCREEN opens the next one (a fresh `Story`),
            // rather than this object resurrecting itself.
            assert!(matches!(
                story.step(&m, &t, &mut bufs),
                StepOut::Done { truncated: false }
            ));
        }
        None => assert!(
            story.pos() as usize > SEQ_CAP * 7,
            "a tale that did not end must have kept generating"
        ),
    }
}

// ── #302 the endless narrator: generation simply never stops ────────────────────────────

/// Drive `steps` passes of an existing `Story`, returning its text and whether the tale ended.
fn narrate(
    m: &Model,
    t: &Tokenizer,
    bufs: &mut Bufs,
    story: &mut Story,
    steps: u32,
) -> (std::string::String, bool) {
    let mut text = std::string::String::new();
    for _ in 0..steps {
        match story.step(m, t, bufs) {
            StepOut::Text(b) => text.push_str(&std::string::String::from_utf8_lossy(b)),
            StepOut::Working => {}
            StepOut::Done { .. } => return (text, true),
        }
    }
    (text, false)
}

#[test]
fn narration_stays_prose_far_past_the_window() {
    // The reason to test length rather than trust the arithmetic: once the ring wraps, EVERY token
    // is written from a context that has partly slid away, and the way a 260K model fails is to
    // collapse into a repeated phrase. So narrate for many windows and check the text still looks
    // like language. Tales that END are simply followed by the next one, exactly as the screen does.
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);

    let mut all = std::string::String::new();
    let mut tales = 0u32;
    let mut story = Story::new(&t, GOLDEN_PROMPT, 5);
    let mut passes = 0u32;
    while passes < SEQ_CAP as u32 * 6 {
        let (text, ended) = narrate(&m, &t, &mut bufs, &mut story, 40);
        passes += 40;
        all.push_str(&text);
        assert!(text.is_ascii(), "non-ASCII narration: {text:?}");
        if ended {
            // A tale ending is a paragraph break, not a terminal state — the screen opens the next
            // one over the same buffers, and so does this test.
            tales += 1;
            story = Story::new(&t, GOLDEN_PROMPT, 5 + tales);
        }
    }
    // Far more text than any single pre-#302 story could hold (that was ~140 chars).
    assert!(all.len() > 800, "endless narration produced only {} chars", all.len());
    let words: std::vec::Vec<&str> = all.split_whitespace().collect();
    let uniq: std::collections::BTreeSet<&&str> = words.iter().collect();
    assert!(
        uniq.len() * 3 > words.len(),
        "narration collapsed into repetition: {} distinct of {} words\n{all}",
        uniq.len(),
        words.len()
    );
    // Sentences, not one run-on: the model's punctuation should survive the sliding context.
    assert!(all.matches('.').count() > 4, "no sentence structure left: {all}");
}

#[test]
fn generation_is_continuous_across_the_wrap() {
    // Nothing is re-fed, re-primed or re-rotated when the ring wraps, so the token stream either
    // side of the wrap is ONE stream. The check: a narration is a pure function of its seed even
    // when it runs long past the wrap, and another story scribbling over the shared cache in
    // between does not change it (the `Bufs` ownership contract, now with eviction in play).
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);

    let long = |bufs: &mut Bufs| {
        let mut story = Story::new(&t, GOLDEN_PROMPT, 11);
        let (text, _) = narrate(&m, &t, bufs, &mut story, SEQ_CAP as u32 * 2);
        (text, story.pos())
    };
    let (a, pos_a) = long(&mut bufs);
    assert!(pos_a as usize > SEQ_CAP, "fixture never reached the wrap");

    let mut other = Story::new(&t, GOLDEN_PROMPT, 99);
    narrate(&m, &t, &mut bufs, &mut other, 120);
    let (b, pos_b) = long(&mut bufs);
    assert_eq!((a, pos_a), (b, pos_b), "narration is not reproducible from its seed");
}

#[test]
fn an_ended_tale_stays_ended_and_the_next_one_starts_clean() {
    // The screen's next-tale path: a fresh `Story` over the SAME `Bufs`. The old tale's keys are
    // still sitting in the cache, so this is the test that a new tale cannot inherit them — a
    // `Session` at pos 0 attends exactly one slot, the one it just wrote.
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);

    // Find a tale the model ends, then confirm it stays ended.
    let mut ended = None;
    for seed in 1..=8u32 {
        let mut story = Story::new(&t, GOLDEN_PROMPT, seed);
        let (_, done) = narrate(&m, &t, &mut bufs, &mut story, SEQ_CAP as u32 * 8);
        if done {
            assert!(story.is_done());
            assert!(matches!(
                story.step(&m, &t, &mut bufs),
                StepOut::Done { truncated: false }
            ));
            ended = Some(seed);
            break;
        }
    }
    assert!(ended.is_some(), "no seed ended a tale in 8 tries — the EOS path is untested");

    // A brand-new tale over the dirty cache must be identical to one over a pristine cache.
    let fresh = {
        let mut clean = std::boxed::Box::new(Bufs::INIT);
        let mut story = Story::new(&t, GOLDEN_PROMPT, 42);
        narrate(&m, &t, &mut clean, &mut story, 60).0
    };
    let after = {
        let mut story = Story::new(&t, GOLDEN_PROMPT, 42);
        narrate(&m, &t, &mut bufs, &mut story, 60).0
    };
    assert_eq!(fresh, after, "a new tale inherited the previous tale's keys");
}

#[test]
fn a_pause_cannot_perturb_the_token_stream() {
    // JP's pause/play (#302): a press holds the narration and a press resumes it mid-sentence. The
    // property that has to hold is that pausing is INVISIBLE to generation — resume must produce the
    // token the pause interrupted, not a re-primed or re-seeded one.
    //
    // On the device a pause is simply the absence of `step` calls, so this passes by construction
    // today; it is here as the REGRESSION guard for that design. It fails the moment generation
    // acquires a dependency on wall-clock time, on how many times it was called per tick, or on any
    // screen-side state — e.g. if someone later re-seeds the RNG per token from `now_ms`, or decides
    // a resume should re-feed the prompt "to re-establish context".
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();

    // Straight through, past the ring's wrap so the paused run has eviction in play too.
    let steps = SEQ_CAP as u32 * 2;
    let uninterrupted = {
        let mut bufs = std::boxed::Box::new(Bufs::INIT);
        let mut story = Story::new(&t, GOLDEN_PROMPT, 13);
        narrate(&m, &t, &mut bufs, &mut story, steps).0
    };

    // The same run, held twice — once before the wrap and once after — with the screen-side work a
    // paused board actually does in between (it keeps painting: wrapping the text for the panel and
    // rolling the scrollback), to prove none of that reaches the generator.
    let paused = {
        let mut bufs = std::boxed::Box::new(Bufs::INIT);
        let mut story = Story::new(&t, GOLDEN_PROMPT, 13);
        let mut text = std::string::String::new();
        let third = steps / 3;
        for (i, chunk) in [third, third, steps - 2 * third].iter().enumerate() {
            if i > 0 {
                // "While paused": many ticks of panel work, and NOT ONE `step` call.
                let mut spans = [(0u16, 0u16); 5];
                let mut buf = text.as_bytes().to_vec();
                for _ in 0..50 {
                    wrap_tail(buf.as_slice(), 14, 5, &mut spans);
                    let len = buf.len();
                    bard_core::textflow::append_rolling(&mut buf, len, b"", 512, len);
                }
            }
            text.push_str(&narrate(&m, &t, &mut bufs, &mut story, *chunk).0);
        }
        text
    };

    assert_eq!(
        uninterrupted, paused,
        "a pause changed the story — generation has acquired a dependency it must not have"
    );
    assert!(uninterrupted.len() > 200, "fixture too short to mean anything");
}

#[test]
fn the_position_cursor_cannot_wrap() {
    // POS_MAX is the only limit an endless narrator has left, so the arithmetic around it has to be
    // provably safe: `Story` stops AT it and `forward` asserts it, both in u16.
    assert!(POS_MAX < u16::MAX as usize, "POS_MAX must leave the u16 cursor room");
    assert!(POS_MAX > SEQ_CAP * 100, "POS_MAX should be a runtime bound, not a story bound");
    // Every position up to and including the bound addresses a real slot — no gap where the ring
    // arithmetic would land outside the cache just before the cursor recycles.
    for pos in [POS_MAX - 1, POS_MAX] {
        assert!(cache_slot(pos) < SEQ_CAP);
        assert_eq!(live_slots(pos), SEQ_CAP);
    }
}

// ── #302 delivery: the CFG `V` value that sets pace and mode ────────────────────────────
//
// A parser for bytes that arrive from a broker, so the tests are mostly about REFUSING things. The
// contract each one pins: a refusal leaves the caller's setting alone, and a clamp says so.

#[test]
fn delivery_parses_the_documented_forms() {
    use bard_core::delivery::{Delivery, Mode};
    let cur = Delivery::DEFAULT;
    let ok = |v: &str| Delivery::parse(v.as_bytes(), cur).expect("should parse");

    assert_eq!(ok("160:inf").delivery, Delivery::DEFAULT);
    assert_eq!(ok("80:page").delivery.ms_per_char, 80);
    assert_eq!(ok("80:page").delivery.mode, Mode::Page);
    // Case is forgiven on the mode word (an operator may type it).
    assert_eq!(ok("80:PAGE").delivery.mode, Mode::Page);
    assert_eq!(ok("80:Inf").delivery.mode, Mode::Inf);
    // Empty FIELD = keep that field; empty VALUE = back to the board defaults (retain-clear).
    let paged = Delivery { ms_per_char: 40, mode: Mode::Page, font: Delivery::DEFAULT.font };
    assert_eq!(Delivery::parse(b":inf", paged).unwrap().delivery.ms_per_char, 40);
    assert_eq!(Delivery::parse(b"300:", paged).unwrap().delivery.mode, Mode::Page);
    assert_eq!(Delivery::parse(b"", paged).unwrap().delivery, Delivery::DEFAULT);
    // Nothing above should report a clamp.
    for v in ["160:inf", "80:page", "20:inf", "500:page"] {
        assert!(!ok(v).clamped, "{v} should not clamp");
    }
}

#[test]
fn delivery_parses_the_optional_font_field() {
    use bard_core::delivery::{Delivery, Font, Mode};
    let cur = Delivery::DEFAULT;
    let ok = |v: &str, c: Delivery| Delivery::parse(v.as_bytes(), c).expect("should parse");

    // The compatibility requirement is that a value already retained on the broker (TWO fields) must
    // keep PARSING — it does, and its speed and mode are honoured verbatim.
    let big = Delivery { ms_per_char: 200, mode: Mode::Page, font: Font::F9x15 };
    assert_eq!(ok("120:inf", big).delivery.ms_per_char, 120);
    assert_eq!(ok("120:inf", big).delivery.mode, Mode::Inf);
    // But an ABSENT font means DEFAULT, not keep-current. This assertion was the opposite way round
    // until the dashboard proved it wrong: HA omits the third field exactly when the value IS the
    // default (pre-font-firmware compatibility), so keep-current made the default UNREACHABLE — pick
    // 9x15, select 5x8, and the node keeps 9x15 with no error anywhere. A value written before the
    // field existed cannot express a font preference, so "the default" is its only honest reading.
    assert_eq!(
        ok("120:inf", big).delivery.font,
        Delivery::DEFAULT.font,
        "an absent font field must mean DEFAULT, or the default is unreachable from the dashboard"
    );
    // An EMPTY third field reads the same as an omitted one — one rule, not two.
    assert_eq!(ok("120:inf:", big).delivery.font, Delivery::DEFAULT.font);

    // All four faces, case-insensitively, and the default.
    assert_eq!(ok("160:inf:5x8", big).delivery.font, Font::F5x8);
    assert_eq!(ok("160:inf:6x10", big).delivery.font, Font::F6x10);
    assert_eq!(ok("160:inf:9X15", big).delivery.font, Font::F9x15);
    assert_eq!(ok("160:inf:10X20", big).delivery.font, Font::F10x20);
    assert_eq!(Delivery::DEFAULT.font, Font::F5x8, "the default must stay the densest face");
    // An empty VALUE is still the full reset, font included.
    assert_eq!(Delivery::parse(b"", big).unwrap().delivery, Delivery::DEFAULT);

    // The longest legal value must fit the length bound, or the dashboard's own maximum is refused.
    assert!("500:page:10x20".len() <= Delivery::MAX_LEN);
    assert!(Delivery::parse(b"500:page:10x20", cur).is_ok());
}

#[test]
fn delivery_refuses_an_unknown_font() {
    use bard_core::delivery::{Delivery, DeliveryErr, Font, Mode};
    let cur = Delivery { ms_per_char: 40, mode: Mode::Page, font: Font::F6x10 };
    // A plausible-looking but wrong token must be refused rather than guessed at — and the previous
    // font kept, since a refusal applies nothing.
    assert_eq!(Delivery::parse(b"160:inf:8x13", cur), Err(DeliveryErr::BadFont));
    assert_eq!(Delivery::parse(b"160:inf:big", cur), Err(DeliveryErr::BadFont));
    assert_eq!(Delivery::parse(b"160:inf:5x9", cur), Err(DeliveryErr::BadFont));
    // A fourth field is not a font token, so it is refused rather than silently ignored.
    assert!(matches!(
        Delivery::parse(b"160:inf:5x8:x", cur),
        Err(DeliveryErr::BadFont)
    ));
    assert_eq!(cur.font, Font::F6x10);
}

#[test]
fn every_font_lays_out_a_sane_panel() {
    use bard_core::delivery::Font;
    // The geometry the panel actually gets, and the invariants `draw_story` depends on: a font must
    // never yield zero rows/cols (division by nothing) nor more than the smallest face does (the span
    // buffer and page budget are cut to that maximum, since they cannot be runtime-sized without
    // alloc). Values are the 72x40 panel divided by each face's glyph box.
    for (f, cols, rows) in [
        (Font::F5x8, 14usize, 5usize),
        (Font::F6x10, 12, 4),
        (Font::F9x15, 8, 2),
        (Font::F10x20, 7, 2),
    ] {
        let (c, r) = f.grid();
        assert_eq!((c, r), (cols, rows), "{f:?} geometry");
        assert!(c >= 1 && r >= 1, "{f:?} must have at least one row and column");
        assert!(c <= 14 && r <= 5, "{f:?} must fit the buffers cut for the smallest face");
        // wrap_tail must stay correct at every size — it is parameterised, so this is the check that
        // the parameters we now pass at runtime are ones it handles.
        let mut spans = [(0u16, 0u16); 5];
        let text = "the little dragon flew over the sleeping town and sang";
        let n = wrap_tail(text.as_bytes(), c, r, &mut spans[..r]);
        assert!(n <= r, "{f:?}: {n} lines exceeds {r} rows");
        for &(a, b) in &spans[..n] {
            assert!(a <= b && (b - a) as usize <= c, "{f:?}: span wider than {c} cols");
        }
    }
}

#[test]
fn delivery_clamps_the_speed_instead_of_refusing_it() {
    use bard_core::delivery::{Delivery, Mode};
    let cur = Delivery::DEFAULT;
    // 0 is the one that matters: an unclamped 0 ms/char makes the reveal loop run the whole buffer
    // every tick, which is a pegged CPU rather than a fast typewriter.
    let z = Delivery::parse(b"0:inf", cur).unwrap();
    assert_eq!(z.delivery.ms_per_char, Delivery::MS_MIN);
    assert!(z.clamped, "a clamp must be reported so the log can say so");

    let slow = Delivery::parse(b"9999:page", cur).unwrap();
    assert_eq!(slow.delivery.ms_per_char, Delivery::MS_MAX);
    assert!(slow.clamped);
    assert_eq!(slow.delivery.mode, Mode::Page, "clamping the speed must not lose the mode");

    // Absurd input must saturate, not wrap into range: "99999999" as u16 arithmetic could land
    // anywhere, and landing on 3 ms/char would be a pegged board from a typo.
    let huge = Delivery::parse(b"99999999:inf", cur).unwrap();
    assert_eq!(huge.delivery.ms_per_char, Delivery::MS_MAX);
    assert!(huge.clamped);
}

#[test]
fn delivery_refuses_the_rest_and_keeps_the_previous_setting() {
    use bard_core::delivery::{Delivery, DeliveryErr, Mode};
    let cur = Delivery { ms_per_char: 40, mode: Mode::Page, font: Delivery::DEFAULT.font };

    // No separator: refused rather than guessed at.
    assert_eq!(Delivery::parse(b"160", cur), Err(DeliveryErr::Malformed));
    // Not a number.
    assert_eq!(Delivery::parse(b"fast:inf", cur), Err(DeliveryErr::BadSpeed));
    assert_eq!(Delivery::parse(b"16 0:inf", cur), Err(DeliveryErr::BadSpeed));
    // Arbitrary wire bytes in the speed field (a CFG payload is not necessarily UTF-8).
    assert_eq!(Delivery::parse(&[0xff, b':', b'i', b'n', b'f'], cur), Err(DeliveryErr::BadSpeed));
    // Unknown mode word — including a plausible one, which is exactly why guessing is refused.
    assert_eq!(Delivery::parse(b"160:slow", cur), Err(DeliveryErr::BadMode));
    assert_eq!(Delivery::parse(b"160:infinite", cur), Err(DeliveryErr::BadMode));
    // Over the length bound, refused before anything is looked at.
    let long = "1".repeat(Delivery::MAX_LEN + 1);
    assert!(matches!(
        Delivery::parse(long.as_bytes(), cur),
        Err(DeliveryErr::TooLong { .. })
    ));

    // A refusal returns Err and NOTHING else — the caller keeps `cur` because there is no other
    // value to apply. (The firmware's `set_delivery` only writes the static on Ok.)
    assert_eq!(
        cur,
        Delivery { ms_per_char: 40, mode: Mode::Page, font: Delivery::DEFAULT.font }
    );
}

#[test]
fn delivery_defaults_are_the_shipped_behaviour() {
    use bard_core::delivery::{Delivery, Mode};
    // The default IS the headline feature — a bard that never stops — and the pre-#302 reading pace.
    assert_eq!(Delivery::DEFAULT.mode, Mode::Inf);
    assert_eq!(Delivery::DEFAULT.ms_per_char, 160);
    assert_eq!(Delivery::DEFAULT.reveal_ms(), 160);
    // The range must bracket the default, or the shipped setting would itself be clamped.
    assert!(Delivery::MS_MIN <= Delivery::MS_DEFAULT && Delivery::MS_DEFAULT <= Delivery::MS_MAX);
}

#[test]
fn delivery_dedupes_a_retained_re_arm() {
    use bard_core::delivery::LastOffer;
    // The gateway re-offers a RETAINED value on every burst, so "have I seen this?" is what keeps a
    // soak's log readable (the bench saw 8 identical apply lines before this existed).
    let mut last = LastOffer::NONE;
    assert!(last.is_new(b"160:inf"), "the first offer is always new");
    assert!(!last.is_new(b"160:inf"), "a re-armed identical value must be recognised");
    assert!(last.is_new(b"80:page"), "a changed value must be new again");
    assert!(!last.is_new(b"80:page"));
    // A bad value dedupes exactly the same way — otherwise a retained typo warns for ever — but a
    // DIFFERENT bad value must still be reported.
    assert!(last.is_new(b"160:slow"));
    assert!(!last.is_new(b"160:slow"));
    assert!(last.is_new(b"160:fast"));

    // The empty offer is a REAL value (restore defaults), not "nothing seen yet": it must be new
    // once and then dedupe, which is the bug a zero-as-sentinel would have.
    let mut fresh = LastOffer::NONE;
    assert!(fresh.is_new(b""), "an empty offer is a meaningful value");
    assert!(!fresh.is_new(b""));
    assert!(fresh.is_new(b"160:inf"));

    // Over-long values keep only a prefix, so length has to be part of the identity or two distinct
    // refusals would collapse into one silent one.
    let a = "1".repeat(40);
    let b = "1".repeat(41);
    let mut long = LastOffer::NONE;
    assert!(long.is_new(a.as_bytes()));
    assert!(!long.is_new(a.as_bytes()));
    assert!(long.is_new(b.as_bytes()), "a different length must not dedupe on a shared prefix");
}

#[test]
fn persona_prompts_fit_the_vocabulary() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    assert_eq!(PROTAGONISTS.len(), 16);
    // The three live boards get a creature matching their realm persona (spec §8).
    assert!(protagonist(7).contains("dragon"), "id7 Draconic Dominion");
    assert!(protagonist(8).contains("owl"), "id8 Eldritch Nexus");
    assert!(protagonist(9).contains("bird"), "id9 Jade Herald");

    let mut ids = [0u16; 64];
    // EVERY node id, not just the 16 table entries: the `% 16` fallback has to hold for an
    // unprovisioned board too, and a prompt is the one string the model never chose.
    for id in 0..=255u8 {
        let mut buf = [0u8; 64];
        let n = prompt(id, &mut buf);
        let s = core::str::from_utf8(&buf[..n]).expect("prompt is ASCII");
        let k = t.encode(s, &mut ids);
        assert!(k < 32, "prompt for node {id} is {k} tokens: {s:?}");
        // Spec §8's real requirement: every word must exist in the 512-token vocabulary. Ids
        // 3..=258 are the `<0xXX>` byte fallbacks — one of those means the prompt SHREDDED,
        // which is exactly what would happen if we fed the model a realm name.
        assert!(
            !ids[..k].iter().any(|i| (3..=258).contains(i)),
            "node {id} prompt {s:?} hit byte fallback: {:?}",
            &ids[..k]
        );
    }
}

/// Render `wrap_tail`'s spans back into visible lines, so the assertions read like the panel.
fn wrapped(text: &str, cols: usize, rows: usize) -> std::vec::Vec<&str> {
    let mut spans = [(0u16, 0u16); 8];
    let n = wrap_tail(text.as_bytes(), cols, rows, &mut spans[..rows]);
    // The fixtures are ASCII, so the byte spans are also char boundaries and the &str can be
    // sliced directly (the firmware renderer decodes defensively instead — see draw_story).
    spans[..n].iter().map(|&(a, b)| &text[a as usize..b as usize]).collect()
}

#[test]
fn wrap_tail_lays_out_the_panel() {
    // Empty text has nothing to show — not one blank line.
    assert_eq!(wrapped("", 14, 5), std::vec::Vec::<&str>::new());

    // Exactly the column count must NOT spill into a second line.
    assert_eq!(wrapped("abcdefghijklmn", 14, 5), ["abcdefghijklmn"]);
    assert_eq!(wrapped("abcdefghijklmno", 14, 5), ["abcdefghijklmn", "o"]);

    // Greedy break at the last space, and the space itself is consumed (not leading the line).
    assert_eq!(wrapped("the dragon flew away", 14, 5), ["the dragon", "flew away"]);

    // A word longer than the panel is hard-broken rather than lost.
    assert_eq!(
        wrapped("supercalifragilistic", 14, 5),
        ["supercalifragi", "listic"]
    );

    // Model-emitted newlines are hard breaks.
    assert_eq!(wrapped("one\ntwo", 14, 5), ["one", "two"]);

    // 6 lines into 5 rows keeps the LAST five — the panel scrolls.
    let six = "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll";
    let all = wrapped(six, 7, 8);
    assert_eq!(all.len(), 6, "fixture should wrap to 6 lines: {all:?}");
    let tail = wrapped(six, 7, 5);
    assert_eq!(tail.len(), 5);
    assert_eq!(tail[..], all[1..], "should drop the OLDEST line");
}

#[test]
fn wrap_tail_survives_every_reveal_position_at_every_font() {
    // The typewriter re-wraps a growing prefix every frame, so the invariants have to hold at EVERY
    // prefix — not just for the tidy synthetic cases above. This walks a real generated story one
    // revealed character at a time, exactly as the panel does.
    //
    // #302 widened it to every SELECTABLE FONT. This is the test that caught the 14-vs-15 wrap bug at
    // one fixed size, and a runtime font makes the geometry an input rather than a constant: 7 columns
    // and 2 rows exercise the rotating window, the swallowed space and the hard mid-word break far
    // harder than 14x5 ever did (more breaks per character revealed, and the window turns over every
    // two lines instead of every five).
    let golden = include_str!("../testdata/golden_ref.txt");
    let story = golden.lines().skip(1).collect::<std::vec::Vec<_>>().join("\n");
    let bytes = story.as_bytes();

    for font in [
        bard_core::delivery::Font::F5x8,
        bard_core::delivery::Font::F6x10,
        bard_core::delivery::Font::F9x15,
        bard_core::delivery::Font::F10x20,
    ] {
        let (cols, rows) = font.grid();
        // The firmware's own buffer bound: spans are cut to the SMALLEST face's row count, since they
        // cannot be runtime-sized without alloc.
        let mut spans = [(0u16, 0u16); 5];
        assert!(rows <= spans.len(), "{font:?} wants {rows} rows, buffer holds {}", spans.len());
        for shown in 0..=bytes.len() {
            let n = wrap_tail(&bytes[..shown], cols, rows, &mut spans[..rows]);
            assert!(n <= rows, "{font:?} shown={shown}: {n} lines exceeds the panel");
            for (i, &(a, b)) in spans[..n].iter().enumerate() {
                assert!(a <= b, "{font:?} shown={shown} line{i}: inverted span ({a},{b})");
                assert!(
                    (b as usize) <= shown,
                    "{font:?} shown={shown} line{i}: span end {b} past the revealed text"
                );
                let w = (b - a) as usize;
                assert!(
                    w <= cols,
                    "{font:?} shown={shown} line{i}: {w} chars is wider than {cols}"
                );
                // A line must never begin with the space we were supposed to swallow, or the text
                // would visibly drift right as the story scrolls.
                if w > 0 && a > 0 {
                    assert_ne!(
                        bytes[a as usize], b' ',
                        "{font:?} shown={shown} line{i}: line starts with a space"
                    );
                }
            }
            // Lines must be in reading order and non-overlapping.
            for w in spans[..n].windows(2) {
                assert!(
                    w[0].1 <= w[1].0,
                    "{font:?} shown={shown}: spans out of order/overlapping"
                );
            }
        }
        // And with the bottom row given to the `|| paused` / `~ more ~` marker, the story still fills
        // what is left. Only meaningful where a marker is actually drawn: at 2 rows the panel keeps
        // both for the story (see `draw_story`), so there is no reserved row to test.
        if rows > 2 {
            let n = wrap_tail(bytes, cols, rows - 1, &mut spans[..rows - 1]);
            assert_eq!(n, rows - 1, "{font:?}: a full story should fill the reserved layout");
        }
    }
}

// ── #302 rolling scrollback: what a continuation does to the text buffer ────────────────

/// `roll` with the buffer/len bookkeeping the firmware does around it, so the fixtures read like
/// the panel's state rather than like a slice call.
fn rolled(text: &str, keep: usize, revealed: usize) -> (std::string::String, usize) {
    let mut buf = text.as_bytes().to_vec();
    let dropped = bard_core::textflow::roll(&mut buf, text.len(), keep, revealed);
    let kept = std::string::String::from_utf8(buf[..text.len() - dropped].to_vec())
        .expect("roll must never split a character");
    (kept, dropped)
}

#[test]
fn roll_drops_the_oldest_text_and_says_how_much() {
    // Under the keep bound: nothing moves (the common case — one chapter does not fill 1 KB).
    assert_eq!(rolled("a short story", 256, 999), ("a short story".into(), 0));

    // Over it: the OLDEST bytes go and the tail slides to offset 0.
    let (kept, dropped) = rolled("0123456789", 4, 999);
    assert_eq!((kept.as_str(), dropped), ("6789", 6));

    // Degenerate bounds must not panic or wrap: keep 0, and an empty buffer.
    assert_eq!(rolled("abc", 0, 999), ("".into(), 3));
    assert_eq!(rolled("", 256, 999), ("".into(), 0));
}

#[test]
fn roll_never_eats_an_unread_word() {
    // The typewriter is 4 bytes in, so at most 4 bytes may be dropped even though the keep bound
    // asks for 6 — the reader has not seen bytes 4.. yet, and losing them would silently skip
    // text mid-story.
    let (kept, dropped) = rolled("0123456789", 4, 4);
    assert_eq!((kept.as_str(), dropped), ("456789", 4));
    // Nothing revealed at all ⇒ nothing may be dropped, however full the buffer is.
    assert_eq!(rolled("0123456789", 2, 0), ("0123456789".into(), 0));
}

#[test]
fn roll_never_splits_a_character() {
    // U+2019 (’) is one 3-byte token in this vocabulary. A cut that lands inside it must walk
    // FORWARD to the next character boundary — a dangling continuation byte would make
    // `draw_story` skip the entire line it lands on, losing a line of story to a blank.
    let text = "ab’cd"; // bytes: a b E2 80 99 c d
    assert_eq!(text.len(), 7);
    for keep in 0..=text.len() {
        let (kept, dropped) = rolled(text, keep, 999);
        assert!(
            !kept.is_empty() || dropped == text.len(),
            "keep={keep} produced an empty buffer without dropping everything"
        );
        // The invariant: whatever is kept is still decodable (checked inside `rolled`) and the
        // cut landed on a boundary, never at byte 3 or 4.
        assert_ne!(dropped, 3, "cut inside the ’ token");
        assert_ne!(dropped, 4, "cut inside the ’ token");
    }
}

/// The firmware's own scrollback geometry (`bard/mod.rs`), so the simulation below rolls where the
/// board rolls.
const BUF: usize = 1024;
const TEXT_KEEP: usize = 512;

#[test]
fn endless_narration_never_breaks_the_panel() {
    // The integration test for #302's rolling scrollback: drive a REAL endless narration through the
    // REAL append-with-roll policy and the REAL wrapper, one token at a time, revealing as the
    // typewriter would — then assert the panel invariants at every single step. This is the test
    // that would catch a roll that splits a character, drops unread text, or leaves the reveal
    // cursor pointing into the middle of a line.
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let mut bufs = std::boxed::Box::new(Bufs::INIT);
    let mut story = Story::new(&t, GOLDEN_PROMPT, 4);

    let mut buf = std::vec![0u8; BUF];
    let mut len = 0usize;
    let mut shown = 0usize;
    let mut spans = [(0u16, 0u16); 5];
    let mut rolls = 0u32;
    let mut tales = 1u32;

    // Enough passes to overflow the 1 KB buffer several times over (~2.7 bytes/token).
    for _ in 0..1200 {
        let bytes: std::vec::Vec<u8> = match story.step(&m, &t, &mut bufs) {
            StepOut::Text(b) => b.to_vec(),
            StepOut::Working => std::vec::Vec::new(),
            StepOut::Done { .. } => {
                // The screen's next-tale path: a paragraph break, then the next opening.
                tales += 1;
                story = Story::new(&t, GOLDEN_PROMPT, 4 + tales);
                std::vec![b'\n']
            }
        };
        if !bytes.is_empty() {
            let before = len;
            let (new_len, dropped) =
                bard_core::textflow::append_rolling(&mut buf, len, &bytes, TEXT_KEEP, shown);
            if dropped > 0 {
                rolls += 1;
                // Never more than the reader has seen: the cut is bounded by `shown`.
                assert!(dropped <= before, "rolled {dropped} of a {before}-byte buffer");
                assert!(dropped <= shown, "rolled past the reveal cursor ({dropped} > {shown})");
                shown -= dropped;
            }
            len = new_len;
            assert!(len <= BUF, "buffer overran: {len}");
            // What went in came out the other end (modulo the roll) — no silently lost token.
            assert_eq!(
                len,
                before - dropped + bytes.len(),
                "append lost bytes: before={before} dropped={dropped} extra={}",
                bytes.len()
            );
        }
        // Reveal at ~2 chars per token, which is SLOWER than generation — i.e. the backlog the
        // firmware's backpressure bounds. Rolling under a lagging reveal is the hard case.
        shown = (shown + 2).min(len);

        // The panel, at this exact instant. Same assertions as the single-story reveal test, now
        // with the rolling buffer underneath.
        let visible = &buf[..shown];
        let n = wrap_tail(visible, 14, 5, &mut spans);
        assert!(n <= 5, "{n} lines exceeds the panel");
        for (i, &(a, b)) in spans[..n].iter().enumerate() {
            assert!(a <= b, "line{i}: inverted span ({a},{b})");
            assert!((b as usize) <= shown, "line{i}: span past the revealed text");
            assert!((b - a) as usize <= 14, "line{i}: wider than the panel");
            // Every line must still decode: a roll that split a character would show up here as a
            // line the renderer has to skip.
            assert!(
                core::str::from_utf8(&visible[a as usize..b as usize]).is_ok(),
                "line{i} is not valid UTF-8 after a roll: {:?}",
                &visible[a as usize..b as usize]
            );
        }
        for w in spans[..n].windows(2) {
            assert!(w[0].1 <= w[1].0, "spans out of order/overlapping");
        }
    }
    // The fixture has to actually exercise the interesting paths, or it proves nothing.
    assert!(rolls > 0, "the buffer never rolled — fixture too short to test anything");
    assert!(tales > 1, "no tale boundary in 1200 passes — the next-tale path went untested");
}

#[test]
fn golden_failure_names_the_divergent_token() {
    // The length-only arm has been exercised for real (planting stale goldens against a changed
    // SEQ_CAP). The numeric-divergence arm never has, because the two implementations have never
    // disagreed — so provoke it deliberately, on an in-memory COPY. The committed testdata is
    // never touched: a test that corrupts a fixture it shares with another test is a trap.
    let golden: std::vec::Vec<u16> = include_str!("../testdata/golden_tokens.txt")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().expect("token id"))
        .collect();
    let at = 40usize;
    assert!(golden.len() > at + 8, "fixture too short to window around {at}");
    let mut mutated = golden.clone();
    mutated[at] = 999; // not a real id in this vocabulary — unmistakable in the output

    // Silence the expected panic so a passing run does not print a scary backtrace.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(std::boxed::Box::new(|_| {}));
    let payload = std::panic::catch_unwind(|| assert_ids_match(&mutated, &golden))
        .expect_err("a flipped id must fail the comparison");
    std::panic::set_hook(prev_hook);

    let msg = payload
        .downcast_ref::<std::string::String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic payload>");
    // It must name the position, not just complain.
    assert!(
        msg.contains(&std::format!("diverges at index {at}")),
        "expected the divergence index in: {msg}"
    );
    // And BOTH windows must straddle the divergence — the rust side showing the flipped id, the
    // golden side the original, each starting at `at`.
    let want_rust = std::format!("{:?}", &mutated[at..at + 8]);
    let want_golden = std::format!("{:?}", &golden[at..at + 8]);
    assert!(msg.contains(&want_rust), "expected rust window {want_rust} in: {msg}");
    assert!(
        msg.contains(&want_golden),
        "expected golden window {want_golden} in: {msg}"
    );
}

// (stack_paint_scanner test dropped: stack_paint is a device instrument, not
// part of bard's inference — it stays in smol/firmware, not this pure crate.)


// ── #303 runtime prompt validation ──────────────────────────────────────────────────────

/// The built-in prompts must all pass the validator they gate operator input with — otherwise
/// the firmware would reject its own defaults.
#[test]
fn builtin_prompts_pass_validation() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    for id in 0u8..=255 {
        let mut buf = [0u8; 64];
        let n = bard_core::persona::prompt(id, &mut buf);
        let got = bard_core::persona::validate_prompt(&t, &buf[..n]);
        assert!(got.is_ok(), "node {id} default prompt rejected: {got:?}");
        assert!(got.unwrap() <= bard_core::persona::PROMPT_TOKEN_BUDGET);
    }
}

#[test]
fn validation_accepts_plain_tinystories_prose() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    for good in [
        "Once upon a time, there was a little cat",
        "One day, a small dog went to the park",
        "The little bird was very happy",
    ] {
        let r = bard_core::persona::validate_prompt(&t, good.as_bytes());
        assert!(r.is_ok(), "rejected good prompt {good:?}: {r:?}");
    }
}

#[test]
fn validation_rejects_the_four_failure_modes() {
    use bard_core::persona::PromptErr;
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();

    // 1. over the 64-byte buffer/CFG-value bound
    let long = "a".repeat(65);
    assert!(matches!(
        bard_core::persona::validate_prompt(&t, long.as_bytes()),
        Err(PromptErr::TooLong { got: 65 })
    ));

    // 2. not UTF-8 (a CFG payload is arbitrary wire bytes)
    assert_eq!(
        bard_core::persona::validate_prompt(&t, &[0xff, 0xfe]),
        Err(PromptErr::NotUtf8)
    );

    // 3. an emoji has no token at all -> byte fallbacks -> refused, and it says WHERE
    match bard_core::persona::validate_prompt(&t, "the cat 🐱 sat".as_bytes()) {
        Err(PromptErr::UnrepresentableByte { at_byte }) => {
            assert!(at_byte > 0, "position should locate the emoji, got {at_byte}")
        }
        other => panic!("emoji should be refused, got {other:?}"),
    }

    // 4. fits 64 bytes but spends too much of the shared window. Note WHICH text does this:
    // ordinary prose is dense (~2.5 B/token, so 58 bytes is only ~23 tokens and passes), so the
    // budget is reached by FRAGMENTATION — nonsense words encode ~1 token/char. That makes the
    // budget an automatic backstop on hazard (2): mild fragmentation is accepted and reported,
    // severe fragmentation refuses itself here.
    let fragmenting = "Xyzzy Plugh Frobnitz Quux Zork Grue Blorb Vogon";
    assert!(fragmenting.len() <= 64);
    match bard_core::persona::validate_prompt(&t, fragmenting.as_bytes()) {
        Err(PromptErr::TooManyTokens { got }) => {
            assert!(got > bard_core::persona::PROMPT_TOKEN_BUDGET)
        }
        other => panic!("a window-hogging prompt should be refused, got {other:?}"),
    }
}

/// The honest distinction that the doc comment claims: an ASCII realm name is REPRESENTABLE
/// (single-char tokens exist) so it is ACCEPTED — but it fragments, and the token count is the
/// signal. If a future vocabulary changes this, this test says so instead of the comment lying.
#[test]
fn ascii_realm_names_are_accepted_but_fragment() {
    let m = Model::parse(BLOB).unwrap();
    let t = Tokenizer::new(m.tok_table, m.cfg.vocab).unwrap();
    let odd = "Once upon a time, there was Eldritch";
    let n = bard_core::persona::validate_prompt(&t, odd.as_bytes())
        .expect("ASCII is representable, so this must be accepted, not refused");
    let plain = "Once upon a time, there was a little owl";
    let n_plain = bard_core::persona::validate_prompt(&t, plain.as_bytes()).unwrap();
    // Same byte-length ballpark, materially more tokens = the fragmentation the operator is warned about.
    assert!(
        n > n_plain,
        "expected 'Eldritch' to fragment into more tokens ({n}) than plain prose ({n_plain})"
    );
}
