// QMI8658 6-axis IMU driver (Accelerometer + Gyroscope)
// Reference: SensorLib/src/SensorQMI8658.hpp
// I2C address 0x6B

use embedded_hal::i2c::I2c;
use esp_hal::delay::Delay;

const QMI8658_ADDR: u8 = 0x6B;

// Registers
const REG_WHO_AM_I: u8 = 0x00;
const REG_CTRL1: u8 = 0x02;  // Serial interface and sensor enable
const REG_CTRL2: u8 = 0x03;  // Accelerometer settings
const REG_CTRL3: u8 = 0x04;  // Gyroscope settings
const REG_CTRL5: u8 = 0x06;  // Low-pass filter
const REG_CTRL7: u8 = 0x08;  // Enable sensors
const REG_CTRL8: u8 = 0x09;  // Motion detection control (pedometer/tap/motion engines)
const REG_CTRL9: u8 = 0x0A;  // Host command register (CTRL9 protocol)
const REG_CAL1_L: u8 = 0x0B; // CTRL9 command parameter registers (CAL1..CAL4)
const REG_CAL1_H: u8 = 0x0C;
const REG_CAL2_L: u8 = 0x0D;
const REG_CAL2_H: u8 = 0x0E;
const REG_CAL3_L: u8 = 0x0F;
const REG_CAL3_H: u8 = 0x10;
const REG_CAL4_L: u8 = 0x11;
const REG_CAL4_H: u8 = 0x12;
const REG_STATUSINT: u8 = 0x2D; // bit7 = CTRL9 CmdDone handshake
const REG_AX_L: u8 = 0x35;   // Accel X low byte
const REG_GX_L: u8 = 0x3B;   // Gyro X low byte
const REG_TEMP_L: u8 = 0x33; // Temperature low byte
const REG_STEP_CNT_L: u8 = 0x5A; // 24-bit step counter, little-endian L/M/H

const QMI8658_WHO_AM_I: u8 = 0x05; // Expected chip ID

// CTRL9 commands
const CTRL_CMD_ACK: u8 = 0x00;
const CTRL_CMD_CONFIGURE_PEDOMETER: u8 = 0x0D;
const STATUSINT_CMD_DONE: u8 = 0x80; // STATUSINT bit 7

// CTRL7 sensor-enable bits
const CTRL7_ACCEL_EN: u8 = 0x01;
const CTRL7_GYRO_EN: u8 = 0x02;
// CTRL8 bit 4: pedometer engine enable (datasheet Pedo_EN)
const CTRL8_PEDO_EN: u8 = 0x10;

// Max STATUSINT polls while waiting for a CTRL9 command to complete.
// Each poll now delays 1 ms (matching SensorLib writeCommand's `delay(1)`,
// SensorQMI8658.hpp ~L1241 assert / ~L1258 clear), so this bounds the
// CmdDone wait to ~1000 ms — the same budget as SensorLib writeCommand's
// default `wait_ms = 1000`.
const CTRL9_POLL_LIMIT: u32 = 1000;

#[derive(Debug, Clone, Copy, Default)]
pub struct AccelData {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GyroData {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub struct Qmi8658Imu<I> {
    i2c: I,
    accel_scale: f32,
    gyro_scale: f32,
}

impl<I: I2c> Qmi8658Imu<I> {
    pub fn new(i2c: I) -> Self {
        Self {
            i2c,
            accel_scale: 1.0 / 4096.0,  // ±8g default
            gyro_scale: 1.0 / 64.0,      // ±512 dps default
        }
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8];
        self.i2c.write_read(QMI8658_ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(QMI8658_ADDR, &[reg, val])
    }

    fn read_regs(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), I::Error> {
        self.i2c.write_read(QMI8658_ADDR, &[reg], buf)
    }

