//! Dialog data types shared between the render layer and the
//! compositor-driven dialogs in `app/dialogs/`.

pub(crate) mod confirm;

use crate::app::AgentToolEntry;
use std::sync::{Arc, Mutex};

/// Snapshot of a tracked agent's state, published by the main loop
/// and consumed by the agents dialog.
#[derive(Clone)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub prompt: Arc<String>,
    pub tool_calls: Vec<AgentToolEntry>,
    pub context_tokens: Option<u32>,
    pub cost_usd: f64,
}

/// Shared, live-updating list of agent snapshots.
pub type SharedSnapshots = Arc<Mutex<Vec<AgentSnapshot>>>;
