// The Slint shell (slint_platform + slint_shell) owns the whole watchface,
// pages and launcher UI now; the old embedded-graphics modules (watchface,
// pages, launcher, power_page) were deleted in task 13. t9_keyboard stays —
// apps/settings.rs uses it for on-screen text entry.
pub mod t9_keyboard;
pub mod slint_platform;
pub mod slint_shell;
