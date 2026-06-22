/// 边框类型枚举，用于控制 Markdown 表格和代码块的边框样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BorderType {
    /// ASCII 风格: +-----+  | ... |
    Ascii,
    /// Unicode 圆角风格: ┌──────┐  │ ... │
    Rounded,
}

impl BorderType {
    /// 获取左上角字符
    pub fn top_left(self) -> char {
        match self {
            BorderType::Ascii => '+',
            BorderType::Rounded => '┌',
        }
    }

    /// 获取右上角字符
    pub fn top_right(self) -> char {
        match self {
            BorderType::Ascii => '+',
            BorderType::Rounded => '┐',
        }
    }

    /// 获取左下角字符
    pub fn bottom_left(self) -> char {
        match self {
            BorderType::Ascii => '+',
            BorderType::Rounded => '└',
        }
    }

    /// 获取右下角字符
    pub fn bottom_right(self) -> char {
        match self {
            BorderType::Ascii => '+',
            BorderType::Rounded => '┘',
        }
    }

    /// 获取水平线字符
    pub fn horizontal(self) -> char {
        match self {
            BorderType::Ascii => '-',
            BorderType::Rounded => '─',
        }
    }

    /// 获取垂直线字符
    pub fn vertical(self) -> char {
        match self {
            BorderType::Ascii => '|',
            BorderType::Rounded => '│',
        }
    }

    /// Convert to ratatui border set for use with `Block::border_set`.
    pub fn ratatui_set(self) -> ratatui::symbols::border::Set<'static> {
        match self {
            BorderType::Ascii => ratatui::symbols::border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            },
            BorderType::Rounded => ratatui::symbols::border::Set {
                top_left: "┌",
                top_right: "┐",
                bottom_left: "└",
                bottom_right: "┘",
                vertical_left: "│",
                vertical_right: "│",
                horizontal_top: "─",
                horizontal_bottom: "─",
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BorderType::Ascii => "ascii",
            BorderType::Rounded => "rounded",
        }
    }
}

impl Default for BorderType {
    fn default() -> Self {
        BorderType::Ascii
    }
}
