mod agent;
pub mod auth;
pub mod clock;
pub mod env;
pub mod host;
pub mod image;
pub mod log;
pub mod opener;

pub mod paths;

pub mod provider;
pub mod redact;
mod request_log;
pub(crate) mod result_dedup;
pub(crate) mod skills;
pub mod tools;
pub(crate) mod trim;

pub use host::{HostCall, HostRequestDecision, PreparedRequestMessages};
use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use paths::{config_dir, data_dir, home_dir, state_dir};

/// Re-export so non-engine crates (the TUI) can store an HTTP client
/// without depending on `reqwest` directly.
pub use reqwest::Client as HttpClient;

pub use provider::EngineProvider;
pub use skills::SkillLoader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemPromptBehavior {
    Interactive,
    Autonomous,
}

/// Whether the frontend services callbacks emitted by the engine.
/// Disabled callbacks fail immediately so the engine can use their fallback
/// without waiting for a frontend that only consumes protocol events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCallbacks {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemPromptCapabilities {
    pub tool_calling: bool,
}

impl SystemPromptCapabilities {
    pub fn from_tool_calling(tool_calling: bool) -> Self {
        Self { tool_calling }
    }
}

struct PromptContext<'a> {
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub fn build_system_prompt(
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    let ctx = PromptContext {
        behavior,
        capabilities,
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

pub fn assemble_system_prompt(
    system_prompt_override: Option<&str>,
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    system_prompt_override
        .map(str::to_owned)
        .unwrap_or_else(|| {
            build_system_prompt(behavior, capabilities, extra_instructions, skill_section)
        })
}

fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let rendered = render_builtin_prompt_template(include_str!("prompts/system.txt"), ctx);

    let mut result = collapse_blank_lines(&rendered);
    for section in [ctx.skills_section, ctx.extra_instructions]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|section| !section.is_empty())
    {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(section);
    }
    result
}

fn render_builtin_prompt_template(template: &str, ctx: &PromptContext<'_>) -> String {
    struct Frame {
        condition: bool,
    }

    let mut out = String::with_capacity(template.len());
    let mut stack: Vec<Frame> = Vec::new();

    for line in template.lines() {
        let trimmed = line.trim();
        match trimmed {
            "{% if tools_enabled %}" => {
                stack.push(Frame {
                    condition: ctx.capabilities.tool_calling,
                });
            }
            "{% if behavior == \"autonomous\" %}" => {
                stack.push(Frame {
                    condition: ctx.behavior == SystemPromptBehavior::Autonomous,
                });
            }
            "{% else %}" => {
                if let Some(frame) = stack.last_mut() {
                    frame.condition = !frame.condition;
                }
            }
            "{% endif %}" => {
                stack.pop();
            }
            _ if stack.iter().all(|frame| frame.condition) => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }

    out
}

fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut blank_count = 0u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    smelt_buffer::text::trim_whitespace(&result).to_string()
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;

    fn prompt(behavior: SystemPromptBehavior, tool_calling: bool) -> String {
        build_system_prompt(
            behavior,
            SystemPromptCapabilities::from_tool_calling(tool_calling),
            Some("Extra instructions."),
            Some("# Skills\nLoaded skill."),
        )
    }

    #[test]
    fn interactive_prompt_renders_collaborator_behavior_and_tools() {
        let rendered = prompt(SystemPromptBehavior::Interactive, true);
        assert!(rendered.contains("# Tools"));
        assert!(rendered.contains("You and the user are collaborators"));
        assert!(!rendered.contains("You are running autonomously"));
        assert!(rendered.contains("# Skills\nLoaded skill."));
        assert!(rendered.ends_with("Extra instructions."));
        assert!(!rendered.contains("{%"));
    }

    #[test]
    fn autonomous_prompt_can_omit_tools() {
        let rendered = prompt(SystemPromptBehavior::Autonomous, false);
        assert!(!rendered.contains("# Tools"));
        assert!(rendered.contains("You are running autonomously"));
        assert!(!rendered.contains("You and the user are collaborators"));
        assert!(!rendered.contains("{%"));
    }
}

