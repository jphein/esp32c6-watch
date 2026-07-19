// AXP2101 power management — read-only port from waveshare-watch-rs.
// We deliberately do NOT touch the DCDC/LDO rail configuration: the boot
// state Waveshare ships already powers the panel, and a wrong rail write
// can brown-out the board. Only the ADC-enable register is written so
// battery telemetry reads real values.

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
