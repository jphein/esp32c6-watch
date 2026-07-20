//! ESP32-C6 internal die-temperature sensor (`esp_hal::tsens`).
//!
//! Reads the chip's on-die temperature (the SoC's own silicon, converted via
//! the SAR-ADC), for a system-page diagnostic alongside the IMU's board temp.
//! **The die runs hotter than ambient** (CPU clock + radio/I/O load), so this
//! reads a few °C above the IMU — it's a thermal-headroom / chip-health readout,
//! not room temperature. Measuring range −40..125 °C.
//!
//! Self-contained: constructed once at boot from `peripherals.TSENS` (otherwise
//! unused on the watch); `celsius()` is a single register read on `&self`, so it
//! drops straight into the main loop's system-page cadence with no borrow churn.

use esp_hal::delay::Delay;
use esp_hal::peripherals::TSENS;
use esp_hal::tsens::{Config, TemperatureSensor};

/// Wraps the on-die temperature sensor.
pub struct DieTemp<'d> {
    sensor: TemperatureSensor<'d>,
}

impl<'d> DieTemp<'d> {
    /// Power up the sensor (default XTAL clock) and let it settle before the
    /// first read. `tsens` comes from `esp_hal::init(..)`'s `Peripherals`.
    ///
    /// `TemperatureSensor::new` returns `Result<_, ConfigError>`, but esp-hal
    /// 1.1's `ConfigError` is an uninhabited enum (no variants), so this can
    /// never actually fail — `expect` documents that invariant and never fires.
    pub fn new(tsens: TSENS<'d>) -> Self {
        let sensor = TemperatureSensor::new(tsens, Config::default())
            .expect("TSENS init is infallible (ConfigError is uninhabited)");
        // Datasheet: wait a few hundred µs after power-up before measuring so
        // the sensor stabilises. Negligible one-time cost at boot.
        Delay::new().delay_micros(200);
        Self { sensor }
    }

    /// Internal die temperature in °C.
    #[inline]
    pub fn celsius(&self) -> f32 {
        self.sensor.get_temperature().to_celsius()
    }

    /// Die temperature in deci-degrees C (×10) — matches the `i16` convention the
    /// sensors/system pages already format as `"{:.1} C"`, so the UI push is a
    /// drop-in alongside the IMU temp.
    #[inline]
    pub fn decidegrees(&self) -> i16 {
        (self.celsius() * 10.0) as i16
    }

    /// Power the sensor down / back up. The SAR-ADC draw is tiny, but light-sleep
    /// (task #29) can drop it before sleeping and restore it on wake.
    pub fn power_down(&self) {
        self.sensor.power_down();
    }

    /// See [`power_down`](Self::power_down).
    pub fn power_up(&self) {
        self.sensor.power_up();
    }
}
