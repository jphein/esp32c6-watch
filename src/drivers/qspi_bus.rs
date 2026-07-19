// QSPI bus driver for CO5300 AMOLED display - DMA version
// Uses SpiDmaBus for large transfers via DMA

use alloc::vec;
use alloc::vec::Vec;
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Address, Command, DataMode, SpiDmaBus};
use esp_hal::Blocking;

// Max bytes per DMA transfer (must fit in DMA TX buffer)
const DMA_CHUNK: usize = 8000;

pub struct QspiBus<'d> {
    spi: SpiDmaBus<'d, Blocking>,
    cs: Output<'d>,
    scratch: Vec<u8>, // heap-allocated scratch buffer for pixel conversion
}

impl<'d> QspiBus<'d> {
    pub fn new(spi: SpiDmaBus<'d, Blocking>, cs: Output<'d>) -> Self {
        Self {
            spi,
            cs,
            scratch: vec![0u8; DMA_CHUNK],
        }
    }

    fn cs_low(&mut self) { self.cs.set_low(); }
    fn cs_high(&mut self) { self.cs.set_high(); }

    pub fn write_command(&mut self, reg: u8) {
        self.cs_low();
        let _ = self.spi.half_duplex_write(
            DataMode::Single, Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single), 0, &[],
        );
        self.cs_high();
    }

    pub fn write_c8d8(&mut self, reg: u8, data: u8) {
        self.cs_low();
        let _ = self.spi.half_duplex_write(
            DataMode::Single, Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single), 0, &[data],
        );
        self.cs_high();
    }

    pub fn write_c8d16d16(&mut self, reg: u8, d1: u16, d2: u16) {
        let data = [(d1 >> 8) as u8, d1 as u8, (d2 >> 8) as u8, d2 as u8];
        self.cs_low();
        let _ = self.spi.half_duplex_write(
            DataMode::Single, Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single), 0, &data,
        );
        self.cs_high();
    }

    pub fn begin_pixels(&mut self) {
        self.cs_low();
        let _ = self.spi.half_duplex_write(
            DataMode::Quad, Command::_8Bit(0x32, DataMode::Single),
            Address::_24Bit(0x003C00, DataMode::Single), 0, &[],
        );
    }

    pub fn stream_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() { return; }
        let max_px = DMA_CHUNK / 2;
        let mut remaining = pixels;
        while !remaining.is_empty() {
            let n = remaining.len().min(max_px);
            for (i, &px) in remaining[..n].iter().enumerate() {
                self.scratch[i * 2] = (px >> 8) as u8;
                self.scratch[i * 2 + 1] = px as u8;
            }
            let _ = self.spi.half_duplex_write(
                DataMode::Quad, Command::None, Address::None, 0, &self.scratch[..n * 2],
            );
            remaining = &remaining[n..];
        }
    }

    pub fn end_pixels(&mut self) { self.cs_high(); }

    pub fn write_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() { return; }
        self.cs_low();
        let max_px = DMA_CHUNK / 2;
        let mut remaining = pixels;
        let mut first = true;
        while !remaining.is_empty() {
            let n = remaining.len().min(max_px);
            for (i, &px) in remaining[..n].iter().enumerate() {
                self.scratch[i * 2] = (px >> 8) as u8;
                self.scratch[i * 2 + 1] = px as u8;
            }
            if first {
                let _ = self.spi.half_duplex_write(
                    DataMode::Quad, Command::_8Bit(0x32, DataMode::Single),
                    Address::_24Bit(0x003C00, DataMode::Single), 0, &self.scratch[..n * 2],
                );
                first = false;
            } else {
                let _ = self.spi.half_duplex_write(
                    DataMode::Quad, Command::None, Address::None, 0, &self.scratch[..n * 2],
                );
            }
            remaining = &remaining[n..];
        }
        self.cs_high();
    }

    pub fn write_repeat(&mut self, color: u16, count: u32) {
        if count == 0 { return; }
        let hi = (color >> 8) as u8;
        let lo = color as u8;
        let max_px = DMA_CHUNK / 2;
        // Fill scratch with repeated color
        for i in 0..max_px {
            self.scratch[i * 2] = hi;
            self.scratch[i * 2 + 1] = lo;
        }
        self.cs_low();
        let mut remaining = count;
        let mut first = true;
        while remaining > 0 {
            let n = remaining.min(max_px as u32);
            let bytes = (n as usize) * 2;
            if first {
                let _ = self.spi.half_duplex_write(
                    DataMode::Quad, Command::_8Bit(0x32, DataMode::Single),
                    Address::_24Bit(0x003C00, DataMode::Single), 0, &self.scratch[..bytes],
                );
                first = false;
            } else {
                let _ = self.spi.half_duplex_write(
                    DataMode::Quad, Command::None, Address::None, 0, &self.scratch[..bytes],
                );
            }
            remaining -= n;
        }
        self.cs_high();
    }
}