pub struct EngineConfig {
    pub instructions: Option<String>,
    /// When set, replaces the built-in system prompt template entirely.
    pub system_prompt_override: Option<String>,
    pub system_prompt_behavior: SystemPromptBehavior,
    pub host_callbacks: HostCallbacks,
    pub cwd: PathBuf,
    /// Pre-rendered "# Skills" block injected into the system prompt.
    /// Built once on startup from the [`SkillLoader`] and refreshed through
    /// [`protocol::UiCommand::UpdateAgentProjectContext`]. The loader itself
    /// lives on `Core::skills` for tool execution.
    pub skill_section: Option<String>,
    /// Source of monotonic + wall-clock time. Production uses
    /// [`clock::RealClock`]; deterministic-simulation harnesses inject a
    /// [`clock::VirtualClock`] so scenarios can replay against advanced time.
    pub clock: Arc<dyn clock::Clock>,
}

impl EngineConfig {
    pub fn new(cwd: PathBuf, clock: Arc<dyn clock::Clock>) -> Self {
        Self {
            instructions: None,
            system_prompt_override: None,
            system_prompt_behavior: SystemPromptBehavior::Interactive,
            host_callbacks: HostCallbacks::Enabled,
            cwd,
            skill_section: None,
            clock,
        }
    }

    fn install_project_context(&mut self, context: &protocol::AgentProjectContext) {
        self.cwd.clone_from(&context.cwd);
        self.instructions.clone_from(&context.instructions);
        self.skill_section.clone_from(&context.skill_section);
        self.system_prompt_override
            .clone_from(&context.system_prompt_override);
    }
}

/// Ordered output from the engine task. Host callbacks share the protocol-event
/// channel so frontends always apply preceding history updates before a callback.
pub enum EngineOutput {
    Event(EngineEvent),
    HostCall(HostCall),
}

#[derive(Clone)]
pub(crate) struct EngineEventSender {
    output_tx: mpsc::UnboundedSender<EngineOutput>,
}

impl EngineEventSender {
    pub(crate) fn send(&self, event: EngineEvent) -> Result<(), Box<EngineEvent>> {
        self.output_tx
            .send(EngineOutput::Event(event))
            .map_err(|error| match error.0 {
                EngineOutput::Event(event) => Box::new(event),
                EngineOutput::HostCall(_) => unreachable!("sent an engine event"),
            })
    }
}

#[derive(Clone)]
pub(crate) struct HostCallSender {
    output_tx: mpsc::UnboundedSender<EngineOutput>,
    enabled: bool,
}

impl HostCallSender {
    pub(crate) fn send(&self, call: HostCall) -> Result<(), Box<HostCall>> {
        if !self.enabled {
            return Err(Box::new(call));
        }
        self.output_tx
            .send(EngineOutput::HostCall(call))
            .map_err(|error| match error.0 {
                EngineOutput::HostCall(call) => Box::new(call),
                EngineOutput::Event(_) => unreachable!("sent a host call"),
            })
    }
}

pub(crate) fn output_channel(
    host_callbacks: HostCallbacks,
) -> (
    EngineEventSender,
    HostCallSender,
    mpsc::UnboundedReceiver<EngineOutput>,
) {
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    (
        EngineEventSender {
            output_tx: output_tx.clone(),
        },
        HostCallSender {
            output_tx,
            enabled: host_callbacks == HostCallbacks::Enabled,
        },
        output_rx,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineDisconnected;

pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: EngineEventSender,
    output_rx: mpsc::UnboundedReceiver<EngineOutput>,
}

impl EngineHandle {
    pub fn send(&self, cmd: UiCommand) {
        let _ = self.try_send(cmd);
    }

    pub fn try_send(&self, cmd: UiCommand) -> Result<(), EngineDisconnected> {
        self.cmd_tx.send(cmd).map_err(|_| EngineDisconnected)
    }

    /// Receive the next ordered engine output. Returns `None` when the engine's
    /// command receiver disconnects, after draining any output already queued.
    pub async fn recv_output(&mut self) -> Option<EngineOutput> {
        tokio::select! {
            biased;
            output = self.output_rx.recv() => output,
            _ = self.cmd_tx.closed() => None,
        }
    }

    pub fn try_recv_output(&mut self) -> Result<EngineOutput, mpsc::error::TryRecvError> {
        match self.output_rx.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) if self.cmd_tx.is_closed() => {
                Err(mpsc::error::TryRecvError::Disconnected)
            }
            result => result,
        }
    }

    /// Receive only protocol events, dropping unsupported host callbacks.
    /// Frontends that implement host callbacks should use `recv_output`.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        while let Some(output) = self.recv_output().await {
            if let EngineOutput::Event(event) = output {
                return Some(event);
            }
        }
        None
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        loop {
            match self.try_recv_output()? {
                EngineOutput::Event(event) => return Ok(event),
                EngineOutput::HostCall(_) => {}
            }
        }
    }

    pub fn injector(&self) -> EventInjector {
        EventInjector {
            event_tx: self.event_tx.clone(),
        }
    }

    /// Build a handle backed by caller-owned channels, with no agent task.
    /// Used by the storybook harness to drive UI state without a real engine.
    pub fn for_test() -> (
        Self,
        mpsc::UnboundedReceiver<UiCommand>,
        EngineOutputInjector,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, host_tx, output_rx) = output_channel(HostCallbacks::Enabled);
        let handle = EngineHandle {
            cmd_tx,
            event_tx: event_tx.clone(),
            output_rx,
        };
        (handle, cmd_rx, EngineOutputInjector { event_tx, host_tx })
    }
}

