#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_graphics::Drawable;
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::prelude::Point;
use embedded_graphics::text::{Baseline, Text, TextStyleBuilder};
use embedded_hal_bus::spi::ExclusiveDevice;
#[allow(unused)]
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::spi::master::Spi;
use esp_hal::spi::{BitOrder, Mode};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use log::info;
use ssd1683::color::EpdColor;
use ssd1683::{Graphics, Interface};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Embassy initialized!");

    // CrowPanel's e-paper module is not powered by default; GPIO7 must be
    // driven high to enable it (see Elecrow's own example, `pinMode(7,
    // OUTPUT); digitalWrite(7, HIGH);`). Without this the panel has no power
    // at all regardless of what's sent over SPI.
    let mut epd_power = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
    epd_power.set_high();

    // Pin mapping per Elecrow's CrowPanel ESP32-S3 5.79" e-paper example
    // (spi.h): SCK=12 MOSI=11 RES=47 DC=46 CS=45 BUSY=48.
    let busy = Input::new(peripherals.GPIO48, InputConfig::default());
    let reset = Output::new(peripherals.GPIO47, Level::Low, OutputConfig::default());
    let dc = Output::new(peripherals.GPIO46, Level::Low, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO45, Level::Low, OutputConfig::default());

    let spi = Spi::new(
        peripherals.SPI2,
        esp_hal::spi::master::Config::default()
            .with_mode(Mode::_0)
            .with_frequency(Rate::from_mhz(10))
            .with_write_bit_order(BitOrder::MsbFirst),
    )
    .expect("init display spi fail")
    .with_sck(peripherals.GPIO12)
    .with_mosi(peripherals.GPIO11);

    let spi_device = ExclusiveDevice::new(spi, cs, Delay::new()).expect("init spi fail");

    let epd_interfaces = Interface::new(spi_device, busy, reset, dc);

    // The CrowPanel 5.79" panel is 792x272 physical pixels, driven by two
    // cascaded SSD1683 controllers (master + mirrored slave), each addressing
    // 400 px of RAM, for a combined addressable width of 800 px.
    const CHIP_WIDTH: u16 = 400;
    const HEIGHT: u16 = 272;
    let config = ssd1683::Config::new(CHIP_WIDTH, HEIGHT).with_cascade(CHIP_WIDTH);

    let mut dirty_buffer = [0_u8; ssd1683::buffer_size(CHIP_WIDTH * 2, HEIGHT)];
    let mut black_buffer = [0xFF; ssd1683::buffer_size(CHIP_WIDTH * 2, HEIGHT)];
    let mut display = Graphics::new(
        epd_interfaces,
        config,
        Delay::new(),
        &mut dirty_buffer,
        &mut black_buffer,
    );
    info!("epd device ready");

    let style = MonoTextStyleBuilder::new()
        .font(&embedded_graphics::mono_font::ascii::FONT_6X10)
        .text_color(EpdColor::Black)
        .background_color(EpdColor::White)
        .build();
    let text_style = TextStyleBuilder::new().baseline(Baseline::Top).build();

    let _ = Text::with_text_style(
        "update start text test",
        Point::new(50, 250),
        style,
        text_style,
    )
    .draw(&mut display);

    info!("update start");
    display.update().expect("display error");
    info!("update done");

    let _ = Text::with_text_style(
        "part update test display error",
        Point::new(50, 100),
        style,
        text_style,
    )
    .draw(&mut display);

    info!("update start");
    display.update().expect("display error");
    info!("update done");
    loop {
        Timer::after(Duration::from_secs(100)).await;
    }
}
