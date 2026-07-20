// The embedded-graphics watchface/pages/launcher/power_page modules are no
// longer referenced by main.rs (the Slint shell replaced them in task 9); they
// stay compiled until task 13 deletes them, so they emit dead-code warnings in
// the meantime. `pages` is still used for MESH_MAX_ROWS.
pub mod watchface;
pub mod pages;
pub mod launcher;
pub mod t9_keyboard;
pub mod power_page;
pub mod slint_platform;
// slint_shell is fully wired into main.rs now, except `set_toast` and
// `touch_is_down`, which are part of the shell API for later tasks (toasts,
// gesture polish) — keep the module attribute until they get their first caller.
#[allow(dead_code)]
pub mod slint_shell;
