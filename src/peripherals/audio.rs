// ES8311 Audio codec (speaker DAC — the mics are on the ES7210) - proper init
// from the Waveshare C reference. Playback data rides the shared I2S TX ring
// (audio_out/silent_clock_task, #23); this driver only sequences codec power:
// unmute() before the amp rises, shutdown() after it drops (see service_amp).

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

        // Reg 0x05: adc_div | dac_div — both dividers are 1, encoded as
        // (div-1): (0 << 4) | 0 = 0. The literal `1 - 1` mirrors the C ref's
        // (div-1) form; clippy's eq_op fires on it as a false positive here.
        #[allow(clippy::eq_op)]
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

    /// Enable the ADC (mic) capture path for I2S RX. Reverses `shutdown()`'s
    /// ADC-side power-down and configures the analog mic input + level.
    ///
    /// `pga_gain` is the reg-0x14 analog-PGA gain code (bits[3:0], 0..=0x0F ≈
    /// 0–30 dB per the ES8311 datasheet; higher = more sensitive). Register
    /// *semantics* are datasheet-confirmed; the exact gain/volume codes are the
    /// on-glass tuning surface — MC6 picks the mic L/R slot + final gain so a
    /// normal room sits mid-scale without railing.
    pub fn enable_adc(&mut self, pga_gain: u8) -> Result<(), I::Error> {
        // Mirror the vendor ES8311 record path exactly (esp-bsp es8311.c: the analog
        // power-up from es8311_init + es8311_microphone_config + microphone_gain_set).
        // Re-power the analog blocks that shutdown() cut, then set the mic registers.
        self.write_reg(0x0D, 0x01)?; // power up analog bias
        self.write_reg(0x0E, 0x02)?; // enable analog PGA + ADC modulator (PDN_PGA=0)
        self.write_reg(0x0A, 0x0C)?; // ADC serial port = 16-bit I2S (re-assert; init sets it)
        self.write_reg(0x1C, 0x6A)?; // ADC EQ bypass + digital DC-offset cancel
        self.write_reg(0x17, 0xC8)?; // ADC digital volume (vendor es8311_microphone_config)
        // Analog mic input: bit6=0 = analog mic (not DMIC); bits[3:0] = PGA gain.
        // 0x10 | 0x0A = 0x1A — the vendor's "enable analog MIC + max PGA gain".
        self.write_reg(0x14, 0x10 | (pga_gain & 0x0F))?;
        // ADC gain SCALE — this write was MISSING, and it is the fix for the meter
        // pinned at the −60 dBFS floor / "no audio response": without it the mic's AC
        // signal sits below mic_dsp's −60 floor and reads as flat silence (rms_dbfs
        // mean-subtracts, so it is not a DC issue — it's ~30 dB of missing gain).
        // 0x06 = +30 dB (es8311_mic_gain_t code 6), the level esp_codec_dev opens at.
        self.write_reg(0x16, 0x06)?;
        // ADCDAT source select: 0x00 = ADCDAT_SEL 0 = the REAL ADC output drives ASDOUT
        // (0x60 = ADCDAT_SEL 6 was the DAC→ASDOUT test loopback). Made explicit so the
        // mic serial-out carries the true converter regardless of the reset default.
        self.write_reg(0x44, 0x00)?;
        // NB: deliberately do NOT write reg 0x15 (ADC ramp / dmic-sense). The vendor
        // record path never touches it; leave 0x15 at its reset default.
        Ok(())
    }

    /// Power the ADC (mic) path back down. Mirrors `shutdown()`'s analog-off
    /// writes so the codec draws ~0 mA when the sound-level meter isn't open.
    /// (v1 never captures + plays simultaneously, so this shares the analog
    /// power regs with the DAC path; revisit if full-duplex audio is added.)
    pub fn disable_adc(&mut self) -> Result<(), I::Error> {
        self.write_reg(0x0E, 0xFF)?; // PDN_PGA | PDN_MOD — analog capture off
        self.write_reg(0x0D, 0xFC)?; // analog bias off
        Ok(())
    }

    pub fn is_initialized(&self) -> bool { self.initialized }
}

// fill_beep_buffer (stereo square-wave synth) retired in v0.8.5: SFX are now
// synthesized MONO via mic-dsp (fill_tone_mono_s16le / fill_click_mono_s16le,
// host-unit-tested) and expanded to stereo by the audio_out feeder (#23).
