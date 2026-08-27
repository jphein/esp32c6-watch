// AXP2101 power management — read-mostly port from waveshare-watch-rs.
// We deliberately do NOT touch the DCDC/LDO rail configuration: the boot
// state Waveshare ships already powers the panel, and a wrong rail write
// can brown-out the board. Writes are limited to: the ADC-enable register
// (telemetry), the ALDO1 mic rail (read-modify-write enable bit only),
// the charger profile regs 0x61-0x64 (issue #16, field-masked RMW), and
// the PWRON key-event plumbing (#48): 0x27 IRQLEVEL field + 0x41 IRQ
// enables (both field-masked RMW), 0x49 status write-1-to-clear, and the
// 0x10 poweroff bit — user-invoked SHUTDOWN only. The vendor's hardware
// failsafe config (0x22 poweroff-source, 0x27 OFFLEVEL=4s) is never
// written; note it is residual vendor state, like the rails: a battery-
// dead PMIC cold boot would revert 0x22 to chip defaults.

use embedded_hal::i2c::I2c;

const AXP2101_ADDR: u8 = 0x34;

const REG_STATUS1: u8 = 0x00;
const REG_IC_TYPE: u8 = 0x03;
const REG_ADC_ENABLE: u8 = 0x30;
const REG_VBAT_H: u8 = 0x34;
const REG_VBAT_L: u8 = 0x35;
const REG_VBUS_H: u8 = 0x38;
const REG_VBUS_L: u8 = 0x39;
const REG_BAT_PERCENT: u8 = 0xA4;
const REG_CHG_STATUS: u8 = 0x01;

// === PWRON power-key events (#48) ===
// The side button is wired to the PMIC's PWRON input, NOT a SoC GPIO, and the
// PMIC INT line is not routed to the C6 either (vendor config.h has no IRQ
// pin) — so the ONLY way the firmware sees the button is by polling the
// AXP2101's latched IRQ status over I2C.
//
// Register map (verified against XPowersLib master src/REG/AXP2101Constants.h
// + src/XPowersParams.hpp `xpowers_axp2101_irq_t`):
//   0x41 INTEN2  — IRQ enable bank 2
//   0x49 INTSTS2 — IRQ status bank 2, WRITE-1-TO-CLEAR (XPowersLib
//                  clearIrqStatus() writes 0xFF to 0x48..0x4A to clear)
//   IRQ2 bits: 0 ponpe (PWRON positive edge) · 1 ponne (negative edge) ·
//              2 ponlp (LONG press, fires at the 0x27 IRQLEVEL time) ·
//              3 ponsp (SHORT press) · 4 bremove · 5 binsert · 6 vremove ·
//              7 vinsert
//   0x27 IRQ_OFF_ON_LEVEL_CTRL — [5:4] IRQLEVEL (0:1s 1:1.5s 2:2s 3:2.5s),
//        [3:2] OFFLEVEL (0:4s..3:10s hardware poweroff), [1:0] ONLEVEL
//        (power-on press time). The vendor writes 0x27=0x10 ("hold 4s to
//        power off", board .cc:30) — which is IRQLEVEL=1.5s + OFFLEVEL=4s.
const REG_INTEN2: u8 = 0x41;
const REG_INTSTS2: u8 = 0x49;
const REG_IRQ_OFF_ON_LEVEL: u8 = 0x27;
const REG_COMMON_CONFIG: u8 = 0x10;

const PKEY_LONG_BIT: u8 = 1 << 2; // ponlp — XPOWERS_AXP2101_PKEY_LONG_IRQ = _BV(10)
const PKEY_SHORT_BIT: u8 = 1 << 3; // ponsp — XPOWERS_AXP2101_PKEY_SHORT_IRQ = _BV(11)

/// A latched PWRON key event, drained by [`Axp2101Power::poll_power_key`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerKey {
    /// Pressed + released before the IRQLEVEL time (1.5s).
    Short,
    /// Held past the IRQLEVEL time (1.5s) — latches while the key is still
    /// down, so the firmware can react before the 4s hardware OFFLEVEL cutoff.
    Long,
}

pub struct Axp2101Power<I> {
    i2c: I,
}

