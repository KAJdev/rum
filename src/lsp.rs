use lsp_types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

// -- uri helpers --

fn path_to_uri(path: &Path) -> Option<Uri> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let s = format!("file://{}", abs.to_str()?);
    s.parse().ok()
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

// -- language server registry --

struct ServerConfig {
    name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    root_markers: &'static [&'static str],
    extensions: &'static [&'static str],
    language_id: &'static str,
}

const SERVERS: &[ServerConfig] = &[
    ServerConfig {
        name: "rust-analyzer",
        command: "rust-analyzer",
        args: &[],
        root_markers: &["Cargo.toml"],
        extensions: &["rs"],
        language_id: "rust",
    },
    ServerConfig {
        name: "typescript-language-server",
        command: "typescript-language-server",
        args: &["--stdio"],
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        language_id: "typescript",
    },
    ServerConfig {
        name: "pyright",
        command: "pyright-langserver",
        args: &["--stdio"],
        root_markers: &["pyproject.toml", "setup.py", "requirements.txt", "pyrightconfig.json"],
        extensions: &["py", "pyi"],
        language_id: "python",
    },
    ServerConfig {
        name: "gopls",
        command: "gopls",
        args: &["serve"],
        root_markers: &["go.mod"],
        extensions: &["go"],
        language_id: "go",
    },
    ServerConfig {
        name: "clangd",
        command: "clangd",
        args: &[],
        root_markers: &["compile_commands.json", "CMakeLists.txt", "Makefile"],
        extensions: &["c", "cpp", "cc", "cxx", "h", "hpp", "hxx"],
        language_id: "c",
    },
];

// check if a command is available on PATH
fn command_exists(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}

