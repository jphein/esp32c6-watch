//! Notification → spoken-text composition.
//!
//! The inputs here are the real thing: `notify::push` ASCII-sanitizes MQTT
//! payloads (non-ASCII → `'?'`) and caps title at 32 / body at 96, so these
//! tests feed the composer exactly what the ring can hand it, including the
//! hostile shapes.

use tts_proto::{compose_utterance, is_speakable, MAX_SPEECH};

#[test]
fn joins_source_title_body_into_sentences() {
    let u = compose_utterance("Home Assistant", "Garage door", "Left open for 15 minutes");
    assert_eq!(u.as_str(), "Home Assistant. Garage door. Left open for 15 minutes.");
}

#[test]
fn does_not_double_existing_punctuation() {
    // A title that already ends in '.' must not become "door.. Left".
    let u = compose_utterance("Home Assistant", "Garage door.", "Left open");
    assert_eq!(u.as_str(), "Home Assistant. Garage door. Left open.");

    // '!' and ':' are sentence-final too.
    let u = compose_utterance("", "Alert!", "Check the stove");
    assert_eq!(u.as_str(), "Alert! Check the stove.");
    let u = compose_utterance("", "Status:", "all clear");
    assert_eq!(u.as_str(), "Status: all clear.");
}

#[test]
fn skips_empty_segments_without_leaving_separators() {
    assert_eq!(compose_utterance("", "Battery low", "").as_str(), "Battery low.");
    assert_eq!(compose_utterance("Battery", "", "").as_str(), "Battery.");
    assert_eq!(compose_utterance("", "", "Just a body").as_str(), "Just a body.");
    // Whitespace-only segments count as empty.
    assert_eq!(compose_utterance("   ", "Title", "  \t ").as_str(), "Title.");
}

#[test]
fn everything_empty_yields_empty_not_punctuation() {
    let u = compose_utterance("", "", "");
    assert!(u.is_empty(), "got {u:?}");
    assert!(!is_speakable(&u));
}

#[test]
fn collapses_whitespace_runs_and_newlines() {
    let u = compose_utterance("", "Multi   space", "line1\nline2\t\tline3");
    assert_eq!(u.as_str(), "Multi space. line1 line2 line3.");
}

#[test]
fn drops_non_ascii_rather_than_voicing_it() {
    // Composer is defensive even though notify::sanitize normally runs first.
    let u = compose_utterance("", "Caf\u{e9} sensor", "temp 21\u{b0}C");
    assert_eq!(u.as_str(), "Caf sensor. temp 21C.");
}

#[test]
fn collapses_placeholder_question_runs_but_keeps_real_ones() {
    // notify::sanitize turns an emoji-laden body into a row of '?'. Speaking
    // "question mark question mark question mark" is worse than dropping it.
    let u = compose_utterance("Home Assistant", "???? Alert", "Door ??? open");
    assert_eq!(u.as_str(), "Home Assistant. Alert. Door open.");

    // A lone '?' is real punctuation and must survive — Azure needs it for
    // interrogative prosody.
    let u = compose_utterance("", "Door still open?", "");
    assert_eq!(u.as_str(), "Door still open?");

    // ...and it counts as sentence-final, so no '.' gets appended after it.
    let u = compose_utterance("", "Ready?", "Yes");
    assert_eq!(u.as_str(), "Ready? Yes.");
}

#[test]
fn a_segment_that_vanishes_entirely_leaves_no_dangling_separator() {
    // Body is pure emoji -> nothing survives -> no trailing ". "
    let u = compose_utterance("Home Assistant", "Alert", "\u{1f525}\u{1f525}");
    assert_eq!(u.as_str(), "Home Assistant. Alert.");
}

#[test]
fn respects_the_cap_and_ends_on_a_word_boundary() {
    let body = "alpha bravo charlie delta echo foxtrot golf hotel india juliet \
                kilo lima mike november oscar papa quebec romeo sierra tango \
                uniform victor whiskey xray yankee zulu";
    let u = compose_utterance("Home Assistant", "Very long notification title", body);

    assert!(u.len() <= MAX_SPEECH, "len {} > cap {}", u.len(), MAX_SPEECH);
    // Ends as a sentence...
    assert!(u.ends_with('.'), "got {u:?}");
    // ...and not mid-word: the char before the final '.' is not a space, and
    // the truncation point fell on a word boundary (no partial token).
    let trimmed = u.trim_end_matches('.');
    assert!(!trimmed.ends_with(' '), "trailing space before period: {u:?}");
    let last_word = trimmed.rsplit(' ').next().unwrap();
    assert!(
        body.split(' ').any(|w| w == last_word) || last_word == "title",
        "truncated mid-word: {last_word:?} in {u:?}"
    );
}

#[test]
fn maximal_ring_notification_fits() {
    // notify::TITLE_CAP = 32, BODY_CAP = 96 — the largest the ring can hold.
    let title = "T".repeat(32);
    let body = "b".repeat(96);
    let u = compose_utterance("Home Assistant", &title, &body);
    assert!(u.len() <= MAX_SPEECH);
    // Nothing was lost: source + title + body all present.
    assert!(u.starts_with("Home Assistant. "));
    assert!(u.contains(&title));
    assert!(u.contains(&body));
}

#[test]
fn is_speakable_rejects_content_free_text() {
    assert!(!is_speakable(""));
    assert!(!is_speakable("   "));
    assert!(!is_speakable("... !!! ---"));
    assert!(is_speakable("ok"));
    assert!(is_speakable("42"));
}

#[test]
fn never_panics_on_adversarial_input() {
    // Fuzz-ish sweep: whatever comes off the broker, this must return.
    let emoji = "\u{1f600}".repeat(200);
    let qmarks = "?".repeat(300);
    let spaces = " ".repeat(300);
    let letters = "a".repeat(1000);
    let nasties: [&str; 7] = [
        "\0\0\0",
        &emoji,
        &qmarks,
        &spaces,
        &letters,
        "\\\"'`<>&;|",
        "\r\n\r\n\r\n",
    ];
    for a in nasties {
        for b in nasties {
            let u = compose_utterance(a, b, a);
            assert!(u.len() <= MAX_SPEECH);
        }
    }
}
