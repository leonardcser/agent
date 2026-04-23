/// Buffer handle. IDs below `LUA_BUF_ID_BASE` are minted by the Rust
/// side via `Ui::buf_create`; IDs at or above are minted by plugin
/// code via `smelt.buf.create`. The split is by contract, not
/// enforcement — `Ui::buf_create_with_id` still refuses to overwrite,
/// so a collision surfaces as a loud notify rather than silent data
/// loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufId(pub u64);

/// Smallest id a plugin-side `smelt.buf.create` will mint. Keeps
/// Lua buffers in a disjoint range from Rust's sequential allocator.
pub const LUA_BUF_ID_BASE: u64 = 1 << 32;

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
