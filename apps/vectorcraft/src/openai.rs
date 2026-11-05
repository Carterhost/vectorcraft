//! Prompt and response contract shared by the VectorCraft Codex bridge.
//!
//! The native app does not embed an API key. Codex's local app-server owns authentication and
//! model access; VectorCraft supplies the live document/catalog context and validates the command
//! plan before applying it.

use serde_json::{Map, Value, json};

use vectorcraft_ui_egui::assistant::{Command, Request, Response};

pub(crate) const ASSISTANT_INSTRUCTIONS: &str = "You are the VectorCraft AI Assistant, a senior vector designer working directly inside the open VectorCraft document. Inspect the supplied live document context before acting. Apply the requested outcome with the smallest coherent set of registered native VectorCraft engine or UI commands. Returned commands are executed automatically with no approval step, so never invent command ids or parameters. Prefer editable native objects, live text, gradients, effects, layers, and undoable commands over flattened output. Return only JSON with this exact top-level shape: {\"message\":\"brief rationale\",\"commands\":[{\"command\":\"registered.id\",\"params\":{},\"description\":\"what it changes\"}],\"done\":true}. Every command must use the key command, never id, name or action. If the request is impossible with the supplied catalog, return an empty commands array and explain why. Do not use shell commands, host tools, arbitrary code, or external services.";

pub(crate) fn prompt_for(request: &Request) -> Result<String, String> {
    serde_json::to_string(&json!({
        "task": request.prompt,
        "liveDocumentContext": request.context,
        "responseContract": {
            "message": "string",
            "commands": [{"command": "registered command id", "params": {}, "description": "string"}],
            "done": true
        }
    }))
    .map_err(|error| format!("Could not serialize VectorCraft assistant request: {error}"))
}

pub(crate) fn parse_model_text(id: u64, text: &str) -> Result<Response, String> {
    let candidate = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```JSON"))
        .or_else(|| text.trim().strip_prefix("```"))
        .map(|value| value.strip_suffix("```").unwrap_or(value).trim())
        .unwrap_or(text.trim());
    let value: Value = serde_json::from_str(candidate).map_err(|error| format!("VectorCraft assistant returned invalid JSON: {error}"))?;
    let object = value.as_object().ok_or_else(|| "VectorCraft assistant returned a non-object response".to_string())?;
    let message = object.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
    let done = object.get("done").and_then(Value::as_bool).unwrap_or(true);
    let mut commands = Vec::new();
    let mut skipped = 0usize;
    if let Some(raw_commands) = object.get("commands").and_then(Value::as_array) {
        for raw in raw_commands {
            let Some(command_object) = raw.as_object() else {
                skipped += 1;
                continue;
            };
            let Some(command) = command_name(command_object) else {
                skipped += 1;
                continue;
            };
            let params = command_object
                .get("params")
                .or_else(|| command_object.get("arguments"))
                .filter(Value::is_object)
                .cloned()
                .unwrap_or_else(|| json!({}));
            let description =
                command_object.get("description").or_else(|| command_object.get("label")).and_then(Value::as_str).unwrap_or_default().to_string();
            commands.push(Command { command, params, description });
        }
    }
    let message = if skipped == 0 {
        message
    } else if message.is_empty() {
        format!("Skipped {skipped} malformed command entries.")
    } else {
        format!("{message}\n\nSkipped {skipped} malformed command entries.")
    };
    Ok(Response { id, message, commands, done, error: None, progress: None })
}

fn command_name(object: &Map<String, Value>) -> Option<String> {
    for key in ["command", "id", "name", "action"] {
        if let Some(value) = object.get(key) {
            if let Some(name) = value.as_str().filter(|name| !name.trim().is_empty()) {
                return Some(name.to_string());
            }
            if let Some(name) = value.get("id").and_then(Value::as_str).filter(|name| !name.trim().is_empty()) {
                return Some(name.to_string());
            }
        }
    }
    None
}