// find the project root by searching upward for root marker files
fn find_project_root(cwd: &Path, markers: &[&str]) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        for marker in markers {
            if dir.join(marker).exists() {
                return Some(dir);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn language_id_for_extension(ext: &str) -> Option<&'static str> {
    // map file extension to the LSP language_id, handling variants
    match ext {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "js" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("cpp"),
        _ => None,
    }
}

// -- json-rpc types --

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcMessage {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<serde_json::Value>,
    method: Option<String>,
    params: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

// -- lsp client --

// events sent from the LSP client to the application
#[derive(Debug, Clone)]
pub enum LspEvent {
    Diagnostics {
        uri: String,
        diagnostics: Vec<DiagnosticInfo>,
    },
    ServerStarted(String),
    ServerError(String),
}

#[derive(Debug, Clone)]
pub struct DiagnosticInfo {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub severity: DiagSeverity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

struct PendingRequest {
    tx: oneshot::Sender<Result<serde_json::Value, String>>,
}

pub struct LspClient {
    _process: Child,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    server_capabilities: Arc<tokio::sync::OnceCell<ServerCapabilities>>,
    root_uri: Uri,
    // document version tracker
    doc_versions: Arc<Mutex<HashMap<String, AtomicI32>>>,
    initialized: Arc<Notify>,
    pub server_name: String,
}

impl LspClient {
    pub async fn start(
        config: &ServerConfig,
        root_path: &Path,
        event_tx: mpsc::UnboundedSender<LspEvent>,
    ) -> anyhow::Result<Self> {
        let mut process = Command::new(config.command)
            .args(config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(root_path)
            .kill_on_drop(true)
            .spawn()?;

        let stdin = process.stdin.take().expect("stdin");
        let stdout = process.stdout.take().expect("stdout");
        let stderr = process.stderr.take().expect("stderr");

        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Arc<Mutex<HashMap<u64, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let server_capabilities = Arc::new(tokio::sync::OnceCell::new());
        let initialized = Arc::new(Notify::new());

        // spawn writer task
        tokio::spawn(writer_loop(writer_rx, stdin));

        // spawn reader task
        let pending_clone = pending.clone();
        let caps_clone = server_capabilities.clone();
        let init_clone = initialized.clone();
        tokio::spawn(reader_loop(
            stdout,
            pending_clone,
            caps_clone,
            init_clone,
            event_tx.clone(),
        ));

        // spawn stderr reader
        let server_name = config.name.to_string();
        let name_clone = server_name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        // log stderr but don't spam the UI
                        eprintln!("[lsp:{}] {}", name_clone, line.trim());
                    }
                    Err(_) => break,
                }
            }
        });

        let root_uri = path_to_uri(root_path)
            .unwrap_or_else(|| "file:///".parse().unwrap());

        let client = Self {
            _process: process,
            writer_tx,
            next_id: AtomicU64::new(1),
            pending,
            server_capabilities,
            root_uri,
            doc_versions: Arc::new(Mutex::new(HashMap::new())),
            initialized,
            server_name,
        };

        // perform initialization handshake
        client.initialize().await?;

        Ok(client)
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(self.root_uri.clone()),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    completion: Some(CompletionClientCapabilities {
                        completion_item: Some(CompletionItemCapability {
                            snippet_support: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                        related_information: Some(true),
                        ..Default::default()
                    }),
                    definition: Some(GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(false),
                    }),
                    synchronization: Some(TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let result = self
            .request::<request::Initialize>(params)
            .await?;

        let _ = self
            .server_capabilities
            .set(result.capabilities);

        // send initialized notification
        self.notify::<notification::Initialized>(InitializedParams {})
            .await;

        self.initialized.notify_waiters();

        Ok(())
    }

    async fn request<R: request::Request>(
        &self,
        params: R::Params,
    ) -> anyhow::Result<R::Result>
    where
        R::Params: Serialize,
        R::Result: for<'de> Deserialize<'de>,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params_value = serde_json::to_value(params)?;

        let msg = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: R::METHOD.to_string(),
            params: Some(params_value),
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, PendingRequest { tx });
        }

        let json = serde_json::to_string(&msg)?;
        let frame = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
        let _ = self.writer_tx.send(frame.into_bytes());

        let result = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await;

        match result {
            Ok(Ok(Ok(value))) => {
                let parsed = serde_json::from_value(value)?;
                Ok(parsed)
            }
            Ok(Ok(Err(e))) => anyhow::bail!("LSP error: {e}"),
            Ok(Err(_)) => anyhow::bail!("LSP request channel closed"),
            Err(_) => {
                // clean up pending request on timeout
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                anyhow::bail!("LSP request timed out")
            }
        }
    }

    async fn notify<N: notification::Notification>(&self, params: N::Params)
    where
        N::Params: Serialize,
    {
        let params_value = serde_json::to_value(params).ok();
        let msg = JsonRpcNotification {
            jsonrpc: "2.0",
            method: N::METHOD.to_string(),
            params: params_value,
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let frame = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
            let _ = self.writer_tx.send(frame.into_bytes());
        }
    }

    pub async fn did_open(&self, uri: &Uri, language_id: &str, text: &str) {
        let version = {
            let mut versions = self.doc_versions.lock().await;
            let v = versions
                .entry(uri.to_string())
                .or_insert_with(|| AtomicI32::new(0));
            v.fetch_add(1, Ordering::SeqCst) + 1
        };

        self.notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_string(),
                version,
                text: text.to_string(),
            },
        })
        .await;
    }

    pub async fn did_change(&self, uri: &Uri, text: &str) {
        let version = {
            let mut versions = self.doc_versions.lock().await;
            let v = versions
                .entry(uri.to_string())
                .or_insert_with(|| AtomicI32::new(0));
            v.fetch_add(1, Ordering::SeqCst) + 1
        };

        self.notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        })
        .await;
    }

    pub async fn did_save(&self, uri: &Uri, text: Option<&str>) {
        self.notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            text: text.map(|t| t.to_string()),
        })
        .await;
    }

    pub async fn did_close(&self, uri: &Uri) {
        self.notify::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
        })
        .await;
        let mut versions = self.doc_versions.lock().await;
        versions.remove(&uri.to_string());
    }

    pub async fn completion(
        &self,
        uri: &Uri,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<CompletionItem>> {
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line, character },
            },
            context: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = self
            .request::<request::Completion>(params)
            .await?;

        let items = match result {
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(list)) => list.items,
            None => Vec::new(),
        };

        Ok(items)
    }

    pub async fn goto_definition(
        &self,
        uri: &Uri,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<Location>> {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = self
            .request::<request::GotoDefinition>(params)
            .await?;

        let locations = match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(GotoDefinitionResponse::Array(locs)) => locs,
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|l| Location {
                    uri: l.target_uri,
                    range: l.target_selection_range,
                })
                .collect(),
            None => Vec::new(),
        };

        Ok(locations)
    }

    pub async fn shutdown(&self) {
        let _ = self.request::<request::Shutdown>(()).await;
        self.notify::<notification::Exit>(()).await;
    }
}

