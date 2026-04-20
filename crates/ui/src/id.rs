#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinId(pub u64);

impl BufId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl WinId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for BufId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buf:{}", self.0)
    }
}

impl std::fmt::Display for WinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win:{}", self.0)
    }
}
