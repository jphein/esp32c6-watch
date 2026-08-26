//! On-device storyteller (feature `bard`) — runs the SBRD int8 transformer
//! from `bard-core` against its flash-resident model and streams the story
//! text. The pure engine is host-verified (40 golden cases pin the exact
//! token stream); this module is the thin on-device driver.
//!
//! v1 surface is a console generator (`bard [prompt]`): it proves the engine
//! FITS and RUNS in the watch's heap/latency budget on real silicon — the one
//! thing host tests cannot show. The on-glass Slint story page + the TTS pipe
//! (the payoff: the watch SPEAKS its stories) is the next slice, app-tier UI
//! that wants glass to design against.
//!
//! Cost: `Box<Bufs>` (~a few KB) + the Session KV cache on the heap, freed
//! when generation ends. The model opens as borrowed views (~0 RAM). Blocking
//! by design — a console command owns the loop for its ~1-2 s; the real app
//! will step incrementally off the render clock.

use alloc::boxed::Box;
use bard_core::nano_llm::{Bufs, Model, StepOut, Story};
use bard_core::tokenizer::Tokenizer;
use esp_println::println;

/// Generate a short story and print it over serial. `seed` varies the output;
/// `max_tokens` bounds the run. Returns false if the model blob won't parse
/// (a corrupt vendor — should never happen, the host CRC test guards it).
pub fn generate(prompt: &str, seed: u32, max_tokens: usize) -> bool {
    let Ok(model) = Model::parse(bard_core::MODEL) else {
        println!("[BARD] model parse failed");
        return false;
    };
    let Some(tok) = Tokenizer::new(model.tok_table, model.cfg.vocab) else {
        println!("[BARD] tokenizer init failed");
        return false;
    };
    let mut bufs = Box::new(Bufs::INIT);
    let mut story = Story::new(&tok, prompt, seed);
    println!("[BARD] \"{prompt}\" ->");
    let mut printed = 0usize;
    for _ in 0..max_tokens {
        match story.step(&model, &tok, &mut bufs) {
            StepOut::Working => {}
            StepOut::Text(bytes) => {
                // Serial is line-oriented; print the decoded fragment raw.
                if let Ok(frag) = core::str::from_utf8(bytes) {
                    esp_println::print!("{frag}");
                }
                printed += 1;
            }
            StepOut::Done { truncated } => {
                println!("\n[BARD] done ({printed} fragments{})",
                    if truncated { ", truncated" } else { "" });
                return true;
            }
        }
    }
    println!("\n[BARD] stopped at {max_tokens}-token cap");
    true
}
