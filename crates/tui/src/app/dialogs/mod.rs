//! Per-dialog open/close logic for builtin floats. Each file (resume,
//! permissions, ps, rewind, export, help, confirm, question, agents,
//! lua_dialog) builds its `PanelSpec`s and registers window callbacks
//! that push `AppOp`s on Submit / Dismiss / custom keymaps. State
//! shared across callbacks lives in `Rc<RefCell<_>>`, captured by the
//! closures.

pub mod agents;
pub mod confirm;
pub mod export;
pub mod help;
pub mod lua_dialog;
pub mod permissions;
pub mod ps;
pub mod question;
pub mod resume;
pub mod rewind;
