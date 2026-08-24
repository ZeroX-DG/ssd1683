pub mod color;
pub mod config;
mod tools;

use crate::command::{Chip, Command, DataEntryMode, DeepSleepMode, IncrementAxis};
use crate::error::Error;
use crate::graphics::color::EpdColor;
pub use crate::graphics::config::{CascadeConfig, Config, Rotation};
use crate::graphics::tools::{DirtyRect, RegionIterator, calculate_dirty_area, rotation};
use crate::{DisplayInterface, Interface};
use embedded_graphics_core::Pixel;
use embedded_graphics_core::prelude::{DrawTarget, OriginDimensions, Point, Size};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;
use log::info;

/// 计算给定分辨率（像素）的 1bpp 帧缓冲区所需的字节数。`width` 必须是 8 的倍数。
pub const fn buffer_size(width: u16, height: u16) -> usize {
    (width as usize) * (height as usize) / 8
}
/// 经过多少次快速刷新后进行一次完整更新
const MAX_FAST_UPDATE_TIME: usize = 100;

pub struct Graphics<'buf, SPI, BUSY, RESET, DC, DELAY>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    DC: OutputPin,
    RESET: OutputPin,
    DELAY: DelayNs,
{
    interface: Interface<SPI, BUSY, RESET, DC>,
    config: Config,
    delay: DELAY,
    update_count: usize,
    dirty_buffer: &'buf mut [u8],
    black_buffer: &'buf mut [u8],
    #[cfg(feature = "use_red")]
    red_buffer: &'buf mut [u8],
}