    /// Initialize the IMU. Returns true if chip ID matches.
    pub fn init(&mut self) -> Result<bool, I::Error> {
        let id = self.read_reg(REG_WHO_AM_I)?;
        if id != QMI8658_WHO_AM_I {
            return Ok(false);
        }

        // CTRL1: address auto-increment (bit 6), little-endian output (bit 5 = 0).
        //
        // Root-cause fix (steps stuck / IMU reads corrupt): the old value here was
        // 0x60 = ADDR_AI (bit 6) | BE (bit 5, SPI_BIG_ENDIAN). Bit 5 = 1 puts the
        // chip in BIG-endian output mode, but every read in this driver is
        // little-endian (`i16::from_le_bytes` for accel/gyro/temp; `L | M<<8 | H<<16`
        // for the 24-bit step counter), so all multi-byte reads were byte-swapped —
        // including STEP_CNT, so the pedometer value never read correctly. SensorLib
        // and tinygo's qmi8658c driver both use 0x40 (ADDR_AI only, little-endian).
        //
        // NOTE: the previous 0x40 write labelled "soft reset" was a misconception —
        // CTRL1 bit 6 is ADDR_AI, not reset. The QMI8658 soft reset is the dedicated
        // RESET register (0x60) written with 0xB0 (polled via RST_RESULT). We skip
        // reset (no delay provider in this sync init); a cold boot starts from
        // defaults and every field below is written explicitly.
        self.write_reg(REG_CTRL1, 0x40)?;

        // CTRL2: Accelerometer ODR=62.5Hz, Full scale=±8g
        // ODR[3:0]=0b0111 (62.5Hz), FS[6:4]=0b010 (±8g)
        // 62.5Hz is the vendor-recommended accel-only rate for the hardware
        // pedometer (params below assume ~50-62.5Hz samples).
        self.write_reg(REG_CTRL2, 0x27)?;
        self.accel_scale = 8.0 / 32768.0; // ±8g

        // CTRL3: Gyroscope ODR=56.05Hz, Full scale=±512dps
        // ODR[3:0]=0b0111 (56.05Hz), FS[6:4]=0b011 (±512dps)
        // In 6DOF mode the shared ODR follows the gyro, so keep it close to
        // the accel-only 62.5Hz to keep pedometer timing valid while the
        // gyro is on (UI needs no more than ~30 samples/s anyway).
        self.write_reg(REG_CTRL3, 0x37)?;
        self.gyro_scale = 512.0 / 32768.0; // ±512dps

        // CTRL5: Low-pass filter enabled for both accel and gyro
        self.write_reg(REG_CTRL5, 0x11)?;

        // Configure the hardware pedometer while all sensors are still
        // disabled (required: CTRL9 config commands with sensors running
        // are rejected). Non-fatal on timeout: accel/gyro still work.
        let _ = self.configure_pedometer()?;

        // CTRL8: enable pedometer engine (bit 4)
        self.write_reg(REG_CTRL8, CTRL8_PEDO_EN)?;

        // CTRL7: Enable accelerometer and gyroscope
        // Bit 0: accel enable, Bit 1: gyro enable
        // (SyncSample bit 7 stays 0: the pedometer only works in
        // non-SyncSample mode.)
        self.write_reg(REG_CTRL7, CTRL7_ACCEL_EN | CTRL7_GYRO_EN)?;

        Ok(true)
    }

    /// Execute a CTRL9 command: write the command, wait for
    /// STATUSINT.CmdDone (bit 7), acknowledge with CTRL_CMD_ACK, then wait
    /// for the flag to clear. Returns Ok(false) on handshake timeout.
    fn ctrl9_command(&mut self, cmd: u8) -> Result<bool, I::Error> {
        self.write_reg(REG_CTRL9, cmd)?;
        if !self.wait_cmd_done(true)? {
            return Ok(false);
        }
        self.write_reg(REG_CTRL9, CTRL_CMD_ACK)?;
        self.wait_cmd_done(false)
    }

