use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use codex_core::config::Config;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpListToolsResponseEvent;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TurnDiffEvent;
use codex_protocol::parse_command::ParsedCommand;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use rand::Rng;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::history_cell;
use crate::history_cell::CommandOutput;
use crate::history_cell::ExecCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PatchEventType;
use crate::tui::FrameRequester;
// streaming internals are provided by crate::streaming and crate::markdown_stream
use crate::user_approval_widget::ApprovalRequest;
mod interrupts;
use self::interrupts::InterruptManager;
mod agent;
use self::agent::spawn_agent;
use crate::streaming::controller::AppEventHistorySink;
use crate::streaming::controller::StreamController;
use codex_common::approval_presets::ApprovalPreset;
use codex_common::approval_presets::builtin_approval_presets;
use codex_common::model_presets::ModelPreset;
use codex_common::model_presets::builtin_model_presets;
use codex_core::ConversationManager;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol_config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_file_search::FileMatch;
use uuid::Uuid;

// Track information about an in-flight exec command.
struct RunningCommand {
    command: Vec<String>,
    parsed_cmd: Vec<ParsedCommand>,
}

pub(crate) struct ChatWidget {
    app_event_tx: AppEventSender,
    codex_op_tx: UnboundedSender<Op>,
    bottom_pane: BottomPane,
    active_exec_cell: Option<ExecCell>,
    config: Config,
    initial_user_message: Option<UserMessage>,
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    // Stream lifecycle controller
    stream: StreamController,
    running_commands: HashMap<String, RunningCommand>,
    pending_exec_completions: Vec<(Vec<String>, Vec<ParsedCommand>, CommandOutput)>,
    task_complete_pending: bool,
    // Queue of interruptive UI events deferred during an active write cycle
    interrupts: InterruptManager,
    // Whether a redraw is needed after handling the current event
    needs_redraw: bool,
    // Accumulates the current reasoning block text to extract a header
    reasoning_buffer: String,
    // Accumulates full reasoning content for transcript-only recording
    full_reasoning_buffer: String,
    session_id: Option<Uuid>,
    frame_requester: FrameRequester,
}

struct UserMessage {
    text: String,
    image_paths: Vec<PathBuf>,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            image_paths: Vec::new(),
        }
    }
}

fn create_initial_user_message(text: String, image_paths: Vec<PathBuf>) -> Option<UserMessage> {
    if text.is_empty() && image_paths.is_empty() {
        None
    } else {
        Some(UserMessage { text, image_paths })
    }
}

impl ChatWidget {
    #[inline]
    fn mark_needs_redraw(&mut self) {
        self.needs_redraw = true;
    }
    fn flush_answer_stream_with_separator(&mut self) {
        let sink = AppEventHistorySink(self.app_event_tx.clone());
        let _ = self.stream.finalize(true, &sink);
    }
    // --- Small event handlers ---
    fn on_session_configured(&mut self, event: codex_core::protocol::SessionConfiguredEvent) {
        self.bottom_pane
            .set_history_metadata(event.history_log_id, event.history_entry_count);
        self.session_id = Some(event.session_id);
        self.add_to_history(history_cell::new_session_info(&self.config, event, true));
        if let Some(user_message) = self.initial_user_message.take() {
            self.submit_user_message(user_message);
        }
        self.mark_needs_redraw();
    }

    fn on_agent_message(&mut self, message: String) {
        let sink = AppEventHistorySink(self.app_event_tx.clone());
        let finished = self.stream.apply_final_answer(&message, &sink);
        self.handle_if_stream_finished(finished);
        self.mark_needs_redraw();
    }

    fn on_agent_message_delta(&mut self, delta: String) {
        self.handle_streaming_delta(delta);
    }

    fn on_agent_reasoning_delta(&mut self, delta: String) {
        // For reasoning deltas, do not stream to history. Accumulate the
        // current reasoning block and extract the first bold element
        // (between **/**) as the chunk header. Show this header as status.
        self.reasoning_buffer.push_str(&delta);

        if let Some(header) = extract_first_bold(&self.reasoning_buffer) {
            // Update the shimmer header to the extracted reasoning chunk header.
            self.bottom_pane.update_status_header(header);
        } else {
            // Fallback while we don't yet have a bold header: leave existing header as-is.
        }
        self.mark_needs_redraw();
    }

    fn on_agent_reasoning_final(&mut self) {
        // At the end of a reasoning block, record transcript-only content.
        self.full_reasoning_buffer.push_str(&self.reasoning_buffer);
        if !self.full_reasoning_buffer.is_empty() {
            self.add_to_history(history_cell::new_reasoning_block(
                self.full_reasoning_buffer.clone(),
                &self.config,
            ));
        }
        self.reasoning_buffer.clear();
        self.full_reasoning_buffer.clear();
        self.mark_needs_redraw();
    }