#[derive(Clone)]
pub struct EngineOutputInjector {
    event_tx: EngineEventSender,
    host_tx: HostCallSender,
}

impl EngineOutputInjector {
    pub fn send(&self, event: EngineEvent) -> Result<(), Box<EngineEvent>> {
        self.event_tx.send(event)
    }

    pub fn send_host_call(&self, call: HostCall) -> Result<(), Box<HostCall>> {
        self.host_tx.send(call)
    }
}

#[derive(Clone)]
pub struct EventInjector {
    event_tx: EngineEventSender,
}

impl EventInjector {
    pub fn inject_tool_output(
        &self,
        invocation_id: protocol::InvocationId,
        call_id: String,
        line: String,
    ) {
        let _ = self.event_tx.send(EngineEvent::ToolOutput {
            invocation_id,
            call_id,
            line,
        });
    }
}

/// Start the engine. Must be called from within a tokio runtime.
pub fn start(config: EngineConfig, dispatcher: Box<dyn tools::ToolDispatcher>) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, host_tx, output_rx) = output_channel(config.host_callbacks);
    let handle = EngineHandle {
        cmd_tx,
        event_tx: event_tx.clone(),
        output_rx,
    };

    tokio::spawn(agent::engine_task(
        config, dispatcher, cmd_rx, event_tx, host_tx,
    ));

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- render_system_prompt ----

    #[test]
    fn render_system_prompt_includes_static_sections_and_extra_instructions() {
        let ctx = PromptContext {
            behavior: SystemPromptBehavior::Interactive,
            capabilities: SystemPromptCapabilities::from_tool_calling(true),
            skills_section: None,
            extra_instructions: Some("MARK-EXTRA-7384"),
        };
        let out = render_system_prompt(&ctx);
        assert!(out.contains("# Managed worktrees"));
        assert!(out.contains("MARK-EXTRA-7384"));
    }

    #[test]
    fn render_system_prompt_collapses_runs_of_blank_lines() {
        let ctx = PromptContext {
            behavior: SystemPromptBehavior::Interactive,
            capabilities: SystemPromptCapabilities::from_tool_calling(true),
            skills_section: None,
            extra_instructions: None,
        };
        let out = render_system_prompt(&ctx);
        // No more than two consecutive newlines anywhere.
        assert!(!out.contains("\n\n\n"));
    }

    #[test]
    fn system_prompt_behavior_switches_interactive_and_autonomous_text() {
        let interactive = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );
        let autonomous = build_system_prompt(
            SystemPromptBehavior::Autonomous,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );

        assert!(interactive.contains("You and the user are collaborators"));
        assert!(!interactive.contains("You are running autonomously"));
        assert!(autonomous.contains("You are running autonomously"));
        assert!(!autonomous.contains("You and the user are collaborators"));
    }

    #[test]
    fn system_prompt_omits_tool_guidance_when_tools_disabled() {
        let with_tools = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );
        let without_tools = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(false),
            None,
            None,
        );

        assert!(with_tools.contains("# Tools"));
        assert!(with_tools.contains("read_file"));
        assert!(with_tools.contains("<attached_file ... tool=\"read_file\" already_read=\"true\">"));
        assert!(with_tools.contains("<command_output ... executed_by=\"smelt\">"));
        assert!(with_tools.contains("<skill ... included_by=\"smelt\">"));
        assert!(!without_tools.contains("# Tools"));
        assert!(!without_tools.contains("read_file"));
        assert!(without_tools.contains("# Code"));
    }

    #[test]
    fn system_prompt_is_byte_stable_for_session_inputs() {
        // The same (skills, instructions) must produce identical bytes
        // every time so /mode and cwd switches don't bust the cache.
        let a = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        let b = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        let c = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn system_prompt_contains_no_time_or_date_substitutions() {
        // Time-varying values (current date, current time, wall-clock
        // timestamps) would silently rotate the cache key. pi-mono embeds
        // `Current date:`; we explicitly don't. Scan the rendered prompt
        // for tokens that would suggest a future regression sneaks one in.
        let prompt = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        let lower = prompt.to_ascii_lowercase();
        for needle in [
            "current date",
            "current time",
            "today is",
            "current datetime",
            "now is",
        ] {
            assert!(
                !lower.contains(needle),
                "system prompt must not embed a time-varying substring (found `{needle}` in rendered output)",
            );
        }
        // Also pin: rendering twice produces identical bytes - the template
        // itself must not call into any time-of-day source.
        let a = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        let b = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        assert_eq!(
            a, b,
            "system prompt drifted between calls with identical inputs",
        );
    }

    // ---- EngineHandle / EventInjector ----

    #[tokio::test]
    async fn for_test_returns_paired_channels_and_send_recv_works() {
        let (mut handle, mut cmd_rx, event_tx) = EngineHandle::for_test();

        handle.send(UiCommand::Cancel);
        let cmd = cmd_rx.recv().await.unwrap();
        assert!(matches!(cmd, UiCommand::Cancel));

        let _ = event_tx.send(EngineEvent::ToolOutput {
            invocation_id: protocol::InvocationId::new(1),
            call_id: "id".into(),
            line: "x".into(),
        });
        match handle.recv().await.unwrap() {
            EngineEvent::ToolOutput { call_id, line, .. } => {
                assert_eq!(call_id, "id");
                assert_eq!(line, "x");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn recv_detects_engine_disconnect_while_event_injectors_remain() {
        let (mut handle, cmd_rx, _output_injector) = EngineHandle::for_test();
        drop(cmd_rx);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv())
                .await
                .expect("engine disconnect should wake receivers")
                .is_none()
        );
        assert!(matches!(
            handle.try_recv_output(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn engine_outputs_preserve_event_and_host_call_order() {
        let (mut handle, _cmd_rx, output_injector) = EngineHandle::for_test();
        let _ = output_injector.send(EngineEvent::Ready);
        let (reply, _reply_rx) = tokio::sync::oneshot::channel();
        assert!(output_injector
            .send_host_call(HostCall::PrepareRequest {
                turn_id: 1,
                messages: PreparedRequestMessages::new(Vec::new(), 0),
                estimated_tokens: 0,
                reply,
            })
            .is_ok());

        assert!(matches!(
            handle.recv_output().await,
            Some(EngineOutput::Event(EngineEvent::Ready))
        ));
        assert!(matches!(
            handle.recv_output().await,
            Some(EngineOutput::HostCall(HostCall::PrepareRequest { .. }))
        ));
    }

    #[test]
    fn disabled_host_calls_are_rejected_before_queueing() {
        let (_event_tx, host_tx, mut output_rx) = output_channel(HostCallbacks::Disabled);
        let (reply, _reply_rx) = tokio::sync::oneshot::channel();

        assert!(host_tx
            .send(HostCall::PrepareRequest {
                turn_id: 1,
                messages: PreparedRequestMessages::new(Vec::new(), 0),
                estimated_tokens: 0,
                reply,
            })
            .is_err());
        assert!(matches!(
            output_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn event_injector_forwards_tool_output() {
        let (mut handle, _cmd_rx, _event_tx) = EngineHandle::for_test();
        let injector = handle.injector();
        injector.inject_tool_output(protocol::InvocationId::new(1), "c".into(), "out".into());
        match handle.recv().await.unwrap() {
            EngineEvent::ToolOutput { call_id, line, .. } => {
                assert_eq!(call_id, "c");
                assert_eq!(line, "out");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn try_recv_returns_empty_when_no_events_pending() {
        let (mut handle, _cmd_rx, _event_tx) = EngineHandle::for_test();
        let res = handle.try_recv();
        assert!(matches!(res, Err(mpsc::error::TryRecvError::Empty)));
    }
}
