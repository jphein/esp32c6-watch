pub mod panel;
pub mod co5300;
pub mod framebuffer;
pub mod qspi_bus;
// The plain-SPI DCS panel stack (CYD-class boards). Not compiled on the C6:
// its board module deliberately lacks the SPI_*_HZ consts these read.
#[cfg(not(feature = "board-waveshare-c6"))]
pub mod spi_bus;
#[cfg(feature = "board-esp32s3-cyd")]
pub mod ili9341;

// The board's panel driver, structurally (the panel.rs contract): main.rs and
// the render/flush seams compile against this alias, and each driver carries
// the same inherent surface. The C5 aliases co5300 LINK-ONLY until morpheus's
// st7789 merges — its glass runs his branch's image today.
#[cfg(any(feature = "board-waveshare-c6", feature = "board-cyd-c5"))]
pub type ActivePanel<'d> = co5300::Co5300Display<'d>;
#[cfg(feature = "board-esp32s3-cyd")]
pub type ActivePanel<'d> = ili9341::Ili9341Display<'d>;
