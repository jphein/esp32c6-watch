//! ES7210 4-channel audio ADC — the MICROPHONE codec on the Waveshare
//! ESP32-C6-Touch-AMOLED-2.06.
//!
//! ROOT CAUSE of "mic reads exact zeros": the board's mics (MIC1_P/N, MIC2_P/N)
//! are wired to THIS chip (U8), whose SDOUT1 drives I2S_ASDOUT → GPIO21 (our RX
//! DIN) through R47. The ES8311 is the speaker DAC only — its ADC is NOT wired to
//! the SoC. So the ES7210 MUST be I2C-initialised or its serial output stays idle
//! and the SoC RX shifts in silence forever.
//!
//! Register sequence ported verbatim from esp-adf `esp_codec_dev` es7210.c /
//! es7210_reg.h (`es7210_open` + `es7210_start` + `es7210_adc_set_gain`).
//! Config: I2S SLAVE (the SoC drives MCLK/BCLK/WS via the full-duplex master),
//! standard 16-bit I2S stereo (MIC1 = LEFT slot, MIC2 = RIGHT slot — NOT TDM;
//! TDM only engages at ≥3 mics), mic bias 2.87 V, +30 dB PGA. MCLK = 256·fs =
//! 4.096 MHz @ 16 kHz, supplied by the SoC. In slave mode the ES7210 auto-derives
//! its rate from the incoming BCLK/WS, so the coeff/divider table (reg 0x04/0x05)
//! is deliberately NOT programmed.

use embedded_hal::i2c::I2c;

/// ES7210 7-bit I2C address (strap AD1=AD0=0). The datasheet/header 0x80 is the
/// 8-bit form; embedded-hal takes the 7-bit address.
pub const ES7210_ADDR: u8 = 0x40;

pub struct Es7210<I> {
    i2c: I,
}

impl<I: I2c> Es7210<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    fn w(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(ES7210_ADDR, &[reg, val])
    }

    /// Read a register (verify/debug).
    pub fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut b = [0u8];
        self.i2c.write_read(ES7210_ADDR, &[reg], &mut b)?;
        Ok(b[0])
    }

    /// Full record init: mics MIC1+MIC2, 16-bit standard-I2S stereo, SLAVE, +30 dB.
    /// Blind ordered writes (esp-adf es7210_open → es7210_start → gain reassert).
    /// Any single I2C error aborts and is returned.
    pub fn init(&mut self) -> Result<(), I::Error> {
        // (A) reset + base config -------------------------------------------------
        self.w(0x00, 0xFF)?; // reset
        self.w(0x00, 0x41)?; // reset release
        self.w(0x01, 0x3F)?; // clock off during config
        self.w(0x09, 0x30)?; // time control 0
        self.w(0x0A, 0x30)?; // time control 1
        self.w(0x23, 0x2A)?; // ADC12 HPF2
        self.w(0x22, 0x0A)?; // ADC12 HPF1
        self.w(0x20, 0x0A)?; // ADC34 HPF1
        self.w(0x21, 0x2A)?; // ADC34 HPF2
        self.w(0x08, 0x10)?; // MODE: SLAVE (bit0=0) — SoC is the I2S master
        self.w(0x40, 0x43)?; // analog power: VDDA 3.3, VMID 5k
        self.w(0x41, 0x70)?; // MIC12 bias = 2.87 V
        self.w(0x42, 0x70)?; // MIC34 bias = 2.87 V
        self.w(0x07, 0x20)?; // OSR
        self.w(0x02, 0xC1)?; // mainclk / DLL
        self.w(0x4B, 0xFF)?; // transient power-down (MIC12)
        self.w(0x4C, 0xFF)?; // transient power-down (MIC34)
        self.w(0x01, 0x34)?; // off_reg
        self.w(0x4B, 0x00)?; // power up MIC1 & MIC2
        self.w(0x43, 0x10)?; // MIC1 enable (bit4)
        self.w(0x44, 0x10)?; // MIC2 enable (bit4)
        self.w(0x12, 0x00)?; // non-TDM (2-mic stereo)
        self.w(0x43, 0x1D)?; // MIC1 +36 dB (enable | gain 0x0D)
        self.w(0x44, 0x1D)?; // MIC2 +36 dB
        // (B) serial format: 16-bit standard I2S (MIC1=Left, MIC2=Right) ----------
        self.w(0x11, 0x60)?;
        // (C) start / enable ------------------------------------------------------
        self.w(0x01, 0x34)?; // clocks on (record path)
        self.w(0x06, 0x00)?; // power-down reg: all up
        self.w(0x40, 0x43)?; // analog reconfirm
        self.w(0x47, 0x08)?; // MIC1 power on
        self.w(0x48, 0x08)?; // MIC2 power on
        self.w(0x49, 0x08)?; // MIC3 power on
        self.w(0x4A, 0x08)?; // MIC4 power on
        self.w(0x4B, 0xFF)?; // transient pd
        self.w(0x4C, 0xFF)?;
        self.w(0x01, 0x34)?;
        self.w(0x4B, 0x00)?; // power up MIC1 & MIC2
        self.w(0x43, 0x10)?; // MIC1 enable (gain nibble RE-ZEROED here — see (D))
        self.w(0x44, 0x10)?; // MIC2 enable
        self.w(0x12, 0x00)?; // non-TDM
        self.w(0x40, 0x43)?; // analog reconfirm
        self.w(0x00, 0x71)?; // digital reset pulse
        self.w(0x00, 0x41)?; // reset release
        // (D) REASSERT GAIN LAST (critical footgun) -------------------------------
        // es7210_start's mic_select above re-zeros the gain nibble; the ADC reads
        // near-silent unless the gain is written AFTER all enable writes. Last word.
        self.w(0x43, 0x1D)?; // MIC1 +36 dB (final)
        self.w(0x44, 0x1D)?; // MIC2 +36 dB (final)
        Ok(())
    }
}
