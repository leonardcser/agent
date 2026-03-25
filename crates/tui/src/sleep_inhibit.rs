//! Prevent the system from sleeping while the agent is working.
//!
//! - macOS: spawns `caffeinate -i -w <pid>` (prevent idle sleep, auto-release on exit)
//! - Linux: spawns `systemd-inhibit` (prevent idle and suspend)
//! - Windows: calls `SetThreadExecutionState`

pub(crate) struct SleepInhibitor {
    #[cfg(not(target_os = "windows"))]
    child: Option<std::process::Child>,
    #[cfg(target_os = "windows")]
    active: bool,
}

impl Default for SleepInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SleepInhibitor {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(not(target_os = "windows"))]
            child: None,
            #[cfg(target_os = "windows")]
            active: false,
        }
    }

    /// Prevent the system from idle-sleeping. Idempotent.
    pub(crate) fn acquire(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            if self.child.is_some() {
                return;
            }
            self.child = spawn_inhibitor();
        }
        #[cfg(target_os = "windows")]
        {
            if self.active {
                return;
            }
            set_execution_state(true);
            self.active = true;
        }
    }

    /// Allow the system to sleep again. Idempotent.
    pub(crate) fn release(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        #[cfg(target_os = "windows")]
        {
            if self.active {
                set_execution_state(false);
                self.active = false;
            }
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.release();
    }
}

// ── Platform: macOS ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn spawn_inhibitor() -> Option<std::process::Child> {
    let pid = std::process::id().to_string();
    std::process::Command::new("caffeinate")
        .args(["-i", "-w", &pid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

// ── Platform: Linux / other Unix ─────────────────────────────────────────────

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn spawn_inhibitor() -> Option<std::process::Child> {
    // systemd-inhibit holds the lock until the wrapped command exits.
    // idle:sleep blocks both the idle timeout and explicit suspend triggers.
    std::process::Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=agent",
            "--why=Agent working",
            "sleep",
            "infinity",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

// ── Platform: Windows ────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn set_execution_state(prevent_sleep: bool) {
    const ES_CONTINUOUS: u32 = 0x80000000;
    const ES_SYSTEM_REQUIRED: u32 = 0x00000001;

    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    let flags = if prevent_sleep {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    unsafe {
        SetThreadExecutionState(flags);
    }
}
