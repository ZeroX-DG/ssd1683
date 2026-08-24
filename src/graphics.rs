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
        // MasterActivation 返回（BUSY 拉低）只表示控制器已启动波形，面板像素
        // 本身还在稳定过程中。若立即进入深度睡眠会切断电荷泵，这一帧的图像
        // 就不会真正显示出来。先留出稳定时间再睡眠。
        self.delay.delay_ms(200);
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

    /// 为级联面板中的一颗芯片设置它自己完整的 RAM 寻址窗口。
    ///
    /// 忠实移植参考实现的 `EPD_SetRAMMP`/`EPD_SetRAMMA`（主芯片）与
    /// `EPD_SetRAMSP`/`EPD_SetRAMSA`（从芯片）：两颗芯片都工作在 AM=Y
    /// （列优先）、Y 递减模式，主芯片 X 递增（0x11 ← 0x05），从芯片 X 递减
    /// （0x91 ← 0x04）。每颗芯片的 RAM X 都是它自己的 0..=chip_bytes-1。
    fn address_cascade_chip(
        &mut self,
        chip: Chip,
        chip_bytes: u8,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        let y_last = self.config.height.saturating_sub(1);
        let (mode, x_start, x_end, x_addr) = match chip {
            Chip::Master => (DataEntryMode::IncrementXDecrementY, 0, chip_bytes - 1, 0),
            Chip::Slave => (
                DataEntryMode::DecrementXDecrementY,
                chip_bytes - 1,
                0,
                chip_bytes - 1,
            ),
        };
        Command::DataEntryMode(mode, IncrementAxis::Vertical)
            .execute_on(&mut self.interface, chip)?;
        Command::StartEndXPosition(x_start, x_end).execute_on(&mut self.interface, chip)?;
        Command::StartEndYPosition(y_last, 0).execute_on(&mut self.interface, chip)?;
        Command::XAddress(x_addr).execute_on(&mut self.interface, chip)?;
        Command::YAddress(y_last).execute_on(&mut self.interface, chip)?;
        self.interface.busy_wait();
        Ok(())
    }

    /// 一颗芯片在帧缓冲区中负责的字节列范围（闭区间）。
    ///
    /// 对应参考实现 `EPD_Display`：主芯片循环先跑完 `tempcol` 0..=chip_bytes-1，
    /// 从芯片循环**不重置** `tempcol`/`templine`，紧接着继续读取后半段列。
    /// 因此主芯片驱动帧缓冲区的左半屏，从芯片驱动右半屏。
    fn cascade_chip_cols(&self, chip: Chip, chip_bytes: u8) -> (usize, usize) {
        let stride = (self.config.width / 8) as usize;
        match chip {
            Chip::Master => (0, chip_bytes as usize - 1),
            Chip::Slave => (chip_bytes as usize, stride - 1),
        }
    }

    /// 将帧缓冲区中属于 `chip` 的半屏写入它的 RAM(BW)。
    ///
    /// 逐字节对应参考实现 `EPD_Display` 的取数循环：
    /// ```c
    /// tempOriginal = *(ImageBW + templine * Source_BYTES * 2 + tempcol);
    /// templine++; if (templine >= Gate_BITS) { tempcol++; templine = 0; }
    /// EPD_WR_DATA8(tempOriginal);
    /// ```
    /// 即：列优先（AM=Y），列号与行号均**递增**，字节按原样发送（不做位反转）。
    /// 面板所需的 180° 方向由参考实现在绘制阶段（`Paint_SetPixel`，
    /// `#define Rotation 180`）写入缓冲区时完成，而不在这里的传输循环中处理。
    fn write_cascade_chip_frame(
        &mut self,
        chip: Chip,
        chip_bytes: u8,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        self.address_cascade_chip(chip, chip_bytes)?;
        Command::WriteRamBW.execute_on(&mut self.interface, chip)?;

        let stride = (self.config.width / 8) as usize;
        let height = self.config.height as usize;
        let (col_lo, col_hi) = self.cascade_chip_cols(chip, chip_bytes);

        let mut chunk = [0u8; 256];
        let mut filled = 0;
        for col in col_lo..=col_hi {
            for row in 0..height {
                chunk[filled] = self.black_buffer[row * stride + col];
                filled += 1;
                if filled == chunk.len() {
                    self.interface.send_data(&chunk)?;
                    filled = 0;
                }
            }
        }
        if filled > 0 {
            self.interface.send_data(&chunk[..filled])?;
        }
        Ok(())
    }

    /// 寻址并写入级联双芯片面板的整帧黑白 RAM。
    fn write_cascade_frame(
        &mut self,
        cascade: CascadeConfig,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        let chip_bytes = (cascade.chip_width / 8) as u8;
        self.write_cascade_chip_frame(Chip::Master, chip_bytes)?;
        self.write_cascade_chip_frame(Chip::Slave, chip_bytes)?;
        Ok(())
    }

    /// 只设置一颗芯片的 RAM 地址计数器（0x4E/0x4F，从芯片 0xCE/0xCF），
    /// 不重设数据输入模式与 X/Y 窗口。对应参考实现的 `EPD_SetRAMMA` /
    /// `EPD_SetRAMSA`。
    ///
    /// 与 [`Self::address_cascade_chip`]（对应 `EPD_SetRAMMP` + `EPD_SetRAMMA`，
    /// 会额外发送 0x11/0x44/0x45）的区别很关键：参考实现在一次窗口设置之后
    /// 连续写入多个 RAM 平面时，后续平面只重设地址计数器而**不**重发
    /// 数据输入模式和窗口（见 `EPD_Display_Clear` 中第二次 0x26 之前只调用
    /// `EPD_SetRAMMA`，以及整个 `EPD_Clear_R26A6H`）。
    fn set_cascade_chip_ram_address(
        &mut self,
        chip: Chip,
        chip_bytes: u8,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        let y_last = self.config.height.saturating_sub(1);
        let x_addr = match chip {
            Chip::Master => 0,
            Chip::Slave => chip_bytes - 1,
        };
        Command::XAddress(x_addr).execute_on(&mut self.interface, chip)?;
        Command::YAddress(y_last).execute_on(&mut self.interface, chip)?;
        Ok(())
    }

    /// 将一颗芯片的某个 RAM 平面整体填充为常数 `value`。
    /// 填充值是常数，因此与扫描方向和位序无关。
    ///
    /// `reset_window` 控制寻址方式：`true` 时发送完整的窗口设置
    /// （`EPD_SetRAMMP` + `EPD_SetRAMMA`），`false` 时只重设地址计数器
    /// （`EPD_SetRAMMA`），以匹配参考实现在连续写入多个平面时的行为。
    fn fill_cascade_chip_plane(
        &mut self,
        chip: Chip,
        chip_bytes: u8,
        write_cmd: Command,
        value: u8,
        reset_window: bool,
    ) -> Result<(), Error<SPI::Error, RESET::Error, DC::Error>> {
        if reset_window {
            self.address_cascade_chip(chip, chip_bytes)?;
        } else {
            self.set_cascade_chip_ram_address(chip, chip_bytes)?;
        }

        let chunk = [value; 256];
        let mut remaining = chip_bytes as usize * self.config.height as usize;
        write_cmd.execute_on(&mut self.interface, chip)?;
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

        // 对应 EPD_Display_Clear：每颗芯片先做一次完整窗口设置
        // （SetRAMMP + SetRAMMA）写 RAM(BW)，随后写 RAM(RED) 时只重设
        // 地址计数器（SetRAMMA），不重发数据输入模式与窗口。
        let chip_bytes = (cascade.chip_width / 8) as u8;
        for chip in [Chip::Master, Chip::Slave] {
            self.fill_cascade_chip_plane(chip, chip_bytes, Command::WriteRamBW, 0xFF, true)?;
            self.fill_cascade_chip_plane(chip, chip_bytes, Command::WriteRamRed, 0x00, false)?;
        }
        Command::DisplayUpdateControl2(0xF7).execute(&mut self.interface)?;
        Command::MasterActivation.execute(&mut self.interface)?;
        self.interface.busy_wait();
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