impl<I: I2c> Axp2101Power<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8];
        self.i2c.write_read(AXP2101_ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    /// Enable VBAT/TS/VBUS/VSYS ADC channels so voltage reads work.
    pub fn enable_adc(&mut self) -> Result<(), I::Error> {
        self.i2c.write(AXP2101_ADDR, &[REG_ADC_ENABLE, 0b0001_1101])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(AXP2101_ADDR, &[reg, val])
    }

    /// Power the microphone rail: AXP2101 **ALDO1 @ 3.3V** (regs 0x92 voltage, 0x90
    /// enable bit0). The vendor board file powers the mics from ALDO1; our firmware
    /// otherwise never enables any LDO, so the ES7210 mic bias has been riding on
    /// *residual* rail state left on by a prior vendor flash (the PMIC keeps rail
    /// state across SoC resets) — a battery-dead cold boot would leave it off and the
    /// mic would go silent again. Read-modify-write the enable reg so we do NOT
    /// disturb the display/touch rails also controlled there. Idempotent; call once
    /// at boot before the ES7210 init.
    pub fn enable_mic_rail(&mut self) -> Result<(), I::Error> {
        self.write_reg(0x92, 0x1C)?; // ALDO1 = 3.3V : (3300-500)/100 = 28 = 0x1C
        let en = self.read_reg(0x90)?;
        self.write_reg(0x90, en | 0x01) // set ALDO1 enable, preserve other rails
    }

    /// Configure the battery charger to the vendor's profile (issue #16), ported
    /// from the vendor board file `esp32-c6-touch-amoled-2.06.cc` Pmic ctor:
    /// CV 4.10V, precharge 50mA, fast-charge 400mA, termination 25mA.
    ///
    /// Read-modify-write each register, masking in ONLY the documented field so
    /// reserved/adjacent bits keep their reset values. Charger regs 0x61-0x64
    /// exclusively — the vendor's surrounding DC/LDO rail block (`.cc:32-46`) is
    /// deliberately NOT ported (see module header + `enable_mic_rail`: a wrong
    /// rail write can brown out the panel). Idempotent; call once at boot.
    pub fn configure_charger(&mut self) -> Result<(), I::Error> {
        // 0x64 CHG_V_CFG, CV[2:0]: 0b010 = 4.10V (vendor `.cc:48`)
        let v = self.read_reg(0x64)?;
        self.write_reg(0x64, (v & !0x07) | 0x02)?;
        // 0x61 IPRECHG[3:0]: 25mA/step -> 0x02 = 50mA (vendor `.cc:50`)
        let v = self.read_reg(0x61)?;
        self.write_reg(0x61, (v & !0x0F) | 0x02)?;
        // 0x62 ICC[4:0]: n<=8 -> n*25mA, n>8 -> 200+(n-8)*100mA -> 0x0A = 400mA
        // (vendor `.cc:51`: "0x08-200mA, 0x09-300mA, 0x0A-400mA")
        let v = self.read_reg(0x62)?;
        self.write_reg(0x62, (v & !0x1F) | 0x0A)?;
        // 0x63 ITERM[3:0]: 25mA/step -> 0x01 = 25mA (vendor `.cc:52`)
        let v = self.read_reg(0x63)?;
        self.write_reg(0x63, (v & !0x0F) | 0x01)
    }

    /// Arm the PWRON short/long-press event latches (#48). Call once at boot.
    ///
    /// 1. Pins the long-press IRQ threshold: field-masked RMW of 0x27 bits
    ///    [5:4] to 01 = **1.5s** (XPowersLib `setIrqLevelTime`: `val & 0xCF |
    ///    opt<<4`). This is the SAME value the vendor's wholesale 0x27=0x10
    ///    write sets, but ours survives a battery-dead PMIC cold boot (same
    ///    residual-state class as the mic rail). Bits [3:2] OFFLEVEL — the 4s
    ///    hardware-poweroff failsafe — and [1:0] ONLEVEL are NOT touched, and
    ///    reg 0x22 (poweroff-source enables) is not written at all.
    /// 2. Enables the short+long press IRQs in INTEN2 (RMW, other banks/bits
    ///    untouched). The INT pin goes nowhere on this board, so the enables
    ///    only serve to arm the status latches for polling — XPowersLib's
    ///    `isPekeyShortPressIrq` likewise requires the enable bit.
    /// 3. Clears any stale latched PWRON events (write-1-to-clear) so a press
    ///    from before this boot can't ghost a menu.
    pub fn enable_pwron_events(&mut self) -> Result<(), I::Error> {
        let v = self.read_reg(REG_IRQ_OFF_ON_LEVEL)?;
        self.write_reg(REG_IRQ_OFF_ON_LEVEL, (v & 0xCF) | 0x10)?;
        let v = self.read_reg(REG_INTEN2)?;
        self.write_reg(REG_INTEN2, v | PKEY_LONG_BIT | PKEY_SHORT_BIT)?;
        self.write_reg(REG_INTSTS2, PKEY_LONG_BIT | PKEY_SHORT_BIT)
    }

    /// Drain the latched PWRON key event, if any (#48). Reads INTSTS2 and
    /// write-1-clears ONLY the PWRON bits it consumed (other latched events in
    /// the bank stay latched for any future consumer). If both short and long
    /// latched inside one poll window, Long wins — it is the destructive-
    /// intent signal and the menu subsumes the short-press wake.
    pub fn poll_power_key(&mut self) -> Result<Option<PowerKey>, I::Error> {
        let sts = self.read_reg(REG_INTSTS2)?;
        let hit = sts & (PKEY_LONG_BIT | PKEY_SHORT_BIT);
        if hit == 0 {
            return Ok(None);
        }
        self.write_reg(REG_INTSTS2, hit)?;
        Ok(Some(if hit & PKEY_LONG_BIT != 0 {
            PowerKey::Long
        } else {
            PowerKey::Short
        }))
    }

    /// AXP2101 software poweroff (#48): COMMON_CONFIG (0x10) bit 0 — byte-for-
    /// byte the vendor firmware's `Axp2101::PowerOff()` (`ReadReg(0x10)|0x01`,
    /// axp2101.cc:37-41) and XPowersLib's `shutdown()` (`setRegisterBit(
    /// COMMON_CONFIG, 0)`). On battery this cuts every rail until PWRON is
    /// pressed again (ONLEVEL). On USB the PMIC re-powers immediately, so it
    /// behaves like a reboot — the power menu says so in its caption.
    pub fn shutdown(&mut self) -> Result<(), I::Error> {
        let v = self.read_reg(REG_COMMON_CONFIG)?;
        self.write_reg(REG_COMMON_CONFIG, v | 0x01)
    }

    pub fn read_chip_id(&mut self) -> Result<u8, I::Error> {
        self.read_reg(REG_IC_TYPE)
    }

    /// STATUS1 bit 3: a battery is physically connected.
    pub fn battery_present(&mut self) -> Result<bool, I::Error> {
        Ok(self.read_reg(REG_STATUS1)? & 0x08 != 0)
    }

    /// STATUS1 bit 5: USB (VBUS) power present.
    pub fn is_vbus_in(&mut self) -> Result<bool, I::Error> {
        Ok(self.read_reg(REG_STATUS1)? & 0x20 != 0)
    }

    /// Battery voltage in millivolts (14-bit ADC).
    pub fn get_battery_voltage(&mut self) -> Result<u16, I::Error> {
        let high = self.read_reg(REG_VBAT_H)? as u16;
        let low = self.read_reg(REG_VBAT_L)? as u16;
        Ok(((high << 8) | low) & 0x3FFF)
    }

    /// VBUS voltage in millivolts.
    pub fn get_vbus_voltage(&mut self) -> Result<u16, I::Error> {
        let high = self.read_reg(REG_VBUS_H)? as u16;
        let low = self.read_reg(REG_VBUS_L)? as u16;
        Ok(((high << 8) | low) & 0x3FFF)
    }

    /// Fuel-gauge battery percentage (0-100).
    pub fn get_battery_percent(&mut self) -> Result<u8, I::Error> {
        self.read_reg(REG_BAT_PERCENT)
    }

    /// Charger state from STATUS2 bits [7:5] (001/010/011 = charging).
    pub fn is_charging(&mut self) -> Result<bool, I::Error> {
        let status = self.read_reg(REG_CHG_STATUS)?;
        let chg = (status >> 5) & 0x07;
        Ok((1..=3).contains(&chg))
    }
}

