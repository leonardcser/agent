use crate::cursor::{Cursor, Scroll};
use crate::layout::{Anchor, Border, Constraint, FloatRelative, Gutters};
use crate::{BufId, WinId};

#[derive(Clone, Debug)]
pub struct SplitConfig {
    pub region: String,
    pub gutters: Gutters,
}

#[derive(Clone, Debug)]
pub struct FloatConfig {
    pub relative: FloatRelative,
    pub anchor: Anchor,
    pub row: i32,
    pub col: i32,
    pub width: Constraint,
    pub height: Constraint,
    pub border: Border,
    pub title: Option<String>,
    pub zindex: u16,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            relative: FloatRelative::Editor,
            anchor: Anchor::NW,
            row: 0,
            col: 0,
            width: Constraint::Pct(80),
            height: Constraint::Pct(50),
            border: Border::Single,
            title: None,
            zindex: 50,
        }
    }
}

#[derive(Clone, Debug)]
pub enum WinConfig {
    Split(SplitConfig),
    Float(FloatConfig),
}

pub struct Window {
    pub(crate) id: WinId,
    pub buf: BufId,
    pub config: WinConfig,
    pub cursor: Cursor,
    pub scroll: Scroll,
    pub focusable: bool,
}

impl Window {
    pub(crate) fn new(id: WinId, buf: BufId, config: WinConfig) -> Self {
        Self {
            id,
            buf,
            config,
            cursor: Cursor::new(),
            scroll: Scroll::default(),
            focusable: true,
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    pub fn is_float(&self) -> bool {
        matches!(self.config, WinConfig::Float(_))
    }

    pub fn is_split(&self) -> bool {
        matches!(self.config, WinConfig::Split(_))
    }

    pub fn zindex(&self) -> u16 {
        match &self.config {
            WinConfig::Float(f) => f.zindex,
            WinConfig::Split(_) => 0,
        }
    }

    pub fn title(&self) -> Option<&str> {
        match &self.config {
            WinConfig::Float(f) => f.title.as_deref(),
            WinConfig::Split(_) => None,
        }
    }

    pub fn set_title(&mut self, title: Option<String>) {
        if let WinConfig::Float(ref mut f) = self.config {
            f.title = title;
        }
    }
}
