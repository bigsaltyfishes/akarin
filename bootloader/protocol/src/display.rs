#[derive(Debug, Clone, Copy)]
pub struct ColorMode {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: Option<u8>,
}

impl ColorMode {
    pub const RGB: Self = Self {
        red: 0,
        green: 1,
        blue: 2,
        alpha: None,
    };

    pub const RGBA: Self = Self {
        red: 0,
        green: 1,
        blue: 2,
        alpha: Some(3),
    };

    pub const BGR: Self = Self {
        red: 2,
        green: 1,
        blue: 0,
        alpha: None,
    };

    pub const BGRA: Self = Self {
        red: 2,
        green: 1,
        blue: 0,
        alpha: Some(3),
    };

    pub const ARGB: Self = Self {
        red: 1,
        green: 2,
        blue: 3,
        alpha: Some(0),
    };

    pub const ABGR: Self = Self {
        red: 3,
        green: 2,
        blue: 1,
        alpha: Some(0),
    };

    pub const BLACK_WHITE: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: None,
    };
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pitch: u32,
    pub color_mode: ColorMode,
}

impl DisplayMode {
    pub fn new(width: u32, height: u32, depth: u32, pitch: u32, color_mode: ColorMode) -> Self {
        assert!(depth > 0, "Depth must be greater than 0");
        assert!(depth <= 32, "Depth must be less than or equal to 32");
        assert_eq!(depth % 8, 0, "Depth must be a multiple of 8");
        Self {
            width,
            height,
            depth,
            pitch,
            color_mode,
        }
    }
}
