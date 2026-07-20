//! Radio-clock bring-up for the vendored 802.15.4 driver.
//!
//! Vendored from esp-radio 0.18 `radio_clocks/clocks_ll/esp32c6.rs` (+ the
//! `radio_clocks/mod.rs::init_radio_clocks` wrapper, which is just
//! `clocks_ll::init_clocks()`). Only the two helpers `raw.rs` needs are copied:
//! `init_radio_clocks` (ICG / modem-clock bring-up) and `enable_ieee802154`
//! (ZB MAC + FE/BB clock gating). esp-radio reaches these registers via its
//! private `regs!(P)` macro = `unsafe { &*esp32c6::P::ptr() }`; we do the same
//! against the `esp32c6` PAC (0.23.2 — the exact version esp-radio 0.18 itself
//! compiles against, so every field name below is guaranteed to exist).

use esp32c6::{MODEM_LPCON, MODEM_SYSCON, PMU};

/// esp-radio `init_radio_clocks()` → `clocks_ll::init_clocks()`.
pub(super) fn init_radio_clocks() {
    let pmu = unsafe { &*PMU::ptr() };
    let syscon = unsafe { &*MODEM_SYSCON::ptr() };
    let lpcon = unsafe { &*MODEM_LPCON::ptr() };
    unsafe {
        pmu.hp_sleep_icg_modem()
            .modify(|_, w| w.hp_sleep_dig_icg_modem_code().bits(0));
        pmu.hp_modem_icg_modem()
            .modify(|_, w| w.hp_modem_dig_icg_modem_code().bits(1));
        pmu.hp_active_icg_modem()
            .modify(|_, w| w.hp_active_dig_icg_modem_code().bits(2));
        pmu.imm_modem_icg()
            .write(|w| w.update_dig_icg_modem_en().set_bit());
        pmu.imm_sleep_sysclk()
            .write(|w| w.update_dig_icg_switch().set_bit());

        syscon.clk_conf_power_st().modify(|_, w| {
            w.clk_modem_apb_st_map().bits(6);
            w.clk_modem_peri_st_map().bits(4);
            w.clk_wifi_st_map().bits(6);
            w.clk_bt_st_map().bits(6);
            w.clk_fe_st_map().bits(6);
            w.clk_zb_st_map().bits(6)
        });

        lpcon.clk_conf_power_st().modify(|_, w| {
            w.clk_lp_apb_st_map().bits(6);
            w.clk_i2c_mst_st_map().bits(6);
            w.clk_coex_st_map().bits(6);
            w.clk_wifipwr_st_map().bits(6)
        });

        lpcon.wifi_lp_clk_conf().modify(|_, w| {
            w.clk_wifipwr_lp_sel_osc_slow().set_bit();
            w.clk_wifipwr_lp_sel_osc_fast().set_bit();
            w.clk_wifipwr_lp_sel_xtal32k().set_bit();
            w.clk_wifipwr_lp_sel_xtal().set_bit()
        });

        lpcon
            .wifi_lp_clk_conf()
            .modify(|_, w| w.clk_wifipwr_lp_div_num().bits(0));

        lpcon
            .clk_conf()
            .modify(|_, w| w.clk_wifipwr_en().set_bit());
    }
}

/// esp-radio `clocks_ll::enable_ieee802154` — gate the ZB MAC + FE/BB clocks.
pub(super) fn enable_ieee802154(en: bool) {
    let syscon = unsafe { &*MODEM_SYSCON::ptr() };
    let lpcon = unsafe { &*MODEM_LPCON::ptr() };

    syscon.clk_conf().modify(|_, w| {
        w.clk_zb_apb_en().bit(en);
        w.clk_zb_mac_en().bit(en)
    });

    syscon.clk_conf1().modify(|_, w| {
        w.clk_fe_apb_en().bit(en);
        w.clk_fe_cal_160m_en().bit(en);
        w.clk_fe_160m_en().bit(en);
        w.clk_fe_80m_en().bit(en);
        w.clk_bt_apb_en().bit(en);
        w.clk_bt_en().bit(en);
        w.clk_wifibb_160x1_en().bit(en);
        w.clk_wifibb_80x1_en().bit(en);
        w.clk_wifibb_40x1_en().bit(en);
        w.clk_wifibb_80x_en().bit(en);
        w.clk_wifibb_40x_en().bit(en);
        w.clk_wifibb_80m_en().bit(en);
        w.clk_wifibb_44m_en().bit(en);
        w.clk_wifibb_40m_en().bit(en);
        w.clk_wifibb_22m_en().bit(en)
    });

    lpcon.clk_conf().modify(|_, w| w.clk_coex_en().set_bit());
}