    /// Poll STATUSINT until CmdDone matches `set` (true = wait for the bit
    /// to assert, false = wait for it to clear). Bounded by CTRL9_POLL_LIMIT.
    ///
    /// Mirrors SensorLib's writeCommand poll loop (SensorQMI8658.hpp: the
    /// assert wait ~L1236-1244, the clear wait ~L1250-1260) — read STATUSINT,
    /// then `delay(1)` (1 ms) before re-checking. That 1 ms settling delay
    /// between polls is the robustness fix: the chip needs real wall-clock
    /// time to process the CTRL9 command and toggle CmdDone, which the old
    /// tight I2C poll (zero delay) never reliably gave it.
    fn wait_cmd_done(&mut self, set: bool) -> Result<bool, I::Error> {
        let delay = Delay::new();
        for _ in 0..CTRL9_POLL_LIMIT {
            let s = self.read_reg(REG_STATUSINT)?;
            delay.delay_millis(1);
            if ((s & STATUSINT_CMD_DONE) != 0) == set {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Configure the hardware pedometer via CTRL_CMD_CONFIGURE_PEDOMETER
    /// (0x0D). Two CTRL9 transactions, parameters staged in CAL1..CAL4.
    /// Values are the vendor-recommended defaults for ~50Hz accel ODR
    /// (QMI8658 datasheet / SensorLib pedometer example):
    ///   ped_sample_cnt     = 0x0032 (50 samples  = 1 s batch window)
    ///   ped_fix_peak2peak  = 0x00CC (~0.2 g valid peak-to-peak threshold)
    ///   ped_fix_peak       = 0x0066 (~0.1 g peak-vs-average threshold)
    ///   ped_time_up        = 0x00C8 (200 samples = 4 s step timeout)
    ///   ped_time_low       = 0x14   (20 samples = 0.4 s step quiet time)
    ///   ped_time_cnt_entry = 0x0A   (10 continuous steps to start counting)
    ///   ped_fix_precision  = 0x00   (recommended 0)
    ///   ped_sig_count      = 0x04   (update output registers every 4 steps)
    /// Must be called with accel + gyro disabled. Returns Ok(false) if the
    /// CTRL9 handshake times out.
    fn configure_pedometer(&mut self) -> Result<bool, I::Error> {
        // First transaction (CAL4_H = 0x01): sample count + peak thresholds
        self.write_reg(REG_CAL1_L, 0x32)?; // ped_sample_cnt low
        self.write_reg(REG_CAL1_H, 0x00)?; // ped_sample_cnt high
        self.write_reg(REG_CAL2_L, 0xCC)?; // ped_fix_peak2peak low
        self.write_reg(REG_CAL2_H, 0x00)?; // ped_fix_peak2peak high
        self.write_reg(REG_CAL3_L, 0x66)?; // ped_fix_peak low
        self.write_reg(REG_CAL3_H, 0x00)?; // ped_fix_peak high
        self.write_reg(REG_CAL4_H, 0x01)?; // parameter page 1
        self.write_reg(REG_CAL4_L, 0x02)?;
        if !self.ctrl9_command(CTRL_CMD_CONFIGURE_PEDOMETER)? {
            return Ok(false);
        }

        // Second transaction (CAL4_H = 0x02): timing + count parameters
        self.write_reg(REG_CAL1_L, 0xC8)?; // ped_time_up low
        self.write_reg(REG_CAL1_H, 0x00)?; // ped_time_up high
        self.write_reg(REG_CAL2_L, 0x14)?; // ped_time_low
        self.write_reg(REG_CAL2_H, 0x0A)?; // ped_time_cnt_entry
        self.write_reg(REG_CAL3_L, 0x00)?; // ped_fix_precision
        self.write_reg(REG_CAL3_H, 0x04)?; // ped_sig_count
        self.write_reg(REG_CAL4_H, 0x02)?; // parameter page 2
        self.write_reg(REG_CAL4_L, 0x02)?;
        self.ctrl9_command(CTRL_CMD_CONFIGURE_PEDOMETER)
    }

    /// Read the hardware pedometer's 24-bit step counter (STEP_CNT_L/M/H).
    /// Cheap single I2C burst; works in any power state where the
    /// accelerometer is running.
    pub fn read_step_count(&mut self) -> Result<u32, I::Error> {
        let mut buf = [0u8; 3];
        self.read_regs(REG_STEP_CNT_L, &mut buf)?;
        Ok(u32::from(buf[0]) | (u32::from(buf[1]) << 8) | (u32::from(buf[2]) << 16))
    }

    /// Low-power state: gyro off (it alone draws ~1.5 mA) but the
    /// accelerometer stays on at 62.5Hz (tens of uA) so the hardware
    /// pedometer keeps counting steps in the background.
    pub fn power_down(&mut self) -> Result<(), I::Error> {
        self.write_reg(REG_CTRL7, CTRL7_ACCEL_EN)
    }

    /// Full power: accelerometer + gyroscope. Call before reading gyro.
    pub fn power_up(&mut self) -> Result<(), I::Error> {
        self.write_reg(REG_CTRL7, CTRL7_ACCEL_EN | CTRL7_GYRO_EN)
    }

    /// Read accelerometer data in g.
    pub fn read_accel(&mut self) -> Result<AccelData, I::Error> {
        let mut buf = [0u8; 6];
        self.read_regs(REG_AX_L, &mut buf)?;

        let x = i16::from_le_bytes([buf[0], buf[1]]) as f32 * self.accel_scale;
        let y = i16::from_le_bytes([buf[2], buf[3]]) as f32 * self.accel_scale;
        let z = i16::from_le_bytes([buf[4], buf[5]]) as f32 * self.accel_scale;

        Ok(AccelData { x, y, z })
    }

    /// Read gyroscope data in degrees per second.
    pub fn read_gyro(&mut self) -> Result<GyroData, I::Error> {
        let mut buf = [0u8; 6];
        self.read_regs(REG_GX_L, &mut buf)?;

        let x = i16::from_le_bytes([buf[0], buf[1]]) as f32 * self.gyro_scale;
        let y = i16::from_le_bytes([buf[2], buf[3]]) as f32 * self.gyro_scale;
        let z = i16::from_le_bytes([buf[4], buf[5]]) as f32 * self.gyro_scale;

        Ok(GyroData { x, y, z })
    }

    /// Read chip temperature in °C.
    pub fn read_temperature(&mut self) -> Result<f32, I::Error> {
        let mut buf = [0u8; 2];
        self.read_regs(REG_TEMP_L, &mut buf)?;
        let raw = i16::from_le_bytes([buf[0], buf[1]]);
        Ok(raw as f32 / 256.0)
    }

    /// Read chip ID.
    pub fn read_chip_id(&mut self) -> Result<u8, I::Error> {
        self.read_reg(REG_WHO_AM_I)
    }
}

// ===========================================================================
// Wrist-raise ("tilt-to-wake") detector
// ===========================================================================
//
// HARDWARE PATH: polling, NOT interrupt. On the Waveshare
// ESP32-C6-Touch-AMOLED-2.06 the QMI8658 INT1/INT2 pins are NOT routed to any
// wake-capable ESP32-C6 GPIO — the vendor BSP declares `BSP_CAPS_IMU 0` and
// defines no IMU-INT pin macro (the only interrupt input on the board is the
// FT3168 touch INT on GPIO15). So there is no hardware wake-on-motion to arm
// as a light-sleep GPIO wake source. Instead the AOD loop wakes on a short
// timer, reads one accel sample, and feeds it to this detector.
//
// AXIS CONVENTION (confirmed from the tilt games + parallax in main.rs:
// maze.rs maps `ax`=fore/aft and `ay`=left/right in-plane tilt, and
// set_parallax uses (ax, ay)): x and y are the two IN-PLANE display axes, so
// `z` is the DISPLAY NORMAL (out of the screen). Face-up (screen toward the
// sky) => gravity lies along the outward normal => `az ~= +1 g`. If a build
// reads the gesture inverted on glass, flip `RAISE_FACE_SIGN` to -1.0.

/// Signed direction of "face up" along the display-normal (z) axis.
/// +1.0 => face-up gives az ~ +1g. Flip to -1.0 if the mounting is inverted.
const RAISE_FACE_SIGN: f32 = 1.0;
/// Face-up detection threshold on the (signed) display-normal component.
/// az beyond this (toward face-up) counts as "looking at the watch".
/// 0.60 g == the normal within ~53 deg of vertical (a comfortable reading tilt).
const RAISE_UP_Z: f32 = 0.60;
/// "Wrist down" threshold: the (signed) normal component must first fall below
/// this to arm the detector, so a raise is a genuine down->up transition and a
/// watch already held face-up cannot re-trigger. Hysteresis gap vs RAISE_UP_Z.
const RAISE_DOWN_Z: f32 = 0.35;
/// Max in-plane gravity (squared) allowed in the face-up pose. Rejects edge-on
/// / tumbling poses so a pocket can't fake a clean face-up. 0.70 g -> 0.49 g^2.
const RAISE_INPLANE_MAX_SQ: f32 = 0.49;
/// Total accel magnitude band (squared) for a "stable, gravity-dominated"
/// sample: rejects violent shake and freefall. [0.60 g, 1.40 g].
const RAISE_MAG_LO_SQ: f32 = 0.36;
const RAISE_MAG_HI_SQ: f32 = 1.96;

/// Debounced wrist-raise gesture detector fed one accel sample per AOD poll
/// (~0.7 s). `update` returns `true` exactly once when the watch is raised from
/// a clear wrist-down pose to a stable face-up pose; it will not fire again
/// until the wrist drops (re-arming the detector), so holding the watch up does
/// not re-wake. All comparisons are squared to avoid a sqrt / libm dependency.
#[derive(Debug, Clone, Copy, Default)]
pub struct RaiseDetector {
    /// Set once a clear wrist-down pose is seen; consumed on a fired raise.
    armed: bool,
}

impl RaiseDetector {
    pub const fn new() -> Self {
        Self { armed: false }
    }

    /// Reset to the disarmed state (e.g. a fresh AOD session). Not required for
    /// correctness — the arm/consume logic self-recovers — but available.
    pub fn reset(&mut self) {
        self.armed = false;
    }

    /// Feed one accelerometer sample. Returns true on a detected wrist-raise.
    pub fn update(&mut self, a: AccelData) -> bool {
        // Signed display-normal component, oriented so +ve == face-up.
        let n = a.z * RAISE_FACE_SIGN;

        // A clear non-facing-up pose arms the detector.
        if n < RAISE_DOWN_Z {
            self.armed = true;
        }

        let inplane_sq = a.x * a.x + a.y * a.y;
        let mag_sq = inplane_sq + a.z * a.z;
        let stable = mag_sq > RAISE_MAG_LO_SQ
            && mag_sq < RAISE_MAG_HI_SQ
            && inplane_sq < RAISE_INPLANE_MAX_SQ;

        // Fire once on the down->up crossing into a stable face-up pose.
        if self.armed && n > RAISE_UP_Z && stable {
            self.armed = false;
            return true;
        }
        false
    }
}
