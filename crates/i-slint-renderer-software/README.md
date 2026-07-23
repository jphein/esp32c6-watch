# esp32c6-watch VENDORED FORK of i-slint-renderer-software 1.17.1

Vendored verbatim from crates.io `i-slint-renderer-software 1.17.1`
(checksum 73496f16…) with ONE local patch, applied via `[patch.crates-io]`
in the workspace root `Cargo.toml`:

- **Even-grid dirty-region alignment** (`align_dirty_region_to_even_grid` in
  `lib.rs`, applied in both the `render()` and `render_by_line()` paths):
  the CO5300 AMOLED controller requires CASET/RASET windows with even start
  and even extent on both axes (datasheet §7.5.21/§7.5.22). Slint 1.17 has no
  public dirty-region rounding hook (LVGL's "rounder" equivalent), so partial
  rendering through a 2-line strip buffer cannot otherwise be pixel-correct —
  see issue #18. Grep for `esp32c6-watch LOCAL PATCH` to find every divergence.

If upgrading Slint: re-vendor the new version and re-apply the patch, or drop
the vendor entirely if upstream gains a dirty-region alignment API.

---


**NOTE**: This library is an **internal** crate of the [Slint project](https://slint.dev).
This crate should **not be used directly** by applications using Slint.
You should use the `slint` crate instead.

**WARNING**: This crate does not follow the semver convention for versioning and can
only be used with `version = "=x.y.z"` in Cargo.toml.
