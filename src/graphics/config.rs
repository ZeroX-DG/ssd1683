#[derive(Debug, Clone, Copy)]
pub enum Rotation {
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// 级联双芯片面板的配置：两颗 SSD1683 控制器左右并排驱动同一块面板，
/// 各自负责 `chip_width` 像素宽的区域（从芯片相对主芯片左右镜像）。
#[derive(Debug, Clone, Copy)]
pub struct CascadeConfig {
    pub(crate) chip_width: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) rotation: Rotation,
    pub(crate) cascade: Option<CascadeConfig>,
}

impl Config {
    /// 创建单芯片面板的配置，`width`/`height` 为像素分辨率。
    /// `width` 必须是 8 的倍数。
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            rotation: Rotation::Rotate0,
            cascade: None,
        }
    }

    pub fn with_rotation(self, rotation: Rotation) -> Self {
        Self { rotation, ..self }
    }

    /// 配置为级联双芯片面板：主芯片驱动左半部分，从芯片驱动右半部分
    /// （左右镜像），每颗芯片负责 `chip_width` 像素宽的区域，
    /// 面板总宽度为 `2 * chip_width`。`chip_width` 必须是 8 的倍数。
    pub fn with_cascade(self, chip_width: u16) -> Self {
        Self {
            width: chip_width * 2,
            cascade: Some(CascadeConfig { chip_width }),
            ..self
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new(400, 300)
    }
}