    fn on_reasoning_section_break(&mut self) {
        // Start a new reasoning block for header extraction and accumulate transcript.
        self.full_reasoning_buffer.push_str(&self.reasoning_buffer);
        self.full_reasoning_buffer.push_str("\n\n");
        self.reasoning_buffer.clear();
    }

    // Raw reasoning uses the same flow as summarized reasoning

    fn on_task_started(&mut self) {
        self.bottom_pane.clear_ctrl_c_quit_hint();
        self.bottom_pane.set_task_running(true);
        self.stream.reset_headers_for_new_turn();
        self.full_reasoning_buffer.clear();
        self.reasoning_buffer.clear();
        self.mark_needs_redraw();
    }

    fn on_task_complete(&mut self) {
        // If a stream is currently active, finalize only that stream to flush any tail
        // without emitting stray headers for other streams.
        if self.stream.is_write_cycle_active() {
            let sink = AppEventHistorySink(self.app_event_tx.clone());
            let _ = self.stream.finalize(true, &sink);
        }
        // Mark task stopped and request redraw now that all content is in history.
        self.bottom_pane.set_task_running(false);
        self.running_commands.clear();
        self.mark_needs_redraw();
    }

    fn on_token_count(&mut self, token_usage: TokenUsage) {
        self.total_token_usage = add_token_usage(&self.total_token_usage, &token_usage);
        self.last_token_usage = token_usage;
        self.bottom_pane.set_token_usage(
            self.total_token_usage.clone(),
            self.last_token_usage.clone(),
            self.config.model_context_window,
        );
    }

    fn on_error(&mut self, message: String) {
        self.add_to_history(history_cell::new_error_event(message));
        self.bottom_pane.set_task_running(false);
        self.running_commands.clear();
        self.stream.clear_all();
        self.mark_needs_redraw();
    }

    fn on_plan_update(&mut self, update: codex_core::plan_tool::UpdatePlanArgs) {
        self.add_to_history(history_cell::new_plan_update(update));
    }

