//! Bridge VectorCraft to the user's local Codex app-server session.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};
use vectorcraft_ui_egui::assistant::{Inbox, Request, Response, SendFn};

use crate::openai;

const CLOSED: &str = "The local Codex app-server closed before VectorCraft received a response.";

pub fn available() -> bool {
    codex_path().is_some()
}

pub fn transport(ctx: &egui::Context) -> (SendFn, Inbox) {
    let inbox = Arc::new(Mutex::new(Vec::new()));
    let (requests, received) = mpsc::channel::<Request>();
    let repaint = ctx.clone();
    let response_inbox = inbox.clone();
    let startup_error = Arc::new(Mutex::new(None::<String>));
    let thread_error = startup_error.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("vectorcraft-codex-bridge".into())
        .spawn(move || bridge_loop(received, response_inbox, repaint, thread_error))
        && let Ok(mut message) = startup_error.lock()
    {
        *message = Some(format!("Could not start the Codex bridge: {error}"));
    }
    let send: SendFn = Box::new(move |request| {
        if let Ok(message) = startup_error.lock()
            && let Some(message) = message.as_ref()
        {
            return Err(message.clone());
        }
        requests.send(request).map_err(|_| "The local Codex bridge is no longer running.".to_string())
    });
    (send, inbox)
}

fn bridge_loop(received: mpsc::Receiver<Request>, inbox: Inbox, ctx: egui::Context, startup_error: Arc<Mutex<Option<String>>>) {
    let mut connection = match Connection::start() {
        Ok(connection) => connection,
        Err(error) => {
            if let Ok(mut message) = startup_error.lock() {
                *message = Some(error.clone());
            }
            while let Ok(request) = received.recv() {
                publish(&inbox, &ctx, Response::error(request.id, error.clone()));
            }
            return;
        }
    };
    while let Ok(request) = received.recv() {
        let response = match connection.turn(&request, &mut |activity| publish(&inbox, &ctx, Response::progress(request.id, activity))) {
            Ok(text) => openai::parse_model_text(request.id, &text).unwrap_or_else(|error| Response::error(request.id, error)),
            Err(error) => Response::error(request.id, error),
        };
        publish(&inbox, &ctx, response);
    }
}

fn publish(inbox: &Inbox, ctx: &egui::Context, response: Response) {
    if let Ok(mut queued) = inbox.lock() {
        queued.push(response);
    }
    ctx.request_repaint();
}

struct Connection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    thread_id: String,
    next_id: u64,
}

impl Connection {
    fn start() -> Result<Self, String> {
        let path =
            codex_path().ok_or_else(|| "Codex was not found. Install Codex or set VECTORCRAFT_CODEX_PATH, then relaunch VectorCraft.".to_string())?;
        let mut child = ProcessCommand::new(path)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not start the local Codex app-server: {error}"))?;
        let stdin = child.stdin.take().ok_or("Codex app-server did not provide an input channel.")?;
        let stdout = child.stdout.take().ok_or("Codex app-server did not provide an output channel.")?;
        let mut connection = Self { child, stdin: BufWriter::new(stdin), stdout: BufReader::new(stdout), thread_id: String::new(), next_id: 1 };
        let initialize = connection.next_request_id();
        connection.send(json!({"id": initialize, "method": "initialize", "params": {"clientInfo": {"name": "vectorcraft", "title": "VectorCraft", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {}}}))?;
        let _ = connection.response_for(initialize)?;
        connection.notify("initialized", json!({}))?;
        let thread = connection.next_request_id();
        connection.send(json!({"id": thread, "method": "thread/start", "params": {"cwd": std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()), "approvalPolicy": "never", "sandbox": "read-only", "ephemeral": true, "developerInstructions": openai::ASSISTANT_INSTRUCTIONS, "baseInstructions": "Use only the registered VectorCraft command catalog supplied in each turn. Do not use host tools or edit files."}}))?;
        let result = connection.response_for(thread)?;
        connection.thread_id = result
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or("Codex app-server returned no thread id.")?
            .to_string();
        Ok(connection)
    }

