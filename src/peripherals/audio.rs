// ES8311 Audio codec - proper init from Waveshare C reference
// + I2S DMA playback via public write_dma()

use embedded_hal::i2c::I2c;

const ES8311_ADDR: u8 = 0x18;

pub struct Es8311<I> {
    i2c: I,
    initialized: bool,
}

impl<I: I2c> Es8311<I> {
    pub fn new(i2c: I) -> Self { Self { i2c, initialized: false } }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(ES8311_ADDR, &[reg, val])
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8];
        self.i2c.write_read(ES8311_ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    /// Initialize ES8311 for 16kHz 16-bit I2S playback.
    /// Exactly mirrors the C driver es8311_init() from Waveshare examples.
    pub fn init(&mut self) -> Result<(), I::Error> {
        // Reset sequence (CRITICAL: must write 0x80 after reset!)
        self.write_reg(0x00, 0x1F)?; // Reset
        self.write_reg(0x00, 0x00)?; // Clear reset
        self.write_reg(0x00, 0x80)?; // Power-on command

        // Clock config for MCLK from pin, 16kHz sample rate
        // MCLK = 16000 * 256 = 4,096,000 Hz
        // Coefficients from table: {4096000, 16000, pre_div=2, pre_multi=0,
        //   adc_div=1, dac_div=1, fs_mode=0, lrck_h=0, lrck_l=0xFF, bclk_div=4, adc_osr=0x10, dac_osr=0x10}
        self.write_reg(0x01, 0x3F)?; // Enable all clocks, MCLK from pin

        // Reg 0x02: pre_div and pre_multi
        let mut reg02 = self.read_reg(0x02).unwrap_or(0) & 0x07;
        reg02 |= (2 - 1) << 5; // pre_div = 2
        reg02 |= 0 << 3;       // pre_multi = 0 (1x)
        self.write_reg(0x02, reg02)?;

        // Reg 0x03: fs_mode | adc_osr
        self.write_reg(0x03, (0 << 6) | 0x10)?; // fs_mode=0, adc_osr=0x10

        // Reg 0x04: dac_osr
        self.write_reg(0x04, 0x10)?;

        // Reg 0x05: adc_div | dac_div
        self.write_reg(0x05, ((1 - 1) << 4) | (1 - 1))?;

        // Reg 0x06: BCLK divider
        let mut reg06 = self.read_reg(0x06).unwrap_or(0) & 0xE0;
        reg06 |= (4 - 1) & 0x1F; // bclk_div = 4
        self.write_reg(0x06, reg06)?;

        // Reg 0x07: LRCK high
        let mut reg07 = self.read_reg(0x07).unwrap_or(0) & 0xC0;
        reg07 |= 0x00; // lrck_h = 0
        self.write_reg(0x07, reg07)?;

        // Reg 0x08: LRCK low
        self.write_reg(0x08, 0xFF)?;

        // SDP (Serial Data Port) - I2S 16-bit format
        // Reg 0x09: DAC SDP (16-bit = 3 << 2 = 0x0C)
        self.write_reg(0x09, 0x0C)?;
        // Reg 0x0A: ADC SDP (16-bit = 3 << 2 = 0x0C)
        self.write_reg(0x0A, 0x0C)?;

        // Power up analog circuitry (from C reference - CRITICAL values!)
        self.write_reg(0x0D, 0x01)?; // Power up analog
        self.write_reg(0x0E, 0x02)?; // Enable analog PGA + ADC modulator
        self.write_reg(0x12, 0x00)?; // Power up DAC
        self.write_reg(0x13, 0x10)?; // Enable HP drive output
        self.write_reg(0x1C, 0x6A)?; // ADC EQ bypass, cancel DC offset
        self.write_reg(0x37, 0x08)?; // DAC EQ bypass

        // Volume: 85% = (85 * 256 / 100) - 1 = 217 = 0xD9
        self.write_reg(0x32, 0xD9)?;

        self.initialized = true;
        Ok(())
    }

    pub fn set_volume(&mut self, vol: u8) -> Result<(), I::Error> {
        self.write_reg(0x32, vol)
    }

    /// Mute: power down DAC + disable HP output
    pub fn mute(&mut self) -> Result<(), I::Error> {
        self.write_reg(0x12, 0x00)?; // DAC power down
        self.write_reg(0x13, 0x00)?; // Disable HP drive
        self.write_reg(0x32, 0x00)   // Volume 0
    }

    /// Unmute: power up DAC + enable HP output
    pub fn unmute(&mut self) -> Result<(), I::Error> {
        // Re-enable analog blocks that shutdown() may have powered down.
        self.write_reg(0x0D, 0x01)?; // Power up analog
        self.write_reg(0x0E, 0x02)?; // Enable analog PGA + ADC modulator
        self.write_reg(0x12, 0x00)?; // DAC power up (0x00 = on per C ref)
        self.write_reg(0x13, 0x10)?; // Enable HP drive
        self.write_reg(0x32, 0xD0)   // Volume ~80%
    }

    /// Full shutdown: power down ALL analog blocks (not just mute).
    /// Use at boot and between playback events — draws ~0 mA from codec.
    /// `unmute()` re-enables everything on next playback.
    pub fn shutdown(&mut self) -> Result<(), I::Error> {
        // Mute + power down DAC path
        self.write_reg(0x32, 0x00)?; // Volume 0
        self.write_reg(0x13, 0x00)?; // Disable HP drive
        self.write_reg(0x12, 0x20)?; // DAC power down (bit 5 = PDN_DAC)
        // Power down analog PGA + ADC modulator
        self.write_reg(0x0E, 0xFF)?; // PDN_PGA | PDN_MOD | all analog off
        // Power down analog bias
        self.write_reg(0x0D, 0xFC)?; // VMIDSEL=off, IBIAS_PGA off, PDN_ANA
        Ok(())
    }

    pub fn is_initialized(&self) -> bool { self.initialized }
}

/// Fill a buffer with a square wave beep (stereo 16-bit LE).
pub fn fill_beep_buffer(buf: &mut [u8], freq_hz: u32, sample_rate: u32, duration_ms: u32) -> usize {
    let total_samples = (sample_rate * duration_ms / 1000) as usize;
    let period = if freq_hz > 0 { sample_rate / freq_hz } else { 1 };
    let half = period / 2;
    let amplitude: i16 = 10000;
    let mut pos = 0;
    for i in 0..total_samples {
        if pos + 4 > buf.len() { break; }
        let phase = (i as u32) % period;
        let sample = if phase < half { amplitude } else { -amplitude };
        let bytes = sample.to_le_bytes();
        buf[pos] = bytes[0]; buf[pos + 1] = bytes[1]; // L
        buf[pos + 2] = bytes[0]; buf[pos + 3] = bytes[1]; // R
        pos += 4;
    }
    pos
}
