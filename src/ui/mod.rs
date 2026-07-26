// The Slint shell (slint_platform + slint_shell) owns the whole watchface,
// pages and launcher UI now; the old embedded-graphics modules (watchface,
// pages, launcher, power_page) were deleted in task 13, and t9_keyboard +
// apps/settings.rs in v0.9.0 (the scene-resident Settings hub + Slint
// keyboard replaced them — see ui/slint/settings.slint / keyboard.slint).
pub mod slint_platform;
pub mod slint_shell;