    fn turn(&mut self, request: &Request, progress: &mut dyn FnMut(&str)) -> Result<String, String> {
        let input = vec![json!({"type": "text", "text": openai::prompt_for(request)?})];
        let id = self.next_request_id();
        self.send(json!({"id": id, "method": "turn/start", "params": {"threadId": self.thread_id, "input": input, "approvalPolicy": "never", "sandboxPolicy": {"type": "readOnly", "networkAccess": false}}}))?;
        let result = self.response_for(id)?;
        progress("Thinking through the request…");
        let turn_id =
            result.get("turn").and_then(|turn| turn.get("id")).and_then(Value::as_str).ok_or("Codex app-server returned no turn id.")?.to_string();
        let mut text = String::new();
        loop {
            let message = self.read_message()?;
            match message.get("method").and_then(Value::as_str) {
                Some("item/agentMessage/delta") => {
                    progress("Drafting the VectorCraft response…");
                    let params = message.get("params");
                    if params.and_then(|p| p.get("threadId")).and_then(Value::as_str) == Some(self.thread_id.as_str())
                        && params.and_then(|p| p.get("turnId")).and_then(Value::as_str) == Some(turn_id.as_str())
                        && let Some(delta) = params.and_then(|p| p.get("delta")).and_then(Value::as_str)
                    {
                        text.push_str(delta);
                    }
                }
                Some("item/completed") if text.is_empty() => {
                    progress("Preparing native VectorCraft edits…");
                    if let Some(item) = message.get("params").and_then(|p| p.get("item"))
                        && item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && let Some(item_text) = item.get("text").and_then(Value::as_str)
                    {
                        text.push_str(item_text);
                    }
                }
                Some("turn/completed") => {
                    let params = message.get("params");
                    let completed = params.and_then(|p| p.get("turn"));
                    if params.and_then(|p| p.get("threadId")).and_then(Value::as_str) == Some(self.thread_id.as_str())
                        && completed.and_then(|turn| turn.get("id")).and_then(Value::as_str) == Some(turn_id.as_str())
                    {
                        let status = completed.and_then(|turn| turn.get("status")).and_then(Value::as_str).unwrap_or("failed");
                        if status != "completed" {
                            return Err(format!("Codex turn {status}: {}", turn_error_detail(completed.unwrap_or(&Value::Null))));
                        }
                        if text.is_empty()
                            && let Some(items) = completed.and_then(|turn| turn.get("items")).and_then(Value::as_array)
                        {
                            for item in items {
                                if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                                    && let Some(value) = item.get("text").and_then(Value::as_str)
                                {
                                    text.push_str(value);
                                }
                            }
                        }
                        if text.trim().is_empty() {
                            return Err("Codex returned an empty VectorCraft response.".into());
                        }
                        return Ok(text);
                    }
                }
                _ => {}
            }
            if let Some(server_id) = message.get("id") {
                self.deny_server_request(server_id)?;
            }
        }
    }

    fn response_for(&mut self, id: u64) -> Result<Value, String> {
        loop {
            let message = self.read_message()?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("Codex app-server: {}", error.get("message").and_then(Value::as_str).unwrap_or("unknown error")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(server_id) = message.get("id") {
                self.deny_server_request(server_id)?;
            }
        }
    }

    fn read_message(&mut self) -> Result<Value, String> {
        let mut line = String::new();
        if self.stdout.read_line(&mut line).map_err(|error| format!("Could not read from Codex app-server: {error}"))? == 0 {
            return Err(CLOSED.into());
        }
        serde_json::from_str(line.trim()).map_err(|error| format!("Codex app-server returned invalid JSON: {error}"))
    }

    fn send(&mut self, message: Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.stdin, &message).map_err(|error| format!("Could not send to Codex app-server: {error}"))?;
        self.stdin.write_all(b"\n").map_err(|error| format!("Could not send to Codex app-server: {error}"))?;
        self.stdin.flush().map_err(|error| format!("Could not flush Codex app-server input: {error}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({"method": method, "params": params}))
    }
    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
    fn deny_server_request(&mut self, id: &Value) -> Result<(), String> {
        self.send(json!({"id": id, "error": {"code": -32601, "message": "VectorCraft does not allow Codex tool requests."}}))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn turn_error_detail(turn: &Value) -> String {
    turn.get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| turn.get("error").and_then(Value::as_str))
        .unwrap_or(CLOSED)
        .to_string()
}

fn codex_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VECTORCRAFT_CODEX_PATH").map(PathBuf::from).filter(|path| path.is_file()) {
        return Some(path);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let candidate = path.join("codex");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [
        home.join(".local/bin/codex"),
        home.join(".antigravity-ide/bin/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}
