use crossterm::style::Color;

#[derive(Clone, Debug, Default)]
pub struct HlAttrs {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Debug)]
pub struct HlGroup {
    pub name: String,
    pub attrs: HlAttrs,
}
