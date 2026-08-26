use cast_core::{encode_dnrgb, rgb565_to_888, Mirror, RowMap, DNRGB, MAX_LEDS_PER_PKT};

#[test]
fn rowmap_covers_every_target_row_exactly_once() {
    let m = RowMap::new(502, 16);
    let mut hits = [0u32; 16];
    for sy in 0..502 {
        if let Some(ty) = m.target_row(sy) {
            hits[ty] += 1;
        }
    }
    assert!(hits.iter().all(|&h| h == 1), "{hits:?}");
}

#[test]
fn sample_span_lands_cell_centers() {
    let m = Mirror::new(4, 1).unwrap();
    let mut store = [0u16; 4];
    // panel width 8: cell centers at sx = 1, 3, 5, 7
    let span: [u16; 8] = [10, 11, 12, 13, 14, 15, 16, 17];
    m.sample_span(&mut store, 0, 8, 0, &span);
    assert_eq!(store, [11, 13, 15, 17]);
    // a PARTIAL span only updates the cells it covers
    let mut store2 = [0u16; 4];
    m.sample_span(&mut store2, 0, 8, 4, &span[4..6]); // pixels sx=4,5
    assert_eq!(store2, [0, 0, 15, 0], "only the sx=5 center was in the span");
}

#[test]
fn dnrgb_chunks_and_addresses() {
    let cells = [0xF800u16; 200]; // pure red, needs 2 packets
    let mut out = [0u8; 512];
    let (len1, n1) = encode_dnrgb(&cells, 0, 2, &mut out);
    assert_eq!(n1, MAX_LEDS_PER_PKT);
    assert_eq!(len1, 4 + n1 * 3);
    assert_eq!(&out[..4], &[DNRGB, 2, 0, 0]);
    let (r, g, b) = (out[4], out[5], out[6]);
    assert_eq!((r, g, b), (0xF8 | 0x07, 0, 0), "5-bit red expands with replication");
    let (len2, n2) = encode_dnrgb(&cells, n1, 2, &mut out);
    assert_eq!(n2, 200 - MAX_LEDS_PER_PKT);
    assert_eq!(&out[..4], &[DNRGB, 2, 0, MAX_LEDS_PER_PKT as u8]);
    assert!(len2 > 0);
    assert_eq!(encode_dnrgb(&cells, 200, 2, &mut out), (0, 0), "past-end is empty");
}

#[test]
fn rgb565_expansion_full_scale() {
    assert_eq!(rgb565_to_888(0xFFFF), (255, 255, 255));
    assert_eq!(rgb565_to_888(0x0000), (0, 0, 0));
}

#[test]
fn sample_span_with_closure_matches_slice() {
    let m = Mirror::new(4, 1).unwrap();
    let span: [u16; 8] = [10, 11, 12, 13, 14, 15, 16, 17];
    let mut a = [0u16; 4];
    let mut b = [0u16; 4];
    m.sample_span(&mut a, 0, 8, 0, &span);
    m.sample_span_with(&mut b, 0, 8, 0, span.len(), |i| span[i]);
    assert_eq!(a, b);
}
