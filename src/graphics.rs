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

        if let Some(cascade) = self.config.cascade {
            // 级联双芯片面板上，参考实现从不在一次完整刷新（0xF7）中直接写入
            // 实际图像内容：完整刷新只用于 EPD_Display_Clear 的空白清屏
            // （RAM(BW)=0xFF、RAM(RED)=0x00），真正的图像内容始终紧接着通过
            // 一次快刷（EPD_FastMode1Init + EPD_Display + EPD_FastUpdate）写入。
            // 因此这里链式执行这两步，而不是直接把帧缓冲区写入 0xF7 刷新。
            // 两步各自通过 cascade_fast_mode1_init 完成自己的硬件复位，
            // 对应参考实现中 EPD_FastMode1Init 每次调用都会先做一次硬件复位。
            self.cascade_display_clear(cascade)?;
            return self.cascade_fast_activate(cascade);
        }

        // init
        self.interface.reset(&mut self.delay)?;
        self.interface.busy_wait();
        Command::SoftReset.execute(&mut self.interface)?;
        self.interface.busy_wait();

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
        #[cfg(not(feature = "use_red"))]
        {
            // 局刷模式依赖 RAM(RED) 中保存与当前图像一致的基准图（"ghost" 缓冲区），
            // 否则局刷会读取到未定义内容。参考单芯片实现 EPD_SetRAMValue_BaseMap，
            // 其注释强调"这个函数是必要的，请不要删除！！！"。写完整窗口后地址
            // 计数器会回绕到起始位置，因此无需重新寻址即可紧接着写第二个平面。
            Command::WriteRamRed.execute(&mut self.interface)?;
            self.interface.send_data(self.black_buffer)?;
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

    /// 级联面板中一颗芯片自己的 RAM X 地址空间在帧缓冲区中的字节列起点。
    /// 每颗 SSD1683 只拥有 `chip_width / 8` 个字节列（例如 400px → 0..=49），
    /// 因此从芯片的帧缓冲区列号必须减去该偏移才能作为它自己的 RAM X 地址。
    fn chip_col_offset(&self, chip: Chip) -> u8 {
        match (chip, self.config.cascade) {
            (Chip::Slave, Some(cascade)) => (cascade.chip_width / 8) as u8,
            _ => 0,
        }
    }

    /// 为级联面板中的一颗芯片设置 RAM 寻址窗口（X/Y 起止位置与地址计数器）。
    /// `col_start`/`col_end` 是相对整个帧缓冲区（而非单颗芯片）的字节列范围，
    /// 在写入 RAM X 相关寄存器前会先转换为该芯片自己的本地地址。
    fn address_cascade_chip_ram(
        &mut self,
        chip: Chip,
        col_start: u8,
        col_end: u8,
        y_start: u16,
        y_end: u16,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        // 每颗芯片的 RAM X 地址都从 0 开始，因此这里必须把帧缓冲区列号
        // 转换为芯片本地列号，否则从芯片会被寻址到自身范围之外
        // （参考实现给从芯片写的是 0xC4 ← 0x31/0x00，即本地的 49..0）。
        let offset = self.chip_col_offset(chip);
        let local_start = col_start.saturating_sub(offset);
        let local_end = col_end.saturating_sub(offset);

        // 两颗芯片在物理面板上左右镜像安装，其中一颗的地址递增方向与
        // 帧缓冲区的列顺序相反，因此 X 起止位置互换、X 方向改为递减。
        // 经实机验证：offset=0x00（此处标记为 Master）的芯片是需要
        // 镜像（递减）寻址的那一颗，offset=0x80（Slave）按正常方向寻址。
        let (mode, x_start, x_end, x_addr) = match chip {
            Chip::Master => (
                DataEntryMode::DecrementXIncrementY,
                local_end,
                local_start,
                local_end,
            ),
            Chip::Slave => (
                DataEntryMode::IncrementYIncrementX,
                local_start,
                local_end,
                local_start,
            ),
        };
        Command::DataEntryMode(mode, IncrementAxis::Horizontal)
            .execute_on(&mut self.interface, chip)?;
        Command::StartEndXPosition(x_start, x_end).execute_on(&mut self.interface, chip)?;
        Command::StartEndYPosition(y_start, y_end).execute_on(&mut self.interface, chip)?;
        Command::XAddress(x_addr).execute_on(&mut self.interface, chip)?;
        Command::YAddress(y_start).execute_on(&mut self.interface, chip)?;
        self.interface.busy_wait();
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
        self.address_cascade_chip_ram(chip, col_start, col_end, y_start, y_end)?;

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

    /// 为级联面板中的一颗芯片寻址并将其指定 RAM 平面窗口填充为常数 `value`。
    fn fill_cascade_chip_plane(
        &mut self,
        chip: Chip,
        region: &DirtyRect,
        write_cmd: Command,
        value: u8,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        self.address_cascade_chip_ram(
            chip,
            region.min_byte_col,
            region.max_byte_col,
            region.min_y,
            region.max_y,
        )?;

        let chunk = [value; 32];
        let total_bytes = (region.max_byte_col - region.min_byte_col + 1) as usize
            * (region.max_y - region.min_y + 1) as usize;
        write_cmd.execute_on(&mut self.interface, chip)?;
        let mut remaining = total_bytes;
        while remaining > 0 {
            let n = remaining.min(chunk.len());
            self.interface.send_data(&chunk[..n])?;
            remaining -= n;
        }
        Ok(())
    }

    /// 级联双芯片面板的空白全刷：两颗芯片的 RAM(BW) 填充为 `0xFF`（白），
    /// RAM(RED) 填充为 `0x00`，然后触发一次完整刷新（0xF7）。
    /// 对应参考实现 `EPD_Display_Clear` + `EPD_Update`。级联面板上真正显示
    /// 图像内容始终是通过后续的快刷/局刷完成的（参考实现从不在完整刷新中
    /// 直接写入实际图像内容），因此这里的数据是固定的空白图案，而非帧缓冲区。
    fn cascade_display_clear(
        &mut self,
        cascade: CascadeConfig,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        self.cascade_fast_mode1_init()?;

        let chip_width_bytes = (cascade.chip_width / 8) as u8;
        let y_end = self.config.height.saturating_sub(1);
        for (chip, col_start, col_end) in [
            (Chip::Master, 0u8, chip_width_bytes - 1),
            (Chip::Slave, chip_width_bytes, chip_width_bytes * 2 - 1),
        ] {
            let region = DirtyRect {
                min_byte_col: col_start,
                max_byte_col: col_end,
                min_y: 0,
                max_y: y_end,
            };
            self.fill_cascade_chip_plane(chip, &region, Command::WriteRamBW, 0xFF)?;
            self.fill_cascade_chip_plane(chip, &region, Command::WriteRamRed, 0x00)?;
        }
        Command::DisplayUpdateControl2(0xF7).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();

        // 全刷之后把 RAM(RED) 置为 0xFF（白），使其与屏幕当前的空白状态一致。
        // 局刷是以 RAM(RED) 作为"旧图"、RAM(BW) 作为"新图"做差分的，若 RAM(RED)
        // 仍是全刷时写入的 0x00（黑），后续局刷的起点就与屏幕实际内容不符。
        // 对应参考实现 EPD_Clear_R26A6H（5.79_PWR / 5.79_key 在全刷后调用）。
        for (chip, col_start, col_end) in [
            (Chip::Master, 0u8, chip_width_bytes - 1),
            (Chip::Slave, chip_width_bytes, chip_width_bytes * 2 - 1),
        ] {
            let region = DirtyRect {
                min_byte_col: col_start,
                max_byte_col: col_end,
                min_y: 0,
                max_y: y_end,
            };
            self.fill_cascade_chip_plane(chip, &region, Command::WriteRamRed, 0xFF)?;
        }
        Ok(())
    }

    /// 级联双芯片面板的快刷初始化：先做一次硬件复位 + 软复位，再读取内置
    /// 温度传感器并写入自定义温度寄存器值以选择对应 LUT。对应参考实现
    /// `EPD_FastMode1Init`（该函数每次调用都以硬件复位开头）。级联双芯片
    /// 面板依赖各自的 OTP 默认参数（栅极数、电压等），因此跳过
    /// DriverOutputControl/DisplayUpdateControl1/BorderWaveform(0x05)。
    fn cascade_fast_mode1_init(
        &mut self,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        self.interface.reset(&mut self.delay)?;
        self.interface.busy_wait();
        Command::SoftReset.execute(&mut self.interface)?;
        self.interface.busy_wait();

        Command::ReadTemperatureSensor(0x80).execute(&mut self.interface)?;
        Command::DisplayUpdateControl2(0xB1).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();

        Command::WriteTemperatureRegister(0x64, 0x00).execute(&mut self.interface)?;

        Command::DisplayUpdateControl2(0x91).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();

        Command::BorderWaveform(0x03).execute(&mut self.interface)?;
        self.interface.busy_wait();
        Ok(())
    }

    /// 级联双芯片面板的快刷：先执行 `cascade_fast_mode1_init`，写入实际帧内容
    /// 到两颗芯片的 RAM(BW)，然后触发一次快速刷新（0xC7）。
    /// 对应参考实现 `EPD_FastMode1Init` + `EPD_Display` + `EPD_FastUpdate`。
    fn cascade_fast_activate(
        &mut self,
        cascade: CascadeConfig,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        self.cascade_fast_mode1_init()?;
        self.write_cascade_frame(cascade)?;
        Command::DisplayUpdateControl2(0xC7).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();
        Ok(())
    }

    fn update_fast(&mut self) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        info!("graphics update fast");

        if let Some(cascade) = self.config.cascade {
            return self.cascade_fast_activate(cascade);
        }

        // init
        self.interface.reset(&mut self.delay)?;
        Command::SoftReset.execute(&mut self.interface)?;
        self.interface.busy_wait();

        Command::DisplayUpdateControl1(0x4000).execute(&mut self.interface)?;
        Command::BorderWaveform(0x05).execute(&mut self.interface)?;
        Command::WriteTemperatureSensor(0x6E).execute(&mut self.interface)?;
        Command::DisplayUpdateControl2(0x91).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;

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

        if let Some(cascade) = self.config.cascade {
            // 级联双芯片面板的局刷参考实现（EPD_Display + EPD_PartUpdate）在硬件
            // 复位后不发送 DisplayUpdateControl1/BorderWaveform，直接寻址并写入
            // RAM(BW)，沿用快刷阶段（cascade_fast_mode1_init）已加载的边界波形/
            // LUT 设置。单芯片参考实现（EPD_Dis_Part/EPD_Dis_PartAll）则会在每次
            // 局刷前显式重设这两个寄存器，因此仅对非级联面板保留该步骤。
            //
            // 另外，参考实现在局刷前始终写入**整帧**（EPD_Display），由控制器自己
            // 与 RAM(RED) 中的旧图做差分，而不是只写脏区域子窗口。级联面板上
            // 从未出现过子窗口局刷的用法，因此这里同样写入整帧。
            self.write_cascade_frame(cascade)?;
        } else {
            Command::DisplayUpdateControl1(0x0000).execute(&mut self.interface)?;
            Command::BorderWaveform(0x80).execute(&mut self.interface)?;

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
        // 级联双芯片面板的局刷使用与单芯片面板不同的显示更新控制值：
        // 单芯片参考实现 EPD_Part_Update 使用 0xFF，级联参考实现 EPD_PartUpdate
        // 使用 0xDC。
        let control2 = if self.config.cascade.is_some() {
            0xDC
        } else {
            0xFF
        };
        Command::DisplayUpdateControl2(control2).execute(&mut self.interface)?;
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
