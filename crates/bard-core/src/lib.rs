//! bard's INFERENCE core — smol #300's on-device storyteller, the pure half,
//! vendored VERBATIM from `jphein/smol` @ 58b0dfa
//! (`rust/clock/src/bard/{nano_llm,tokenizer,persona,textflow}.rs` + the
//! 277 KB `stories260K-q8.bin` SBRD model). smol stays authoritative — the
//! mesh-flood vendoring rule.
//!
//! What this is: a REAL (tiny) llama-style transformer — int8 weights with
//! f32 group scales, RMSNorm, the tok512 tokenizer — that opens its
//! flash-resident model as BORROWED views (277 KB of flash, ~0 RAM to open)
//! and generates persona-flavored story text. Golden-reference tests pin the
//! exact token stream, so the port cannot drift silently.
//!
//! NOT here (the watch app half, its own design pass): smol's 920-line OLED
//! story screen + delivery cadence. The watch edition wants a Slint page
//! riding the registry (#44's plugin shape) and can pair the generation with
//! the watch's own TTS — the bard SPEAKS on this hardware.
#![no_std]
#![forbid(unsafe_code)]

pub mod delivery;
pub mod nano_llm;
pub mod persona;
pub mod textflow;
pub mod tokenizer;
