//! VectorCraft's native AI Assistant panel and local bridge contract.
//!
//! The desktop host supplies a transport backed by Codex's local app-server. The assistant only
//! returns registered VectorCraft commands; this module validates and executes them on the UI
//! thread so every edit remains native and undoable.

use std::sync::{Arc, Mutex};

use egui::{Color32, RichText};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::VectorcraftApp;
use crate::theme::Tokens;

#[derive(Clone, Debug, Serialize)]
pub struct Request {
    pub id: u64,
    pub prompt: String,
    pub context: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Command {
    pub command: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response {
    pub id: u64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub progress: Option<String>,
}

impl Response {
    pub fn error(id: u64, error: impl Into<String>) -> Self {
        Self { id, message: String::new(), commands: Vec::new(), done: true, error: Some(error.into()), progress: None }
    }

    pub fn progress(id: u64, progress: impl Into<String>) -> Self {
        Self { id, message: String::new(), commands: Vec::new(), done: false, error: None, progress: Some(progress.into()) }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub request_id: Option<u64>,
    pub role: Role,
    pub text: String,
    pub commands: Vec<Command>,
    pub applied: Vec<bool>,
    pub progress: Option<String>,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Default)]
pub struct State {
    pub draft: String,
    pub messages: Vec<Message>,
    pub pending: Option<u64>,
    pub next_request: u64,
    pub context_open: bool,
    /// Enables trusted localhost clients, including this Codex session, to submit prompts.
    pub external_prompts_enabled: bool,
}

pub type Inbox = Arc<Mutex<Vec<Response>>>;
pub type SendFn = Box<dyn FnMut(Request) -> Result<(), String>>;

/// Drain transport responses and apply returned native commands on the UI thread.
pub fn drain(app: &mut VectorcraftApp, ctx: &egui::Context) {
    let Some(inbox) = app.services.assistant_inbox.clone() else { return };
    let responses = std::mem::take(&mut *inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    if responses.is_empty() {
        return;
    }
    for response in responses {
        if app.assistant.pending != Some(response.id) {
            continue;
        }
        if let Some(error) = response.error {
            app.assistant.messages.push(Message {
                request_id: Some(response.id),
                role: Role::System,
                text: format!("Assistant request failed: {error}"),
                commands: Vec::new(),
                applied: Vec::new(),
                progress: None,
                complete: true,
            });
            app.assistant.pending = None;
            continue;
        }
        let index = app.assistant.messages.iter().position(|m| m.request_id == Some(response.id)).unwrap_or_else(|| {
            app.assistant.messages.push(Message {
                request_id: Some(response.id),
                role: Role::Assistant,
                text: String::new(),
                commands: Vec::new(),
                applied: Vec::new(),
                progress: None,
                complete: false,
            });
            app.assistant.messages.len() - 1
        });
        if let Some(message) = app.assistant.messages.get_mut(index) {
            if !response.message.is_empty() {
                message.text = response.message;
            }
            if !response.commands.is_empty() {
                message.commands = response.commands;
                message.applied = vec![false; message.commands.len()];
            }
            message.progress = response.progress;
            message.complete = response.done;
        }
        if response.done {
            let count = app.assistant.messages[index].commands.len();
            for command_index in 0..count {
                let _ = execute_command(app, index, command_index);
            }
            app.assistant.pending = None;
        }
    }
    ctx.request_repaint();
}

fn document_context(app: &mut VectorcraftApp) -> Result<Value, String> {
    let document = app.run("document.inspect", json!({}))?;
    let panels = crate::state::all_panels().map(|(id, label)| json!({"id": id, "label": label})).collect::<Vec<_>>();
    let tools = vectorcraft_tools::TOOL_GROUPS
        .iter()
        .chain(vectorcraft_tools::addons::GROUPS.iter())
        .map(|group| serde_json::to_value(group).unwrap_or_default())
        .collect::<Vec<_>>();
    Ok(json!({
        "document": document,
        "activeTool": app.session.tool_id(),
        "availableCommands": crate::control::all_commands(app),
        "availableTools": tools,
        "availablePanels": panels,
        "assistantExecution": {"autoApply": true, "requiresReview": false, "undoable": true},
        "externalPromptBridge": {"enabled": app.assistant.external_prompts_enabled}
    }))
}

/// Full context exposed to a trusted local control client.
pub fn inspect(app: &mut VectorcraftApp) -> Value {
    match document_context(app) {
        Ok(context) => context,
        Err(error) => json!({"document": null, "error": error, "externalPromptBridge": {"enabled": app.assistant.external_prompts_enabled}}),
    }
}

/// Submit a prompt from the localhost control bridge. The panel toggle is the trust gate.
pub fn submit_external(app: &mut VectorcraftApp, prompt: String) -> Result<u64, String> {
    if !app.assistant.external_prompts_enabled {
        return Err("External prompt bridge is disabled in the AI Assistant panel.".into());
    }
    submit_prompt(app, prompt)
}

pub fn external_status(app: &VectorcraftApp) -> Value {
    json!({
        "enabled": app.assistant.external_prompts_enabled,
        "working": app.assistant.pending.is_some(),
        "requestId": app.assistant.pending,
        "messageCount": app.assistant.messages.len()
    })
}

fn submit_prompt(app: &mut VectorcraftApp, prompt: String) -> Result<u64, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("Assistant prompt is empty.".into());
    }
    if app.assistant.pending.is_some() {
        return Err("The assistant is already working on another prompt.".into());
    }
    let context = document_context(app).map_err(|error| format!("Open a document before asking the assistant: {error}"))?;
    app.assistant.next_request = app.assistant.next_request.saturating_add(1).max(1);
    let id = app.assistant.next_request;
    app.assistant.messages.push(Message {
        request_id: None,
        role: Role::User,
        text: prompt.clone(),
        commands: Vec::new(),
        applied: Vec::new(),
        progress: None,
        complete: true,
    });
    app.assistant.pending = Some(id);
    let result = match app.services.assistant_send.as_mut() {
        Some(send) => send(Request { id, prompt, context }),
        None => Err("Assistant transport is not connected. Start VectorCraft with Codex installed.".into()),
    };
    if let Err(error) = result {
        app.assistant.pending = None;
        return Err(format!("Assistant request failed: {error}"));
    }
    Ok(id)
}

fn submit(app: &mut VectorcraftApp) {
    let prompt = app.assistant.draft.trim().to_string();
    if prompt.is_empty() || app.assistant.pending.is_some() {
        return;
    }
    if let Err(error) = submit_prompt(app, prompt) {
        app.assistant.messages.push(Message {
            request_id: None,
            role: Role::System,
            text: error,
            commands: Vec::new(),
            applied: Vec::new(),
            progress: None,
            complete: true,
        });
    } else {
        app.assistant.draft.clear();
    }
}

fn execute_command(app: &mut VectorcraftApp, message_index: usize, command_index: usize) -> Result<Value, String> {
    let command =
        app.assistant.messages.get(message_index).and_then(|m| m.commands.get(command_index)).cloned().ok_or("assistant command disappeared")?;
    if !app.session.commands().iter().any(|info| info.id == command.command)
        && !crate::menus::UI_COMMANDS.iter().any(|entry| entry.0 == command.command)
    {
        return Err(format!("Command is not registered in VectorCraft: {}", command.command));
    }
    crate::menus::check_params(&app.session, &command.command, &command.params)?;
    match app.run(&command.command, command.params) {
        Ok(result) => {
            if let Some(message) = app.assistant.messages.get_mut(message_index)
                && let Some(applied) = message.applied.get_mut(command_index)
            {
                *applied = true;
            }
            app.status(format!("Assistant applied {}. Undo remains available.", command.command));
            Ok(result)
        }
        Err(error) => {
            app.status(format!("AI Assistant: {error}"));
            Err(error)
        }
    }
}

fn role_color(role: Role, t: &Tokens) -> Color32 {
    match role {
        Role::User => t.text,
        Role::Assistant => t.accent,
        Role::System => t.text_dim,
    }
}

/// Draw the persistent AI Assistant panel.
pub fn show(app: &mut VectorcraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let label = app.services.assistant_label.clone().unwrap_or_else(|| "VectorCraft Assistant".into());
    let working = app.assistant.pending.is_some();
    ui.horizontal(|ui| {
        ui.heading(RichText::new("AI Assistant").color(t.text_strong));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Clear").clicked() && !working {
                app.assistant.messages.clear();
                app.assistant.draft.clear();
            }
        });
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&label).strong().color(t.accent));
        ui.label(RichText::new(if working { "• Working" } else { "• Ready" }).color(if working { t.accent } else { t.text_dim }));
        if working {
            ui.spinner();
        }
    });
    ui.label(
        RichText::new("Describe a visual change, inspection, or layout task. Returned edits stay native and undoable.").size(10.0).color(t.text_dim),
    );
    ui.horizontal_wrapped(|ui| {
        if ui.small_button(if app.assistant.context_open { "Hide context" } else { "Show context" }).clicked() {
            app.assistant.context_open = !app.assistant.context_open;
        }
        let was_enabled = app.assistant.external_prompts_enabled;
        ui.checkbox(&mut app.assistant.external_prompts_enabled, "Allow external prompts")
            .on_hover_text("Allow trusted localhost clients, including this Codex session, to submit prompts here.");
        ui.label(
            RichText::new(if app.assistant.external_prompts_enabled { "Bridge ON" } else { "Bridge OFF" })
                .size(10.0)
                .color(if app.assistant.external_prompts_enabled { t.accent } else { t.text_dim }),
        );
        if was_enabled != app.assistant.external_prompts_enabled {
            app.status(if app.assistant.external_prompts_enabled { "External prompt bridge enabled." } else { "External prompt bridge disabled." });
        }
    });
    if app.assistant.context_open {
        egui::Frame::group(ui.style()).fill(t.panel_darker).show(ui, |ui| {
            let title = app.session.active().map(|d| d.title().to_string()).unwrap_or_else(|| "No document".into());
            ui.label(RichText::new("LIVE CONTEXT").size(9.0).strong().color(t.text_dim));
            ui.label(RichText::new(title).strong().color(t.text_strong));
            ui.label(
                RichText::new(format!("{} engine commands · {} panels", app.session.commands().len(), crate::state::all_panels().count()))
                    .size(10.0)
                    .color(t.text_dim),
            );
        });
    }
    if app.session.active().is_none() {
        ui.label(RichText::new("Open a document to start a design conversation.").strong().color(t.text_strong));
        return;
    }
    egui::ScrollArea::vertical().id_salt("vectorcraft_assistant_thread").max_height(340.0).show(ui, |ui| {
        for message in app.assistant.messages.clone() {
            egui::Frame::group(ui.style()).fill(if message.role == Role::User { t.input } else { t.panel_darker }).show(ui, |ui| {
                ui.label(
                    RichText::new(match message.role {
                        Role::User => "You",
                        Role::Assistant => label.as_str(),
                        Role::System => "Status",
                    })
                    .strong()
                    .color(role_color(message.role, &t)),
                );
                if !message.text.is_empty() {
                    ui.label(message.text);
                }
                if let Some(progress) = message.progress {
                    ui.label(RichText::new(progress).size(10.0).color(t.text_dim));
                }
                for (index, command) in message.commands.iter().enumerate() {
                    let applied = message.applied.get(index).copied().unwrap_or(false);
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            if applied { "✓" } else { "•" },
                            if command.description.is_empty() { &command.command } else { &command.description }
                        ))
                        .size(10.0)
                        .color(if applied { t.accent } else { t.text }),
                    );
                }
            });
        }
    });
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        for suggestion in ["Improve visual hierarchy", "Inspect the active artwork", "Create a more expressive composition"] {
            if ui.small_button(suggestion).clicked() {
                app.assistant.draft = suggestion.into();
            }
        }
    });
    let response = ui.add(
        egui::TextEdit::multiline(&mut app.assistant.draft).desired_rows(3).desired_width(f32::INFINITY).hint_text("Describe the change you want…"),
    );
    let send = ui.add_enabled(!working && !app.assistant.draft.trim().is_empty(), egui::Button::new("Run with Assistant"));
    if send.clicked() || (response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter) && input.modifiers.command)) {
        submit(app);
    }
    ui.label(RichText::new("⌘↵ to run · native commands applied automatically · undo remains available").size(9.0).color(t.text_dim));
}