// -- transport --

async fn writer_loop(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stdin: tokio::process::ChildStdin,
) {
    let mut writer = BufWriter::new(stdin);
    while let Some(data) = rx.recv().await {
        if writer.write_all(&data).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    server_capabilities: Arc<tokio::sync::OnceCell<ServerCapabilities>>,
    _initialized: Arc<Notify>,
    event_tx: mpsc::UnboundedSender<LspEvent>,
) {
    let mut reader = BufReader::new(stdout);
    let mut header_buf = String::new();
    let mut body_buf = Vec::new();

    loop {
        // read headers
        let mut content_length: Option<usize> = None;
        loop {
            header_buf.clear();
            match reader.read_line(&mut header_buf).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
            let line = header_buf.trim();
            if line.is_empty() {
                break;
            }
            if let Some(val) = line.strip_prefix("Content-Length: ") {
                content_length = val.parse().ok();
            }
        }

        let length = match content_length {
            Some(l) => l,
            None => continue,
        };

        // read body
        body_buf.resize(length, 0);
        if reader.read_exact(&mut body_buf).await.is_err() {
            return;
        }

        let msg: JsonRpcMessage = match serde_json::from_slice(&body_buf) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // response to a pending request
        if let Some(id_val) = &msg.id {
            if msg.method.is_none() {
                let id = match id_val {
                    serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
                    _ => continue,
                };
                let mut pending = pending.lock().await;
                if let Some(req) = pending.remove(&id) {
                    if let Some(err) = msg.error {
                        let _ = req.tx.send(Err(format!("{err}")));
                    } else {
                        let _ = req
                            .tx
                            .send(Ok(msg.result.unwrap_or(serde_json::Value::Null)));
                    }
                }
                continue;
            }
        }

        // server notification or request
        if let Some(method) = &msg.method {
            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if let Some(params) = msg.params {
                        if let Ok(diag_params) =
                            serde_json::from_value::<PublishDiagnosticsParams>(params)
                        {
                            let diagnostics = diag_params
                                .diagnostics
                                .iter()
                                .map(|d| DiagnosticInfo {
                                    line: d.range.start.line,
                                    col: d.range.start.character,
                                    end_line: d.range.end.line,
                                    end_col: d.range.end.character,
                                    severity: match d.severity {
                                        Some(DiagnosticSeverity::ERROR) => DiagSeverity::Error,
                                        Some(DiagnosticSeverity::WARNING) => DiagSeverity::Warning,
                                        Some(DiagnosticSeverity::INFORMATION) => DiagSeverity::Info,
                                        Some(DiagnosticSeverity::HINT) => DiagSeverity::Hint,
                                        _ => DiagSeverity::Info,
                                    },
                                    message: d.message.clone(),
                                    source: d.source.clone(),
                                })
                                .collect();

                            let _ = event_tx.send(LspEvent::Diagnostics {
                                uri: diag_params.uri.to_string(),
                                diagnostics,
                            });
                        }
                    }
                }
                "window/logMessage" | "window/showMessage" => {
                    // can be logged but not critical for now
                }
                _ => {
                    // server-initiated requests (window/workDoneProgress, etc.)
                    // respond with null to avoid blocking the server
                    if msg.id.is_some() {
                        // ignore for now
                    }
                }
            }
        }
    }
}

// -- lsp manager --

pub struct LspManager {
    clients: HashMap<String, Arc<LspClient>>,
    cwd: PathBuf,
    event_tx: mpsc::UnboundedSender<LspEvent>,
    // aggregated diagnostics keyed by file path
    pub diagnostics: Arc<Mutex<HashMap<String, Vec<DiagnosticInfo>>>>,
}

