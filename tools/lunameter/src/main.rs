// lunameter — renders the REAL WatchShell .slint tree through the REAL vendored
// Slint software renderer and prints, per screen, the per-frame scene-vector
// lengths that esp-alloc has to satisfy on the watch. See README.md for the cost
// model and for why these counts are the #75 story.
//
// Host-only. Run via tools/lunameter/measure.sh (which stages the instrumented
// renderer first). Never linked into the firmware.

use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use std::cell::Cell;
use std::rc::Rc;

slint::include_modules!();

const W: usize = 410;
const H: usize = 502;

struct HostPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: std::time::Instant,
}

impl Platform for HostPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> core::time::Duration {
        self.start.elapsed()
    }
}

struct Sink {
    buf: Vec<Rgb565Pixel>,
}

impl LineBufferProvider for &mut Sink {
    type TargetPixel = Rgb565Pixel;
    fn process_line(
        &mut self,
        _line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let base = _line * W;
        render_fn(&mut self.buf[base + range.start..base + range.end]);
    }
}

// Mirror of src/apps/registry.rs REGISTRY (name, icon_id, accent, section)
// and src/ui/slint_shell.rs::build_launcher_pages (page-major, 9 slots/page).
const REGISTRY: &[(&str, i32, u32, u8)] = &[
    ("Snake", 0, 0x35e0b0, 1),
    ("World Snake", 1, 0x00ff80, 1),
    ("2048", 2, 0xf0d000, 1),
    ("Tetris", 3, 0x00d0f0, 1),
    ("Flappy Bird", 4, 0xffffff, 1),
    ("Maze (Tilt)", 5, 0x8090ff, 1),
    ("Settings", 6, 0xc0ffc0, 2),
    ("WLED", 9, 0xffd166, 2),
    ("Hunt", 10, 0xff7a3d, 1),
    ("Energy", 11, 0x35e0b0, 2),
    ("Climate", 12, 0xff9d5c, 2),
    ("Voice", 7, 0xa78bfa, 0),
    ("Sound", 8, 0x4fd6ff, 0),
    ("Theme", 13, 0xa78bfa, 2),
    ("Lights", 14, 0xffb454, 2),
    ("Ping", 15, 0xffd166, 2),
];
const SECTIONS: [(u8, &str); 3] = [(0, "AUDIO"), (1, "GAMES"), (2, "SYSTEM")];

fn build_launcher_pages() -> (Vec<LauncherTile>, Vec<slint::SharedString>) {
    let mut tiles = Vec::new();
    let mut titles = Vec::new();
    for (sec, label) in SECTIONS {
        let apps: Vec<(usize, &(&str, i32, u32, u8))> =
            REGISTRY.iter().enumerate().filter(|(_, d)| d.3 == sec).collect();
        for chunk in apps.chunks(9) {
            titles.push(label.into());
            for (i, d) in chunk {
                tiles.push(LauncherTile {
                    name: d.0.into(),
                    accent: slint::Color::from_rgb_u8(
                        (d.2 >> 16) as u8,
                        (d.2 >> 8) as u8,
                        d.2 as u8,
                    ),
                    icon_id: d.1,
                    idx: *i as i32,
                    present: true,
                });
            }
            for _ in chunk.len()..9 {
                tiles.push(LauncherTile {
                    name: "".into(),
                    accent: slint::Color::from_rgb_u8(0, 0, 0),
                    icon_id: 0,
                    idx: 0,
                    present: false,
                });
            }
        }
    }
    (tiles, titles)
}