/// Approximate 1S-LiPo charge percentage from cell voltage (mV).
///
/// For boards with no fuel-gauge IC (the S3-CYD reads battery voltage off a
/// divider ADC — see `board::HAS_BATT_ADC`), not the AXP2101's gauge. v1 is a
/// piecewise-linear fit to the standard 1S discharge knee; the flat 3.7-3.9 V
/// plateau makes any voltage→% mapping coarse, so refine the knee points on the
/// bench against a known charge state rather than trusting these to a percent.
/// Saturates to 0..=100 and is monotonic. Pure — unit-tested, no hardware.
pub fn lipo_pct(mv: u16) -> u8 {
    // (mV, %) knees, ascending. Below the first → 0, above the last → 100.
    const KNEES: [(u16, u8); 7] = [
        (3300, 0),
        (3500, 15),
        (3650, 30),
        (3750, 45),
        (3850, 60),
        (4000, 85),
        (4200, 100),
    ];
    if mv <= KNEES[0].0 {
        return 0;
    }
    if mv >= KNEES[KNEES.len() - 1].0 {
        return 100;
    }
    let mut i = 0;
    while i + 1 < KNEES.len() && mv > KNEES[i + 1].0 {
        i += 1;
    }
    let (v0, p0) = KNEES[i];
    let (v1, p1) = KNEES[i + 1];
    // Linear interpolate within [v0, v1]. u32 math; range keeps it well clear
    // of overflow (max ~100*900).
    (p0 as u32 + (p1 as u32 - p0 as u32) * (mv - v0) as u32 / (v1 - v0) as u32) as u8
}
