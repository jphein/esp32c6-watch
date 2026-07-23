// AXP2101 power management — read-mostly port from waveshare-watch-rs.
// We deliberately do NOT touch the DCDC/LDO rail configuration: the boot
// state Waveshare ships already powers the panel, and a wrong rail write
// can brown-out the board. Writes are limited to: the ADC-enable register
// (telemetry), the ALDO1 mic rail (read-modify-write enable bit only), and
// the charger profile regs 0x61-0x64 (issue #16, field-masked RMW).

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