fn dump(tag: &str, buf: &[Rgb565Pixel]) {
    let dir = std::env::var("LUNAMETER_OUT").unwrap_or_else(|_| ".".into());
    let mut out = Vec::with_capacity(W * H * 3 + 32);
    out.extend_from_slice(format!("P6\n{W} {H}\n255\n").as_bytes());
    for px in buf {
        let v = px.0;
        let r = ((v >> 11) & 0x1f) as u32;
        let g = ((v >> 5) & 0x3f) as u32;
        let b = (v & 0x1f) as u32;
        // 5/6/5 -> 8/8/8 with bit replication (what the panel shows)
        out.push(((r << 3) | (r >> 2)) as u8);
        out.push(((g << 2) | (g >> 4)) as u8);
        out.push(((b << 3) | (b >> 2)) as u8);
    }
    std::fs::write(format!("{dir}/{tag}.ppm"), out).unwrap();
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(W as u32, H as u32));
    slint::platform::set_platform(Box::new(HostPlatform {
        window: window.clone(),
        start: std::time::Instant::now(),
    }))
    .unwrap();

    let ui = WatchShell::new().unwrap();

    // A realistic, populated watchface — the state the launcher opens over.
    ui.set_time_text("10:42".into());
    ui.set_seconds_text("07".into());
    ui.set_date_text("WED 29 JUL".into());
    ui.set_minute_progress(0.42);
    ui.set_steps(4820);
    ui.set_weather_text("21\u{b0}C CLEAR".into());
    ui.set_cpu_text("160 MHz".into());
    ui.set_battery_percent(78);
    ui.set_wifi_on(true);
    ui.set_mesh_peers(2);
    ui.set_fam_known(true);
    ui.set_fam_holding(true);
    ui.set_fam_stage(2);

    let (tiles, titles) = build_launcher_pages();
    let page_count = titles.len() as i32;
    ui.set_launcher_page_count(page_count);
    ui.set_launcher_titles(slint::ModelRc::from(Rc::new(slint::VecModel::from(titles))));
    ui.set_launcher_tiles(slint::ModelRc::from(Rc::new(slint::VecModel::from(tiles))));

    ui.show().unwrap();

    let mut sink = Sink { buf: vec![Rgb565Pixel(0); W * H] };

    let mut frame = |label: &str, sink: &mut Sink| {
        // Force a full-screen dirty region so every measurement is the
        // worst case (== what an overlay open/page flip actually costs).
        window.request_redraw();
        eprintln!("--- FRAME {label} ---");
        sink.buf.fill(Rgb565Pixel(0));
        window.draw_if_needed(|r| {
            r.render_by_line(&mut *sink);
        });
        dump(label, &sink.buf);
    };

    // 1. Watchface only, launcher closed.
    ui.set_launcher_open(false);
    frame("watchface(page0,closed)", &mut sink);

    // 2. Launcher open, each section page.
    ui.set_launcher_open(true);
    for p in 0..page_count {
        ui.set_launcher_page(p);
        frame(&format!("launcher(page{p})"), &mut sink);
    }
    ui.set_launcher_open(false);

    // 3. Re-show regression: closing the launcher must restore the watchface
    //    byte-for-byte (the `visible` gate is the only thing that moved).
    frame("watchface(reshow)", &mut sink);

    // 3b. The PAGER's own five pages. Only page 0 (clock) was ever framed, which
    //     left the SYSTEM page (2) unmeasured — the page documented as "EXACTLY
    //     full at seven 48 px rows (120 + 7*48 + 6*6 = 492 of 502 px)" and whose
    //     rows were restructured in 15c3bad to fit a BUILD row by merging the chip
    //     and panel rows. Tightest layout in the tree + rows just moved + never
    //     measured is the same combination that hid the story character page.
    // Setting `current-page` only sets the animation TARGET. Slint's animation
    // clock is a STORED GLOBAL TICK (`AnimationDriver::global_instant`), not the
    // platform clock, and it is advanced only by
    // `slint::platform::update_timers_and_animations()` — which this harness never
    // called. So every `animate` in the tree sat frozen at its start value, the
    // pager's `x` never moved, and all five pages measured a bit-identical 92/62:
    // page 0's numbers, five times.
    //
    // BOTH halves below are required and neither works alone. One tick call lands
    // at ~t0 (animation progress ~0). A sleep alone changes nothing, because
    // nothing reads the wall clock at render time — that asymmetry is exactly what
    // distinguishes "the clock is never advanced" from "we raced a 260 ms window".
    // Populate the rows FIRST. On the first run of this loop the pager frames
    // executed before section 4's value assignments, so six of the SYSTEM page's
    // seven rows rendered "--" and the page measured a lower bound — precisely the
    // placeholder-content trap that had just been found on the About rows, walked
    // straight into one section higher up. Placeholder content is not a
    // measurement; the gap between "--" and a real value is unbounded.
    ui.set_sigil_text("eldritch-lantern".into());
    ui.set_fw_text("v0.12.1 \u{b7} d7cdcee".into());
    ui.set_build_text("Smoldering Ironheart".into());
    ui.set_heap_text("73620 B free".into());
    ui.set_uptime_text("3d 14h 22m".into());
    ui.set_battery_text("78 % \u{b7} 4012 mV".into());
    let settle = || {
        slint::platform::update_timers_and_animations(); // start the animation at t0
        std::thread::sleep(std::time::Duration::from_millis(300)); // > the 260 ms
        slint::platform::update_timers_and_animations(); // tick to t0+300 -> done
    };
    for p in 0..5 {
        ui.set_current_page(p);
        settle();
        frame(&format!("pager(page{p})"), &mut sink);
    }
    ui.set_current_page(0);
    settle();

    // 4. Full Settings sweep — every page, every sub-view, worst-case content.
    ui.set_sigil_text("EMBER-7".into());
    // Populated, not the "--" default: the About rows are the screen 15c3bad
    // changed, and measuring them empty understates the new BUILD row by ~9
    // glyphs. Longest realm-sigil forge name is "Smoldering Ironheart" (20 ch).
    ui.set_fw_text("v0.12.1 \u{b7} d7cdcee".into());
    ui.set_build_text("Smoldering Ironheart".into());
    ui.set_node_id(7);
    ui.set_net_current("realm-roam-5g".into());
    ui.set_net_status(2);
    ui.set_ota_status("up to date".into());
    ui.set_volume_level(11);
    ui.set_mic_gain_db(0);
    ui.set_settings_open(true);
    for p in 0..6 {
        ui.set_settings_page(p);
        frame(&format!("settings(page{p})"), &mut sink);
    }
    // page 5 worst case: the longest ButtonAction label on all four slots
    // (config.rs: "Power menu" / "Read aloud" are the 10-char maxima).
    ui.set_settings_page(5);
    ui.set_boot_short_action("Power menu".into());
    ui.set_boot_long_action("Read aloud".into());
    ui.set_pwron_short_action("Power menu".into());
    ui.set_pwron_long_action("Read aloud".into());
    frame("settings(page5-worst)", &mut sink);
    // page 0 worst case: MUTED readout (5 glyphs, not 2) + max gain (+18 dB)
    ui.set_settings_page(0);
    ui.set_volume_muted(true);
    ui.set_mic_gain_db(18);
    frame("settings(page0-worst)", &mut sink);
    ui.set_volume_muted(false);
    ui.set_mic_gain_db(0);

    // NETWORK sub-views: scan picker, then the keyboard.
    let nets: Vec<WifiNet> = [
        ("realm-roam-5g", 4, true), ("realm-iot", 3, true), ("realm-guest", 3, false),
        ("Hein-Family", 2, true), ("XFINITY-2G4", 2, true), ("SpectrumSetup-9f", 1, true),
    ].iter().map(|(s,b,sec)| WifiNet { ssid: (*s).into(), bars: *b, secured: *sec }).collect();
    ui.set_wifi_nets(slint::ModelRc::from(Rc::new(slint::VecModel::from(nets))));
    ui.set_settings_page(3);
    ui.set_net_view(1);
    frame("settings(net-picker)", &mut sink);
    ui.set_kb_title("PASSWORD".into());
    ui.set_kb_context("realm-roam-5g".into());
    ui.set_kb_text("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}".into());
    ui.set_net_view(2);
    frame("settings(keyboard)", &mut sink);
    ui.set_net_view(0);
    ui.set_settings_open(false);

    // 5. Theme picker.
    ui.set_theme_open(true);
    frame("theme-overlay", &mut sink);
    ui.set_theme_open(false);

    // 6. The three surfaces `covered` deliberately skips, plus AOD. All four
    //    stack on top of the watchface's 92 items / 62 glyphs / 27 rounded.
    let cards: Vec<NotifCard> = [
        (3, "WiFi", "Joined realm-roam-5g", "2m"),
        (1, "Battery", "Charging \u{2014} 78 %", "11m"),
        (4, "Home Assistant", "Garage door left open", "34m"),
        (2, "Update", "v0.12.1 downloaded, tap to apply", "1h"),
    ].iter().map(|(src, t, b, a)| NotifCard {
        source: *src, title: (*t).into(), body: (*b).into(), age: (*a).into(), present: true,
    }).collect();
    ui.set_notif_cards(slint::ModelRc::from(Rc::new(slint::VecModel::from(cards))));
    ui.set_notif_total(7);
    ui.set_shade_open(true);
    frame("shade(4 cards)", &mut sink);
    ui.set_shade_open(false);

    let sw: Vec<LauncherTile> = [(0usize, "Snake"), (3, "Tetris"), (4, "Flappy Bird"), (5, "Maze (Tilt)")]
        .iter().map(|(i, n)| LauncherTile {
            name: (*n).into(),
            accent: slint::Color::from_rgb_u8(0x35, 0xe0, 0xb0),
            icon_id: REGISTRY[*i].1, idx: *i as i32, present: true,
        }).collect();
    ui.set_switcher_tiles(slint::ModelRc::from(Rc::new(slint::VecModel::from(sw))));
    ui.set_switcher_count(4);
    ui.set_switcher_open(true);
    frame("switcher(4 cards)", &mut sink);
    ui.set_switcher_open(false);

    ui.set_volume_overlay_open(true);
    frame("volume-hud", &mut sink);
    ui.set_volume_overlay_open(false);

    ui.set_aod(true);
    frame("aod", &mut sink);
    ui.set_aod(false);

    // 6b. STORY (#story). The character page was invisible to #75 because no
    //     scenario drove it — this is that scenario, and it found a real cliff.
    //
    //     Rust fills BOTH slot models from the fixed label arrays
    //     UNCONDITIONALLY (slint_shell.rs), so the row count is ALWAYS 11
    //     equipment + 6 appearance, even on an empty ledger. It never scales
    //     with how much gear the character has; only the VALUES change. So the
    //     variable that moves the rung is value LENGTH, not slot count:
    //
    //       empty / 4 ch / 6 ch  -> 159 / 211 / 245 glyphs -> cap 256 (7,168 B)
    //       8 ch and up          -> 279+          glyphs -> cap 512 (14,336 B)
    //
    //     6 characters leaves ELEVEN glyphs of margin. `MAX_SLOT_VAL` permits 24,
    //     and the daemon's own naming style runs 22-28 characters
    //     ("Shard of Divine Foundation"), so the first non-null equipment write
    //     doubles the textures rung. Today's ledger has no equip/appear rows at
    //     all, which is the only reason this page is currently free.
    const EQ: [&str; 11] = ["Head", "Amulet", "Chest", "Cloak", "Hands", "Legs", "Feet",
                            "Main hand", "Off hand", "Ring I", "Ring II"];
    const AP: [&str; 6] = ["Height", "Build", "Hair", "Eyes", "Skin", "Notable"];
    let fill = |labels: &[&str], val: Option<&str>| -> Vec<StorySlot> {
        labels.iter().map(|l| StorySlot {
            label: (*l).into(),
            value: val.unwrap_or("").into(),
            known: val.is_some(),
        }).collect()
    };
    // Chapter tiles. `VISIBLE_CHAPTERS = 5` is what the firmware can actually
    // produce — `slint_shell.rs:1471` does `.take(VISIBLE_CHAPTERS)` before
    // building the model — so FIVE is the reachable number and sixteen is a bound
    // the device cannot reach. Both are framed, labelled, because measuring only
    // the unreachable one produced a "worst screen in the entire UI" claim about a
    // screen the watch never draws.
    let mk_chapters = |n: i32| -> Vec<StoryChapter> {
        (1..=n).map(|i| StoryChapter {
            number: i,
            title: "Bones of the Sunken Cathedral Wing".into(),
            duration: "12:34".into(),
            playable: true,
            current: i == 3,
        }).collect()
    };
    ui.set_story_chapters(slint::ModelRc::from(Rc::new(slint::VecModel::from(mk_chapters(5)))));
    ui.set_story_more(9);
    ui.set_story_play_title("Bones of the Sunken Cathedral Wing".into());
    ui.set_story_speaker("Varkas Emberhand".into());
    ui.set_story_speaker_kind(1);
    ui.set_story_progress(0.42);
    ui.set_story_elapsed("5:12".into());
    ui.set_story_total("12:34".into());
    ui.set_story_seg_index(37);
    ui.set_story_seg_count(128);
    ui.set_story_playing(true);
    ui.set_story_subject("Thessaly of the Ninefold Ward".into());
    ui.set_story_level("14".into());
    ui.set_story_xp("18,420 / 24,000".into());
    ui.set_story_gold("2,317".into());
    ui.set_story_location("Sunken Cathedral".into());
    ui.set_story_status("bleeding, burdened, blessed by the drowned choir".into());
    ui.set_story_hp_text("62 / 140".into());
    ui.set_story_hp_frac(0.44);
    ui.set_story_hp_known(true);
    ui.set_story_open(true);
    for p in 0..4 {
        ui.set_story_page(p);
        frame(&format!("story(page{p})"), &mut sink);
    }
    // The unreachable bound, for contrast — and to check the repeater is in fact
    // bounded by the model rather than by anything in the .slint.
    ui.set_story_chapters(slint::ModelRc::from(Rc::new(slint::VecModel::from(mk_chapters(16)))));
    ui.set_story_page(0);
    frame("story(page0,ch16-UNREACHABLE)", &mut sink);
    // A toast lands over whatever screen is up and is deliberately NOT culled
    // (0685985), adding one rectangle plus one item per rendered glyph. The
    // chapter list is a screen you SIT on while choosing, and OTA/notification
    // toasts arrive asynchronously — so this is the realistic worst case for
    // page 0, not the 16-chapter one.
    ui.set_story_chapters(slint::ModelRc::from(Rc::new(slint::VecModel::from(mk_chapters(5)))));
    ui.set_toast_text("No WiFi credentials \u{2014} set in Settings".into());
    frame("story(page0,+toast31)", &mut sink);
    ui.set_toast_text("".into());
    ui.set_story_open(false);

    // POSITIVE CONTROLS for the toast. Until these existed, the only evidence the
    // toast rendered at all was that the scene-item count went up — which it does
    // whether or not a single pixel changes, and for a full release it did not: the
    // toast block sat above the overlay conditionals, so every full-screen overlay
    // overpainted it. `story(page0)` and `story(page0,+toast31)` were BYTE-IDENTICAL.
    //
    // The bare-watchface arm proves the toast draws. The over-settings arm proves it
    // survives the case it exists for — the WIFI-tap toast is emitted while
    // `settings-open` is true by construction, so an overlay that eats it makes the
    // control silently dead, which is the exact failure the toast was added to
    // prevent. Compare each against its no-toast neighbour with a PPM diff, not with
    // an item count.
    ui.set_toast_text("No WiFi credentials \u{2014} set in Settings".into());
    frame("watchface(+toast31)", &mut sink);
    ui.set_settings_open(true);
    ui.set_settings_page(2);
    frame("settings(page2,+toast31)", &mut sink);
    ui.set_settings_open(false);
    ui.set_toast_text("".into());
    ui.set_story_open(true);
    // The rung threshold. len06 is the LAST safe average value length; len08 is
    // the first that crosses. Keep both arms — a single sample cannot show a cliff.
    for (tag, val) in [
        ("empty", None),
        ("len04", Some("iron")),
        ("len06", Some("bronze")),
        ("len08", Some("oakstaff")),
        ("len24", Some("moonsilver greatsword +2")),
    ] {
        ui.set_story_equipment(slint::ModelRc::from(Rc::new(slint::VecModel::from(fill(&EQ, val)))));
        ui.set_story_appearance(slint::ModelRc::from(Rc::new(slint::VecModel::from(fill(&AP, val)))));
        ui.set_story_equipped_count(if val.is_some() { 11 } else { 0 });
        ui.set_story_appearance_count(if val.is_some() { 6 } else { 0 });
        ui.set_story_page(3);
        frame(&format!("story(page3,{tag})"), &mut sink);
    }
    ui.set_story_loading(true);
    ui.set_story_page(0);
    frame("story(page0-loading)", &mut sink);
    ui.set_story_loading(false);
    ui.set_story_open(false);

    // 7. INPUT PROBE — does `visible: false` cull hit-testing as well as draw?
    //    My 400a251 comment asserted it does; verify rather than assert. The
    //    WIFI RadioDot's hit area is (22,8)-(100,72); tap its centre.
    let hit = Rc::new(Cell::new(false));
    let h = hit.clone();
    ui.on_wifi_tap(move || h.set(true));
    let mut probe = |label: &str, ui: &WatchShell| {
        hit.set(false);
        let pos = slint::LogicalPosition::new(61.0, 40.0);
        window.dispatch_event(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerExited);
        let _ = ui;
        eprintln!("LUNAMETER probe {label}: wifi-tap fired = {}", hit.get());
    };
    ui.set_launcher_open(false);
    probe("chrome exposed (launcher closed)", &ui);
    ui.set_launcher_open(true);
    probe("chrome visible:false (launcher open)", &ui);
    ui.set_launcher_open(false);
}