    fn on_exec_approval_request(&mut self, id: String, ev: ExecApprovalRequestEvent) {
        let id2 = id.clone();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_exec_approval(id, ev),
            |s| s.handle_exec_approval_now(id2, ev2),
        );
    }

    fn on_apply_patch_approval_request(&mut self, id: String, ev: ApplyPatchApprovalRequestEvent) {
        let id2 = id.clone();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_apply_patch_approval(id, ev),
            |s| s.handle_apply_patch_approval_now(id2, ev2),
        );
    }

    fn on_exec_command_begin(&mut self, ev: ExecCommandBeginEvent) {
        self.flush_answer_stream_with_separator();
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_exec_begin(ev), |s| s.handle_exec_begin_now(ev2));
    }

    fn on_exec_command_output_delta(
        &mut self,
        _ev: codex_core::protocol::ExecCommandOutputDeltaEvent,
    ) {
        // TODO: Handle streaming exec output if/when implemented
    }

    fn on_patch_apply_begin(&mut self, event: PatchApplyBeginEvent) {
        self.add_to_history(history_cell::new_patch_event(
            PatchEventType::ApplyBegin {
                auto_approved: event.auto_approved,
            },
            event.changes,
        ));
    }

    fn on_patch_apply_end(&mut self, event: codex_core::protocol::PatchApplyEndEvent) {
        let ev2 = event.clone();
        self.defer_or_handle(
            |q| q.push_patch_end(event),
            |s| s.handle_patch_apply_end_now(ev2),
        );
    }

    fn on_exec_command_end(&mut self, ev: ExecCommandEndEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_exec_end(ev), |s| s.handle_exec_end_now(ev2));
    }

    fn on_mcp_tool_call_begin(&mut self, ev: McpToolCallBeginEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_mcp_begin(ev), |s| s.handle_mcp_begin_now(ev2));
    }

    fn on_mcp_tool_call_end(&mut self, ev: McpToolCallEndEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_mcp_end(ev), |s| s.handle_mcp_end_now(ev2));
    }

    fn on_get_history_entry_response(
        &mut self,
        event: codex_core::protocol::GetHistoryEntryResponseEvent,
    ) {
        let codex_core::protocol::GetHistoryEntryResponseEvent {
            offset,
            log_id,
            entry,
        } = event;
        self.bottom_pane
            .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
    }

    fn on_shutdown_complete(&mut self) {
        self.app_event_tx.send(AppEvent::ExitRequest);
    }

    fn on_turn_diff(&mut self, unified_diff: String) {
        debug!("TurnDiffEvent: {unified_diff}");
    }

    fn on_background_event(&mut self, message: String) {
        debug!("BackgroundEvent: {message}");
    }
    /// Periodic tick to commit at most one queued line to history with a small delay,
    /// animating the output.
    pub(crate) fn on_commit_tick(&mut self) {
        let sink = AppEventHistorySink(self.app_event_tx.clone());
        let finished = self.stream.on_commit_tick(&sink);
        self.handle_if_stream_finished(finished);
    }
    fn is_write_cycle_active(&self) -> bool {
        self.stream.is_write_cycle_active()
    }

    fn flush_interrupt_queue(&mut self) {
        let mut mgr = std::mem::take(&mut self.interrupts);
        mgr.flush_all(self);
        self.interrupts = mgr;
    }

    #[inline]
    fn defer_or_handle(
        &mut self,
        push: impl FnOnce(&mut InterruptManager),
        handle: impl FnOnce(&mut Self),
    ) {
        // Preserve deterministic FIFO across queued interrupts: once anything
        // is queued due to an active write cycle, continue queueing until the
        // queue is flushed to avoid reordering (e.g., ExecEnd before ExecBegin).
        if self.is_write_cycle_active() || !self.interrupts.is_empty() {
            push(&mut self.interrupts);
        } else {
            handle(self);
        }
    }

    #[inline]
    fn handle_if_stream_finished(&mut self, finished: bool) {
        if finished {
            if self.task_complete_pending {
                self.bottom_pane.set_task_running(false);
                self.task_complete_pending = false;
            }
            self.flush_interrupt_queue();
        }
    }

    #[inline]
    fn handle_streaming_delta(&mut self, delta: String) {
        let sink = AppEventHistorySink(self.app_event_tx.clone());
        self.stream.begin(&sink);
        self.stream.push_and_maybe_commit(&delta, &sink);
        self.mark_needs_redraw();
    }

    pub(crate) fn handle_exec_end_now(&mut self, ev: ExecCommandEndEvent) {
        let running = self.running_commands.remove(&ev.call_id);
        let (command, parsed) = match running {
            Some(rc) => (rc.command, rc.parsed_cmd),
            None => (vec![ev.call_id.clone()], Vec::new()),
        };
        self.pending_exec_completions.push((
            command,
            parsed,
            CommandOutput {
                exit_code: ev.exit_code,
                stdout: ev.stdout.clone(),
                stderr: ev.stderr.clone(),
            },
        ));

        if self.running_commands.is_empty() {
            self.active_exec_cell = None;
            let pending = std::mem::take(&mut self.pending_exec_completions);
            for (command, parsed, output) in pending {
                self.add_to_history(history_cell::new_completed_exec_command(
                    command, parsed, output,
                ));
            }
        }
    }

    pub(crate) fn handle_patch_apply_end_now(
        &mut self,
        event: codex_core::protocol::PatchApplyEndEvent,
    ) {
        if event.success {
            self.add_to_history(history_cell::new_patch_apply_success(event.stdout));
        } else {
            self.add_to_history(history_cell::new_patch_apply_failure(event.stderr));
        }
    }

    pub(crate) fn handle_exec_approval_now(&mut self, id: String, ev: ExecApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();

        let request = ApprovalRequest::Exec {
            id,
            command: ev.command,
            reason: ev.reason,
        };
        self.bottom_pane.push_approval_request(request);
        self.mark_needs_redraw();
    }

    pub(crate) fn handle_apply_patch_approval_now(
        &mut self,
        id: String,
        ev: ApplyPatchApprovalRequestEvent,
    ) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_patch_event(
            PatchEventType::ApprovalRequest,
            ev.changes.clone(),
        ));

        let request = ApprovalRequest::ApplyPatch {
            id,
            reason: ev.reason,
            grant_root: ev.grant_root,
        };
        self.bottom_pane.push_approval_request(request);
        self.mark_needs_redraw();
    }

    pub(crate) fn handle_exec_begin_now(&mut self, ev: ExecCommandBeginEvent) {
        // Ensure the status indicator is visible while the command runs.
        self.running_commands.insert(
            ev.call_id.clone(),
            RunningCommand {
                command: ev.command.clone(),
                parsed_cmd: ev.parsed_cmd.clone(),
            },
        );
        // Accumulate parsed commands into a single active Exec cell so they stack
        match self.active_exec_cell.as_mut() {
            Some(exec) => {
                exec.parsed.extend(ev.parsed_cmd);
            }
            _ => {
                self.active_exec_cell = Some(history_cell::new_active_exec_command(
                    ev.command,
                    ev.parsed_cmd,
                ));
            }
        }

        // Request a redraw so the working header and command list are visible immediately.
        self.mark_needs_redraw();
    }

    pub(crate) fn handle_mcp_begin_now(&mut self, ev: McpToolCallBeginEvent) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_active_mcp_tool_call(ev.invocation));
    }
    pub(crate) fn handle_mcp_end_now(&mut self, ev: McpToolCallEndEvent) {
        self.flush_answer_stream_with_separator();
        self.add_boxed_history(history_cell::new_completed_mcp_tool_call(
            80,
            ev.invocation,
            ev.duration,
            ev.result
                .as_ref()
                .map(|r| !r.is_error.unwrap_or(false))
                .unwrap_or(false),
            ev.result,
        ));
    }
    fn interrupt_running_task(&mut self) {
        if self.bottom_pane.is_task_running() {
            self.active_exec_cell = None;
            self.running_commands.clear();
            self.bottom_pane.clear_ctrl_c_quit_hint();
            self.submit_op(Op::Interrupt);
            self.bottom_pane.set_task_running(false);
            self.stream.clear_all();
            self.request_redraw();
        }
    }
    fn layout_areas(&self, area: Rect) -> [Rect; 2] {
        Layout::vertical([
            Constraint::Max(
                self.active_exec_cell
                    .as_ref()
                    .map_or(0, |c| c.desired_height(area.width)),
            ),
            Constraint::Min(self.bottom_pane.desired_height(area.width)),
        ])
        .areas(area)
    }

    pub(crate) fn new(
        config: Config,
        conversation_manager: Arc<ConversationManager>,
        frame_requester: FrameRequester,
        app_event_tx: AppEventSender,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        enhanced_keys_supported: bool,
    ) -> Self {
        let mut rng = rand::rng();
        let placeholder = EXAMPLE_PROMPTS[rng.random_range(0..EXAMPLE_PROMPTS.len())].to_string();
        let codex_op_tx = spawn_agent(config.clone(), app_event_tx.clone(), conversation_manager);

        Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
            }),
            active_exec_cell: None,
            config: config.clone(),
            initial_user_message: create_initial_user_message(
                initial_prompt.unwrap_or_default(),
                initial_images,
            ),
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            stream: StreamController::new(config),
            running_commands: HashMap::new(),
            pending_exec_completions: Vec::new(),
            task_complete_pending: false,
            interrupts: InterruptManager::new(),
            needs_redraw: false,
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            session_id: None,
        }
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane.desired_height(width)
            + self
                .active_exec_cell
                .as_ref()
                .map_or(0, |c| c.desired_height(width))
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Press {
            self.bottom_pane.clear_ctrl_c_quit_hint();
        }

        match self.bottom_pane.handle_key_event(key_event) {
            InputResult::Submitted(text) => {
                if self.try_handle_slash_command(&text) {
                    // handled by app via event
                } else {
                    self.submit_user_message(text.into());
                }
            }
            InputResult::None => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        self.bottom_pane.handle_paste(text);
    }

    fn flush_active_exec_cell(&mut self) {
        if let Some(active) = self.active_exec_cell.take() {
            self.app_event_tx
                .send(AppEvent::InsertHistoryCell(Box::new(active)));
        }
    }

    fn add_to_history(&mut self, cell: impl HistoryCell + 'static) {
        self.flush_active_exec_cell();
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
    }

    fn add_boxed_history(&mut self, cell: Box<dyn HistoryCell>) {
        self.flush_active_exec_cell();
        self.app_event_tx.send(AppEvent::InsertHistoryCell(cell));
    }

    fn submit_user_message(&mut self, user_message: UserMessage) {
        let UserMessage { text, image_paths } = user_message;
        let mut items: Vec<InputItem> = Vec::new();

        if !text.is_empty() {
            items.push(InputItem::Text { text: text.clone() });
        }

        for path in image_paths {
            items.push(InputItem::LocalImage { path });
        }

        if items.is_empty() {
            return;
        }

        self.codex_op_tx
            .send(Op::UserInput { items })
            .unwrap_or_else(|e| {
                tracing::error!("failed to send message: {e}");
            });

        // Persist the text to cross-session message history.
        if !text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text: text.clone() })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        // Only show the text portion in conversation history.
        if !text.is_empty() {
            self.add_to_history(history_cell::new_user_prompt(text.clone()));
        }
    }

    /// Parse minimal `/resume` command and forward to the app layer.
    fn try_handle_slash_command(&mut self, text: &str) -> bool {
        if !text.trim_start().starts_with('/') {
            return false;
        }
        let mut parts = shlex::Shlex::new(text.trim());
        let cmd: String = parts.next().map(|s| s.to_string()).unwrap_or_default();
        match cmd.as_str() {
            "/resume" => {
                let target = match parts.next() {
                    Some(t) => t,
                    None => return false,
                };
                let mut at: Option<String> = None;
                let mut step: Option<usize> = None;
                let mut prompt_tokens: Vec<String> = Vec::new();
                while let Some(tok) = parts.next() {
                    if tok == "--at" {
                        if let Some(id) = parts.next() {
                            at = Some(id);
                        }
                    } else if tok == "--step" {
                        if let Some(n) = parts.next() {
                            step = n.parse::<usize>().ok();
                        }
                    } else {
                        prompt_tokens.push(tok);
                        // drain rest into prompt
                        prompt_tokens.extend(parts.by_ref());
                        break;
                    }
                }
                let prompt = prompt_tokens.join(" ");
                if prompt.is_empty() {
                    return false;
                }
                self.app_event_tx.send(AppEvent::ResumeRequest {
                    target,
                    at,
                    step,
                    prompt,
                });
                true
            }
            _ => false,
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        // Reset redraw flag for this dispatch
        self.needs_redraw = false;
        let Event { id, msg } = event;

        match msg {
            EventMsg::AgentMessageDelta(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::ExecCommandOutputDelta(_) => {}
            _ => {
                tracing::trace!("handle_codex_event: {:?}", msg);
            }
        }

        match msg {
            EventMsg::SessionConfigured(e) => self.on_session_configured(e),
            EventMsg::AgentMessage(AgentMessageEvent { message }) => self.on_agent_message(message),
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.on_agent_message_delta(delta)
            }
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta })
            | EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => self.on_agent_reasoning_delta(delta),
            EventMsg::AgentReasoning(AgentReasoningEvent { .. })
            | EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { .. }) => {
                self.on_agent_reasoning_final()
            }
            EventMsg::AgentReasoningSectionBreak(_) => self.on_reasoning_section_break(),
            EventMsg::TaskStarted => self.on_task_started(),
            EventMsg::TaskComplete(TaskCompleteEvent { .. }) => self.on_task_complete(),
            EventMsg::TokenCount(token_usage) => self.on_token_count(token_usage),
            EventMsg::Error(ErrorEvent { message }) => self.on_error(message),
            EventMsg::TurnAborted(_) => self.on_error("Turn interrupted".to_owned()),
            EventMsg::PlanUpdate(update) => self.on_plan_update(update),
            EventMsg::ExecApprovalRequest(ev) => self.on_exec_approval_request(id, ev),
            EventMsg::ApplyPatchApprovalRequest(ev) => self.on_apply_patch_approval_request(id, ev),
            EventMsg::ExecCommandBegin(ev) => self.on_exec_command_begin(ev),
            EventMsg::ExecCommandOutputDelta(delta) => self.on_exec_command_output_delta(delta),
            EventMsg::PatchApplyBegin(ev) => self.on_patch_apply_begin(ev),
            EventMsg::PatchApplyEnd(ev) => self.on_patch_apply_end(ev),
            EventMsg::ExecCommandEnd(ev) => self.on_exec_command_end(ev),
            EventMsg::McpToolCallBegin(ev) => self.on_mcp_tool_call_begin(ev),
            EventMsg::McpToolCallEnd(ev) => self.on_mcp_tool_call_end(ev),
            EventMsg::GetHistoryEntryResponse(ev) => self.on_get_history_entry_response(ev),
            EventMsg::McpListToolsResponse(ev) => self.on_list_mcp_tools(ev),
            EventMsg::ShutdownComplete => self.on_shutdown_complete(),
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => self.on_turn_diff(unified_diff),
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                self.on_background_event(message)
            }
        }
        // Coalesce redraws: issue at most one after handling the event
        if self.needs_redraw {
            self.request_redraw();
            self.needs_redraw = false;
        }
    }

    fn request_redraw(&mut self) {
        self.frame_requester.schedule_frame();
    }

    pub(crate) fn add_diff_in_progress(&mut self) {
        self.bottom_pane.set_task_running(true);
        self.bottom_pane
            .update_status_text("computing diff".to_string());
        self.request_redraw();
    }

    pub(crate) fn add_diff_output(&mut self, diff_output: String) {
        self.bottom_pane.set_task_running(false);
        self.add_to_history(history_cell::new_diff_output(diff_output));
        self.mark_needs_redraw();
    }

    pub(crate) fn add_status_output(&mut self) {
        self.add_to_history(history_cell::new_status_output(
            &self.config,
            &self.total_token_usage,
            &self.session_id,
        ));
    }

    /// Open a timeline view to choose a session and step to resume from.
    pub(crate) fn open_resume_timeline(&mut self) {
        let items = self.collect_timeline_items();
        if items.is_empty() {
            // No sessions; add a hint to the history.
            let mut lines = Vec::new();
            lines.push(ratatui::text::Line::from(
                "No sessions found under ~/.codex/sessions",
            ));
            lines.push(ratatui::text::Line::from(""));
            self.add_to_history(crate::history_cell::PlainHistoryCell::from_lines(lines));
            self.request_redraw();
            return;
        }
        // Provide clearer navigation/help, including fast navigation keys.
        self.bottom_pane.show_selection_view(
            "Resume from a previous turn".to_string(),
            Some(
                "Sorted by time (newest first). Use ↑/↓; PgUp/PgDn/Home/End for fast navigation."
                    .to_string(),
            ),
            Some("Press Enter to resume or Esc to cancel".to_string()),
            items,
        );
    }

    fn collect_timeline_items(&self) -> Vec<crate::bottom_pane::SelectionItem> {
        use crate::bottom_pane::SelectionAction;
        use crate::bottom_pane::SelectionItem;
        let mut items: Vec<SelectionItem> = Vec::new();
        let base = self.config.codex_home.join("sessions");
        let mut files = Vec::new();
        self.collect_rollouts(&base, &mut files);

        // Collect timeline entries across all sessions, then sort by timestamp desc
        struct Entry {
            sort_key: i64,
            name: String,
            description: Option<String>,
            action: SelectionAction,
            cwd_match: bool,
        }

        let mut entries: Vec<Entry> = Vec::new();
        for f in files.into_iter() {
            let session_id = Self::session_id_from_filename(&f).unwrap_or_else(|| "?".to_string());
            let steps = Self::read_steps_from_rollout(&f);
            let cwd = Self::extract_cwd_from_rollout(&f);
            let cwd_short = cwd.as_deref().map(Self::shorten_path);
            let cwd_match = cwd
                .as_deref()
                .map(|c| Self::paths_match(&self.config.cwd, c))
                .unwrap_or(false);

            for (idx, (ts, summary, sort_key)) in steps.into_iter().enumerate() {
                let name = format!("{} [step {}]", &session_id, idx);
                let mut desc_parts: Vec<String> = Vec::new();
                if !ts.is_empty() {
                    desc_parts.push(ts);
                }
                if let Some(c) = &cwd_short {
                    desc_parts.push(format!("cwd: {}", c));
                }
                if !summary.is_empty() {
                    desc_parts.push(summary);
                }
                let description = if desc_parts.is_empty() {
                    None
                } else {
                    Some(desc_parts.join("  •  "))
                };
                let path_str = f.to_string_lossy().to_string();
                let step_idx = idx;
                let action: SelectionAction = Box::new(move |tx| {
                    tx.send(crate::app_event::AppEvent::ResumeRequest {
                        target: path_str.clone(),
                        at: None,
                        step: Some(step_idx),
                        prompt: String::new(),
                    });
                });
                entries.push(Entry {
                    sort_key,
                    name,
                    description,
                    action,
                    cwd_match,
                });
            }
        }

        // Sort newest first; prioritize entries matching current cwd
        entries.sort_by(|a, b| match b.cwd_match.cmp(&a.cwd_match) {
            std::cmp::Ordering::Equal => b.sort_key.cmp(&a.sort_key),
            other => other,
        });

        for e in entries.into_iter() {
            items.push(SelectionItem {
                name: e.name,
                description: e.description,
                is_current: false,
                actions: vec![e.action],
            });
        }
        items
    }

    fn collect_rollouts(&self, dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if !dir.exists() {
            return;
        }
        if let Ok(read) = std::fs::read_dir(dir) {
            for e in read.flatten() {
                let p = e.path();
                if p.is_dir() {
                    self.collect_rollouts(&p, out);
                } else if let Some(name) = p.file_name().and_then(|n| n.to_str())
                    && name.starts_with("rollout-")
                    && name.ends_with(".jsonl")
                {
                    out.push(p);
                }
            }
        }
    }

    fn session_id_from_filename(path: &std::path::Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".jsonl")?;
        let id = stem.rsplit('-').next()?;
        Some(id.to_string())
    }

    /// Extract step metadata from a rollout file as (ts, summary, sort_key_ms).
    /// For legacy sessions without state lines, synthesize a single entry using file mtime
    /// and a brief summary from the last assistant/user message.
    fn read_steps_from_rollout(path: &std::path::Path) -> Vec<(String, String, i64)> {
        let mut steps: Vec<(String, String, i64)> = Vec::new();
        let Ok(text) = std::fs::read_to_string(path) else {
            return steps;
        };
        let mut last_id: Option<String> = None;
        for line in text.lines() {
            if !line.contains("\"record_type\":\"state\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let id = v
                .get("last_response_id")
                .or_else(|| v.get("state").and_then(|s| s.get("last_response_id")))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if id.is_some() && id != last_id {
                let ts = v
                    .get("created_at")
                    .or_else(|| v.get("state").and_then(|s| s.get("created_at")))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = v
                    .get("summary")
                    .or_else(|| v.get("state").and_then(|s| s.get("summary")))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let sort_key = chrono::DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or_else(|_| Self::file_mtime_ms(path));
                steps.push((ts, summary, sort_key));
                last_id = id;
            }
        }
        if steps.is_empty() {
            // Legacy fallback: synthesize one step when there are no state records.
            let ts_display = Self::file_mtime_display(path);
            let summary = Self::legacy_synthesize_summary_from_rollout(path);
            let sort_key = Self::file_mtime_ms(path);
            steps.push((ts_display, summary, sort_key));
        }
        steps
    }

    fn file_mtime_ms(path: &std::path::Path) -> i64 {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn file_mtime_display(path: &std::path::Path) -> String {
        use chrono::DateTime;
        use chrono::Local;
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(sys_time) => DateTime::<Local>::from(sys_time)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            Err(_) => String::new(),
        }
    }

    fn legacy_synthesize_summary_from_rollout(path: &std::path::Path) -> String {
        let Ok(text) = std::fs::read_to_string(path) else {
            return String::new();
        };
        let mut last_user: Option<String> = None;
        let mut last_assistant: Option<String> = None;
        for line in text.lines() {
            if line.contains("\"record_type\":\"state\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("type").and_then(|x| x.as_str()) != Some("message") {
                continue;
            }
            let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("");
            // Extract text content
            let mut content = String::new();
            if let Some(arr) = v.get("content").and_then(|x| x.as_array()) {
                for item in arr {
                    if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(t);
                    } else if let Some(c0) = item
                        .get("content")
                        .and_then(|c| c.get(0))
                        .and_then(|x| x.get("text"))
                        .and_then(|x| x.as_str())
                    {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(c0);
                    }
                }
            }
            if content.trim().is_empty() {
                continue;
            }
            if role == "assistant" {
                last_assistant = Some(content);
            } else if role == "user" {
                last_user = Some(content);
            }
        }
        let raw = last_assistant.or(last_user).unwrap_or_default();
        let trimmed = raw.lines().next().unwrap_or("").trim();
        let mut out = trimmed.to_string();
        if out.len() > 80 {
            out.truncate(80);
            out.push('…');
        }
        out
    }

    fn extract_cwd_from_rollout(path: &std::path::Path) -> Option<String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return None;
        };
        for line in text.lines() {
            if line.contains("\"record_type\":\"state\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let typ = v.get("type").and_then(|x| x.as_str());
            if typ == Some("local_shell_call")
                && let Some(action) = v.get("action").and_then(|x| x.get("exec"))
                && let Some(cwd) = action.get("working_directory").and_then(|x| x.as_str())
                && !cwd.is_empty()
            {
                return Some(cwd.to_string());
            }
        }
        None
    }

    fn shorten_path(path: &str) -> String {
        let p = std::path::Path::new(path);
        let mut comps: Vec<&str> = p
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        if comps.len() <= 2 {
            return comps.join("/");
        }
        comps.drain(..comps.len() - 2);
        comps.join("/")
    }

    fn paths_match(current: &std::path::Path, other: &str) -> bool {
        let other_path = std::path::Path::new(other);
        // If either is a prefix of the other, treat as match.
        current.starts_with(other_path) || other_path.starts_with(current)
    }

    /// Open a popup to choose the model preset (model + reasoning effort).
    pub(crate) fn open_model_popup(&mut self) {
        let current_model = self.config.model.clone();
        let current_effort = self.config.model_reasoning_effort;
        let presets: &[ModelPreset] = builtin_model_presets();

        let mut items: Vec<SelectionItem> = Vec::new();
        for preset in presets.iter() {
            let name = preset.label.to_string();
            let description = Some(preset.description.to_string());
            let is_current = preset.model == current_model && preset.effort == current_effort;
            let model_slug = preset.model.to_string();
            let effort = preset.effort;
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                    cwd: None,
                    approval_policy: None,
                    sandbox_policy: None,
                    model: Some(model_slug.clone()),
                    effort: Some(effort),
                    summary: None,
                }));
                tx.send(AppEvent::UpdateModel(model_slug.clone()));
                tx.send(AppEvent::UpdateReasoningEffort(effort));
            })];
            items.push(SelectionItem {
                name,
                description,
                is_current,
                actions,
            });
        }

        self.bottom_pane.show_selection_view(
            "Select model and reasoning level".to_string(),
            Some("Switch between OpenAI models for this and future Codex CLI session".to_string()),
            Some("Press Enter to confirm or Esc to go back".to_string()),
            items,
        );
    }

    /// Open a popup to choose the approvals mode (ask for approval policy + sandbox policy).
    pub(crate) fn open_approvals_popup(&mut self) {
        let current_approval = self.config.approval_policy;
        let current_sandbox = self.config.sandbox_policy.clone();
        let mut items: Vec<SelectionItem> = Vec::new();
        let presets: Vec<ApprovalPreset> = builtin_approval_presets();
        for preset in presets.into_iter() {
            let is_current =
                current_approval == preset.approval && current_sandbox == preset.sandbox;
            let approval = preset.approval;
            let sandbox = preset.sandbox.clone();
            let name = preset.label.to_string();
            let description = Some(preset.description.to_string());
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                    cwd: None,
                    approval_policy: Some(approval),
                    sandbox_policy: Some(sandbox.clone()),
                    model: None,
                    effort: None,
                    summary: None,
                }));
                tx.send(AppEvent::UpdateAskForApprovalPolicy(approval));
                tx.send(AppEvent::UpdateSandboxPolicy(sandbox.clone()));
            })];
            items.push(SelectionItem {
                name,
                description,
                is_current,
                actions,
            });
        }

        self.bottom_pane.show_selection_view(
            "Select Approval Mode".to_string(),
            None,
            Some("Press Enter to confirm or Esc to go back".to_string()),
            items,
        );
    }

    /// Set the approval policy in the widget's config copy.
    pub(crate) fn set_approval_policy(&mut self, policy: AskForApproval) {
        self.config.approval_policy = policy;
    }

    /// Set the sandbox policy in the widget's config copy.
    pub(crate) fn set_sandbox_policy(&mut self, policy: SandboxPolicy) {
        self.config.sandbox_policy = policy;
    }

    /// Set the reasoning effort in the widget's config copy.
    pub(crate) fn set_reasoning_effort(&mut self, effort: ReasoningEffortConfig) {
        self.config.model_reasoning_effort = effort;
    }

    /// Set the model in the widget's config copy.
    pub(crate) fn set_model(&mut self, model: String) {
        self.config.model = model;
    }

    pub(crate) fn add_mcp_output(&mut self) {
        if self.config.mcp_servers.is_empty() {
            self.add_to_history(history_cell::empty_mcp_output());
        } else {
            self.submit_op(Op::ListMcpTools);
        }
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    /// Handle Ctrl-C key press.
    /// Returns CancellationEvent::Handled if the event was consumed by the UI, or
    /// CancellationEvent::Ignored if the caller should handle it (e.g. exit).
    pub(crate) fn on_ctrl_c(&mut self) -> CancellationEvent {
        match self.bottom_pane.on_ctrl_c() {
            CancellationEvent::Handled => return CancellationEvent::Handled,
            CancellationEvent::Ignored => {}
        }
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            CancellationEvent::Ignored
        } else if self.bottom_pane.ctrl_c_quit_hint_visible() {
            self.submit_op(Op::Shutdown);
            CancellationEvent::Handled
        } else {
            self.bottom_pane.show_ctrl_c_quit_hint();
            CancellationEvent::Ignored
        }
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        self.bottom_pane.insert_str(text);
    }
    /// Forward an `Op` directly to codex.
    pub(crate) fn submit_op(&self, op: Op) {
        // Record outbound operation for session replay fidelity.
        crate::session_log::log_outbound_op(&op);
        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to submit op: {e}");
        }
    }

    fn on_list_mcp_tools(&mut self, ev: McpListToolsResponseEvent) {
        self.add_to_history(history_cell::new_mcp_tools_output(&self.config, ev.tools));
    }

    /// Programmatically submit a user text message as if typed in the
    /// composer. The text will be added to conversation history and sent to
    /// the agent.
    pub(crate) fn submit_text_message(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.submit_user_message(text.into());
    }

    pub(crate) fn token_usage(&self) -> &TokenUsage {
        &self.total_token_usage
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.total_token_usage = TokenUsage::default();
        self.bottom_pane.set_token_usage(
            self.total_token_usage.clone(),
            self.last_token_usage.clone(),
            self.config.model_context_window,
        );
    }

    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let [_, bottom_pane_area] = self.layout_areas(area);
        self.bottom_pane.cursor_pos(bottom_pane_area)
    }
}

