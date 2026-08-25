//! bard (#300): word-wrap for the 72×40 panel — the one piece of the UI that is pure logic,
//! so it is host-tested instead of eyeballed on hardware.
//!
//! Pure, `no_std`, zero alloc: [`wrap_tail`] returns byte SPANS into the caller's text rather
//! than copied lines, so the 1 KB story buffer is never duplicated. That frugality is not
//! stylistic — on the canonical fleet tier the bard's `.bss` comes out of the RUNTIME STACK,
//! which the linker shrinks silently, leaving 14240 B (see `nano_llm::SEQ_CAP` for all three
//! measured points). `tools/repro_build.sh` refuses to package an image with less than 12288 B,
//! so a second copy of the story buffer here could turn into a failed release build.

/// Roll the scrollback: drop the oldest bytes of a `len`-byte story so a continuation (#302) has
/// room, moving the rest down to offset 0 and returning how many bytes went.
///
/// Pure, so the two ways this could quietly ruin the panel are host-tested instead of eyeballed:
///   * **A reader can never lose an unread word.** `revealed` (the typewriter's cursor) bounds the
///     cut, so a compaction that lands while the reveal is lagging keeps the buffer fuller rather
///     than eating text nobody has seen. The caller then subtracts the return value from that
///     cursor — it is a byte offset into the buffer that just moved.
///   * **A UTF-8 character is never split.** The tokenizer can emit multi-byte tokens (U+2019 has
///     its own id), and a dangling continuation byte would silently blank the whole line it lands
///     on, since the renderer skips a line it cannot decode.
///
/// Bytes past `len` are untouched garbage from earlier chapters; only `..len` is meaningful, and
/// only `..len - dropped` is meaningful afterwards.
pub fn roll(text: &mut [u8], len: usize, keep: usize, revealed: usize) -> usize {
    let len = len.min(text.len());
    if len <= keep {
        return 0;
    }
    let mut cut = (len - keep).min(revealed);
    // 0b10xx_xxxx is a UTF-8 continuation byte: walk forward off the middle of a character.
    while cut < len && text[cut] & 0xC0 == 0x80 {
        cut += 1;
    }
    text.copy_within(cut..len, 0);
    cut
}

/// Append `extra` to `text[..len]`, ROLLING the oldest bytes out when it will not fit, and return
/// `(new_len, dropped)`.
///
/// This is the whole scrollback policy of an endless narrator (#302), in one pure function so the
/// host tests drive the real thing rather than a copy: the firmware's `push_text` is only the
/// `static mut` wrapper around it. `dropped` is what the caller must subtract from its reveal
/// cursor, since that cursor is a byte offset into a buffer that just moved.
///
/// A roll frees down to `keep` bytes (not to just-enough), so the memmove happens rarely instead of
/// once per token. If it cannot free enough — only possible when nothing has been revealed yet, see
/// [`roll`] — the append is TRUNCATED rather than allowed to overflow: dropping the tail of one
/// token beats a panic on a board that is otherwise fine.
pub fn append_rolling(
    text: &mut [u8],
    len: usize,
    extra: &[u8],
    keep: usize,
    revealed: usize,
) -> (usize, usize) {
    let len = len.min(text.len());
    let mut dropped = 0usize;
    let mut len = len;
    if len + extra.len() > text.len() {
        dropped = roll(text, len, keep, revealed);
        len -= dropped;
    }
    let n = extra.len().min(text.len() - len);
    text[len..len + n].copy_from_slice(&extra[..n]);
    (len + n, dropped)
}

/// Word-wrap `text` to `cols` columns and write the LAST `rows` line spans into `out`,
/// returning how many were written.
///
/// Each span is `(start, end)` byte offsets into `text`. Breaking rules, in order:
///   * `\n` is a hard break (the model emits them, and a story's paragraphs should survive).
///   * A line that would exceed `cols` breaks at the last space, which is CONSUMED — so the
///     next line starts with the word, not with a leading blank.
///   * A word longer than `cols` is hard-broken at the column edge rather than dropped.
///
/// Columns are counted in BYTES. That is exact for the ASCII these stories are made of, and a
/// multi-byte token would only mis-measure one line's width — never index outside `text`, and
/// never split a span the renderer can't decode (it skips a line it cannot read as UTF-8).
///
/// Wrapping runs FORWARD from the start of `text`, keeping the last `rows` spans in a small
/// rotating window. That matters for how it looks: break positions depend on where a line
/// begins, so wrapping the tail backwards would make already-visible words re-flow as the story
/// grows. Forward wrapping keeps every line stable once it appears — the text scrolls, it does
/// not shuffle. One pass over ≤1 KB per frame, no line buffer beyond `rows` spans.
pub fn wrap_tail(text: &[u8], cols: usize, rows: usize, out: &mut [(u16, u16)]) -> usize {
    if rows == 0 || out.is_empty() || cols == 0 {
        return 0;
    }
    let rows = rows.min(out.len());
    // Rotating window: line N lands in out[N % rows], so `out` only ever holds the newest rows.
    let mut total = 0usize;
    let mut emit = |start: usize, end: usize, total: &mut usize| {
        out[*total % rows] = (start as u16, end as u16);
        *total += 1;
    };

    let (mut line_start, mut col, mut last_space) = (0usize, 0usize, None::<usize>);
    let mut i = 0usize;
    while i < text.len() {
        if text[i] == b'\n' {
            emit(line_start, i, &mut total);
            i += 1;
            (line_start, col, last_space) = (i, 0, None);
            continue;
        }
        if col == cols {
            // The overflowing character is itself a space: the line fits EXACTLY, so break here
            // and swallow that space. (Without this, "aaa bbb" at 7 cols would backtrack to the
            // earlier space and emit "aaa" — every line one word narrower than it should be.)
            if text[i] == b' ' {
                emit(line_start, i, &mut total);
                line_start = i + 1;
                i = line_start;
                (col, last_space) = (0, None);
                continue;
            }
            match last_space {
                // Break at the space and swallow it.
                Some(sp) if sp > line_start => {
                    emit(line_start, sp, &mut total);
                    line_start = sp + 1;
                }
                // No space to break at: hard-break mid-word.
                _ => {
                    emit(line_start, i, &mut total);
                    line_start = i;
                }
            }
            i = line_start;
            (col, last_space) = (0, None);
            continue;
        }
        if text[i] == b' ' {
            last_space = Some(i);
        }
        col += 1;
        i += 1;
    }
    if line_start < text.len() {
        emit(line_start, text.len(), &mut total);
    }

    // Un-rotate IN PLACE: the window holds the newest `rows` lines starting at `total % rows`,
    // so one rotate puts them in reading order — no temporary, and no cap on `rows`.
    if total > rows {
        out[..rows].rotate_left(total % rows);
        return rows;
    }
    total
}