#[allow(clippy::type_complexity)]
impl<'buf, SPI, BUSY, RESET, DC, DELAY> Graphics<'buf, SPI, BUSY, RESET, DC, DELAY>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
    DC: OutputPin,
    DELAY: DelayNs,
{
    pub fn new(
        interface: Interface<SPI, BUSY, RESET, DC>,
        config: Config,
        delay: DELAY,
        dirty_buffer: &'buf mut [u8],
        black_buffer: &'buf mut [u8],
        #[cfg(feature = "use_red")] red_buffer: &'buf mut [u8],
    ) -> Self {
        if !config.width.is_multiple_of(8) {
            panic!("Width must be multiple of 8");
        }
        Self {
            interface,
            config,
            delay,
            update_count: 0,
            dirty_buffer,
            black_buffer,
            #[cfg(feature = "use_red")]
            red_buffer,
        }
    }

    pub fn update(&mut self) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        if self.update_count == 0 {
            self.update_normal()?;
        } else {
            let dirty_rect = match calculate_dirty_area(self.dirty_buffer, self.config.width as u32)
            {
                None => {
                    return Ok(());
                }
                Some(dirty_rect) => dirty_rect,
            };
            info!("graphics update dirty area: {:?}", &dirty_rect);
            // 当更新范围过大时使用全局更新
            if dirty_rect.max_byte_col - dirty_rect.min_byte_col > (self.config.width / 16) as u8
                && dirty_rect.max_y - dirty_rect.min_y > self.config.height / 2
            {
                if self.update_count >= MAX_FAST_UPDATE_TIME {
                    self.update_count = 1;
                    self.update_normal()?;
                } else {
                    self.update_fast()?;
                }
            } else {
                self.update_part(dirty_rect)?;
            }
        }
        self.dirty_buffer.iter_mut().for_each(|d| *d = 0);
        self.update_count += 1;
        self.deep_sleep(DeepSleepMode::PreserveRAM)?;
        Ok(())
    }

    fn update_normal(&mut self) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        info!("graphics update normal");
        // init
        self.interface.reset(&mut self.delay)?;
        self.interface.busy_wait();
        Command::SoftReset.execute(&mut self.interface)?;
        self.interface.busy_wait();

        if let Some(cascade) = self.config.cascade {
            // 级联双芯片面板依赖各自的 OTP 默认参数（栅极数、电压等），
            // 因此跳过 DriverOutputControl/DisplayUpdateControl1/BorderWaveform，
            // 只寻址并写入两颗芯片各自的 RAM 区域。
            self.write_cascade_frame(cascade)?;
        } else {
            Command::DriverOutputControl(self.config.height, 0x00).execute(&mut self.interface)?;
            Command::DisplayUpdateControl1(0x4000).execute(&mut self.interface)?;
            Command::BorderWaveform(0x05).execute(&mut self.interface)?;
            Command::DataEntryMode(
                DataEntryMode::IncrementYIncrementX,
                IncrementAxis::Horizontal,
            )
            .execute(&mut self.interface)?;
            Command::StartEndXPosition(0x00, (self.config.width / 8 - 1) as u8)
                .execute(&mut self.interface)?;
            Command::StartEndYPosition(0x00, self.config.height).execute(&mut self.interface)?;
            Command::XAddress(0x00).execute(&mut self.interface)?;
            Command::YAddress(0x00).execute(&mut self.interface)?;
            self.interface.busy_wait();

            // write data
            Command::WriteRamBW.execute(&mut self.interface)?;
            self.interface.send_data(self.black_buffer)?;
            #[cfg(feature = "use_red")]
            {
                Command::WriteRamRed.execute(&mut self.interface)?;
                self.interface.send_data(self.red_buffer)?;
            }
        }

        // refresh
        Command::DisplayUpdateControl2(0xF7).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();
        Ok(())
    }

    /// 寻址并写入级联双芯片面板整帧的黑白 RAM：主芯片负责左半部分
    /// （地址递增），从芯片负责右半部分（相对主芯片左右镜像，地址递减）。
    fn write_cascade_frame(
        &mut self,
        cascade: CascadeConfig,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        let chip_width_bytes = (cascade.chip_width / 8) as u8;
        let y_end = self.config.height.saturating_sub(1);
        self.write_cascade_chip_ram(Chip::Master, 0, chip_width_bytes - 1, 0, y_end)?;
        self.write_cascade_chip_ram(
            Chip::Slave,
            chip_width_bytes,
            chip_width_bytes * 2 - 1,
            0,
            y_end,
        )?;
        Ok(())
    }

    /// 为级联面板中的一颗芯片寻址并写入其黑白 RAM 窗口。
    /// `col_start`/`col_end` 是相对整个帧缓冲区（而非单颗芯片）的字节列范围。
    fn write_cascade_chip_ram(
        &mut self,
        chip: Chip,
        col_start: u8,
        col_end: u8,
        y_start: u16,
        y_end: u16,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        // 两颗芯片在物理面板上左右镜像安装，其中一颗的地址递增方向与
        // 帧缓冲区的列顺序相反，因此 X 起止位置互换、X 方向改为递减。
        // 经实机验证：offset=0x00（此处标记为 Master）的芯片是需要
        // 镜像（递减）寻址的那一颗，offset=0x80（Slave）按正常方向寻址。
        let (mode, x_start, x_end, x_addr) = match chip {
            Chip::Master => (
                DataEntryMode::DecrementXIncrementY,
                col_end,
                col_start,
                col_end,
            ),
            Chip::Slave => (
                DataEntryMode::IncrementYIncrementX,
                col_start,
                col_end,
                col_start,
            ),
        };
        Command::DataEntryMode(mode, IncrementAxis::Horizontal)
            .execute_on(&mut self.interface, chip)?;
        Command::StartEndXPosition(x_start, x_end).execute_on(&mut self.interface, chip)?;
        Command::StartEndYPosition(y_start, y_end).execute_on(&mut self.interface, chip)?;
        Command::XAddress(x_addr).execute_on(&mut self.interface, chip)?;
        Command::YAddress(y_start).execute_on(&mut self.interface, chip)?;
        self.interface.busy_wait();

        let region = DirtyRect {
            min_byte_col: col_start,
            max_byte_col: col_end,
            min_y: y_start,
            max_y: y_end,
        };
        Command::WriteRamBW.execute_on(&mut self.interface, chip)?;
        for row in RegionIterator::new(self.black_buffer, self.config.width as usize, &region) {
            self.interface.send_data(row)?;
        }
        Ok(())
    }

    fn update_fast(&mut self) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        info!("graphics update fast");
        // init
        self.interface.reset(&mut self.delay)?;
        Command::SoftReset.execute(&mut self.interface)?;
        self.interface.busy_wait();
        Command::DisplayUpdateControl1(0x4000).execute(&mut self.interface)?;
        Command::BorderWaveform(0x05).execute(&mut self.interface)?;
        Command::WriteTemperatureSensor(0x6E).execute(&mut self.interface)?;
        Command::DisplayUpdateControl2(0x91).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;

        if let Some(cascade) = self.config.cascade {
            self.write_cascade_frame(cascade)?;
        } else {
            Command::DataEntryMode(
                DataEntryMode::IncrementYIncrementX,
                IncrementAxis::Horizontal,
            )
            .execute(&mut self.interface)?;
            Command::StartEndXPosition(0x00, (self.config.width / 8 - 1) as u8)
                .execute(&mut self.interface)?;
            Command::StartEndYPosition(0x00, self.config.height).execute(&mut self.interface)?;

            Command::XAddress(0x00).execute(&mut self.interface)?;
            Command::YAddress(0x00).execute(&mut self.interface)?;
            self.interface.busy_wait();

            // write data
            Command::WriteRamBW.execute(&mut self.interface)?;
            self.interface.send_data(self.black_buffer)?;
            #[cfg(feature = "use_red")]
            {
                Command::WriteRamRed.execute(&mut self.interface)?;
                self.interface.send_data(self.red_buffer)?;
            }
        }

        // refresh
        Command::DisplayUpdateControl2(0xC7).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();
        Ok(())
    }

    fn update_part(
        &mut self,
        dirty_rect: DirtyRect,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        info!("graphics update part");
        // init
        self.interface.reset(&mut self.delay)?;
        Command::DisplayUpdateControl1(0x0000).execute(&mut self.interface)?;
        Command::BorderWaveform(0x80).execute(&mut self.interface)?;

        if let Some(cascade) = self.config.cascade {
            let chip_width_bytes = (cascade.chip_width / 8) as u8;
            if dirty_rect.min_byte_col < chip_width_bytes {
                let col_end = dirty_rect.max_byte_col.min(chip_width_bytes - 1);
                self.write_cascade_chip_ram(
                    Chip::Master,
                    dirty_rect.min_byte_col,
                    col_end,
                    dirty_rect.min_y,
                    dirty_rect.max_y,
                )?;
            }
            if dirty_rect.max_byte_col >= chip_width_bytes {
                let col_start = dirty_rect.min_byte_col.max(chip_width_bytes);
                self.write_cascade_chip_ram(
                    Chip::Slave,
                    col_start,
                    dirty_rect.max_byte_col,
                    dirty_rect.min_y,
                    dirty_rect.max_y,
                )?;
            }
        } else {
            Command::StartEndXPosition(dirty_rect.min_byte_col, dirty_rect.max_byte_col)
                .execute(&mut self.interface)?;
            Command::StartEndYPosition(dirty_rect.min_y, dirty_rect.max_y)
                .execute(&mut self.interface)?;
            Command::XAddress(dirty_rect.min_byte_col).execute(&mut self.interface)?;
            Command::YAddress(dirty_rect.min_y).execute(&mut self.interface)?;

            // write data
            let bw_region_iter =
                RegionIterator::new(self.black_buffer, self.config.width as usize, &dirty_rect);
            Command::WriteRamBW.execute(&mut self.interface)?;
            for region in bw_region_iter {
                self.interface.send_data(region)?;
            }
            #[cfg(feature = "use_red")]
            {
                let red_region_iter =
                    RegionIterator::new(self.red_buffer, self.config.width as usize, &dirty_rect);
                Command::WriteRamRed.execute(&mut self.interface)?;
                for region in red_region_iter {
                    self.interface.send_data(region)?;
                }
            }
        }

        // refresh
        Command::DisplayUpdateControl2(0xFF).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();
        Ok(())
    }

    pub fn deep_sleep(
        &mut self,
        deep_sleep_mode: DeepSleepMode,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        Command::DeepSleepMode(deep_sleep_mode).execute(&mut self.interface)?;
        self.delay.delay_ms(100);
        Ok(())
    }

    fn set_pixel(&mut self, x: u32, y: u32, color: EpdColor) {
        let (index, bit) = rotation(
            x,
            y,
            self.config.width as u32,
            self.config.height as u32,
            self.config.rotation,
        );
        let index = index as usize;
        self.dirty_buffer[index] |= bit;

        match color {
            EpdColor::Black => {
                self.black_buffer[index] &= !bit;
                #[cfg(feature = "use_red")]
                {
                    self.red_buffer[index] &= !bit;
                }
            }
            EpdColor::White => {
                self.black_buffer[index] |= bit;
                #[cfg(feature = "use_red")]
                {
                    self.red_buffer[index] &= !bit;
                }
            }
            #[cfg(feature = "use_red")]
            EpdColor::Red => {
                self.black_buffer[index] |= bit;
                self.red_buffer[index] |= bit;
            }
        }
    }
}

impl<SPI, BUSY, RESET, DC, DELAY> OriginDimensions for Graphics<'_, SPI, BUSY, RESET, DC, DELAY>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
    DC: OutputPin,
    DELAY: DelayNs,
{
    fn size(&self) -> Size {
        let width = self.config.width as u32;
        let height = self.config.height as u32;
        match self.config.rotation {
            Rotation::Rotate0 | Rotation::Rotate180 => Size::new(width, height),
            Rotation::Rotate90 | Rotation::Rotate270 => Size::new(height, width),
        }
    }
}

impl<SPI, BUSY, DC, RESET, DELAY> DrawTarget for Graphics<'_, SPI, BUSY, RESET, DC, DELAY>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
    DC: OutputPin,
    DELAY: DelayNs,
{
    type Color = EpdColor;
    type Error = Error<SPI::Error, RESET::Error, DC::Error>;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for pixel in pixels {
            let Pixel(Point { x, y }, color) = pixel;
            self.set_pixel(x as u32, y as u32, color);
        }
        Ok(())
    }
}