impl WidgetRef for &ChatWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let [active_cell_area, bottom_pane_area] = self.layout_areas(area);
        (&self.bottom_pane).render(bottom_pane_area, buf);
        if let Some(cell) = &self.active_exec_cell {
            cell.render_ref(active_cell_area, buf);
        }
    }
}

const EXAMPLE_PROMPTS: [&str; 6] = [
    "Explain this codebase",
    "Summarize recent commits",
    "Implement {feature}",
    "Find and fix a bug in @filename",
    "Write tests for @filename",
    "Improve documentation in @filename",
];

fn add_token_usage(current_usage: &TokenUsage, new_usage: &TokenUsage) -> TokenUsage {
    let cached_input_tokens = match (
        current_usage.cached_input_tokens,
        new_usage.cached_input_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    let reasoning_output_tokens = match (
        current_usage.reasoning_output_tokens,
        new_usage.reasoning_output_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    TokenUsage {
        input_tokens: current_usage.input_tokens + new_usage.input_tokens,
        cached_input_tokens,
        output_tokens: current_usage.output_tokens + new_usage.output_tokens,
        reasoning_output_tokens,
        total_tokens: current_usage.total_tokens + new_usage.total_tokens,
    }
}

// Extract the first bold (Markdown) element in the form **...** from `s`.
// Returns the inner text if found; otherwise `None`.
fn extract_first_bold(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    // Found closing **
                    let inner = &s[start..j];
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    } else {
                        return None;
                    }
                }
                j += 1;
            }
            // No closing; stop searching (wait for more deltas)
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests;