impl LspManager {
    pub fn new(cwd: PathBuf, event_tx: mpsc::UnboundedSender<LspEvent>) -> Self {
        Self {
            clients: HashMap::new(),
            cwd,
            event_tx,
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // detect and start appropriate language servers for the project
    pub async fn start_servers(&mut self) {
        for config in SERVERS {
            // check if the server command is available
            if !command_exists(config.command) {
                continue;
            }

            // check if the project uses this language
            let root = match find_project_root(&self.cwd, config.root_markers) {
                Some(r) => r,
                None => continue,
            };

            match LspClient::start(config, &root, self.event_tx.clone()).await {
                Ok(client) => {
                    let _ = self.event_tx.send(LspEvent::ServerStarted(
                        config.name.to_string(),
                    ));
                    self.clients.insert(config.name.to_string(), Arc::new(client));
                }
                Err(e) => {
                    let _ = self.event_tx.send(LspEvent::ServerError(format!(
                        "{}: {}",
                        config.name, e
                    )));
                }
            }
        }
    }

    // find the LSP client that handles a given file extension
    fn client_for_extension(&self, ext: &str) -> Option<Arc<LspClient>> {
        for config in SERVERS {
            if config.extensions.contains(&ext) {
                if let Some(client) = self.clients.get(config.name) {
                    return Some(client.clone());
                }
            }
        }
        None
    }

    pub fn client_for_file(&self, path: &Path) -> Option<Arc<LspClient>> {
        let ext = path.extension()?.to_str()?;
        self.client_for_extension(ext)
    }

    pub async fn notify_open(&self, path: &Path, text: &str) {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => return,
        };
        let lang_id = match language_id_for_extension(ext) {
            Some(id) => id,
            None => return,
        };
        let client = match self.client_for_extension(ext) {
            Some(c) => c,
            None => return,
        };
        let uri = match path_to_uri(path) {
            Some(u) => u,
            None => return,
        };
        client.did_open(&uri, lang_id, text).await;
    }

    pub async fn notify_change(&self, path: &Path, text: &str) {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => return,
        };
        let client = match self.client_for_extension(ext) {
            Some(c) => c,
            None => return,
        };
        let uri = match path_to_uri(path) {
            Some(u) => u,
            None => return,
        };
        client.did_change(&uri, text).await;
    }

    pub async fn notify_save(&self, path: &Path, text: Option<&str>) {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => return,
        };
        let client = match self.client_for_extension(ext) {
            Some(c) => c,
            None => return,
        };
        let uri = match path_to_uri(path) {
            Some(u) => u,
            None => return,
        };
        client.did_save(&uri, text).await;
    }

    pub async fn completion(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Option<Vec<CompletionItem>> {
        let client = self.client_for_file(path)?;
        let uri = path_to_uri(path)?;
        client.completion(&uri, line, character).await.ok()
    }

    pub async fn goto_definition(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Option<Vec<Location>> {
        let client = self.client_for_file(path)?;
        let uri = path_to_uri(path)?;
        client.goto_definition(&uri, line, character).await.ok()
    }

    // store diagnostics from an LSP event
    pub async fn handle_event(&self, event: &LspEvent) {
        if let LspEvent::Diagnostics { uri, diagnostics } = event {
            if let Some(path) = uri_to_path(uri) {
                let key = path.to_string_lossy().to_string();
                let mut diags = self.diagnostics.lock().await;
                if diagnostics.is_empty() {
                    diags.remove(&key);
                } else {
                    diags.insert(key, diagnostics.clone());
                }
            }
        }
    }

    // get diagnostics for a specific file
    pub async fn diagnostics_for(&self, path: &Path) -> Vec<DiagnosticInfo> {
        let key = path.to_string_lossy().to_string();
        let diags = self.diagnostics.lock().await;
        diags.get(&key).cloned().unwrap_or_default()
    }

    // get all diagnostics formatted as a summary string (for agent injection)
    pub async fn diagnostics_summary(&self) -> Option<String> {
        let diags = self.diagnostics.lock().await;
        if diags.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        for (path, file_diags) in diags.iter() {
            let errors: Vec<&DiagnosticInfo> = file_diags
                .iter()
                .filter(|d| matches!(d.severity, DiagSeverity::Error | DiagSeverity::Warning))
                .collect();
            if errors.is_empty() {
                continue;
            }
            for d in &errors {
                let sev = match d.severity {
                    DiagSeverity::Error => "error",
                    DiagSeverity::Warning => "warning",
                    _ => "info",
                };
                // shorten path relative to cwd if possible
                lines.push(format!(
                    "{}:{}:{}: {}: {}",
                    path,
                    d.line + 1,
                    d.col + 1,
                    sev,
                    d.message
                ));
            }
        }

        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    pub async fn shutdown_all(&self) {
        for (_, client) in &self.clients {
            client.shutdown().await;
        }
    }
}
