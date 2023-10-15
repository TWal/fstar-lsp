mod fstar_ide;
mod logging;
mod sync_channel;

use crate::notification::Notification;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use std::sync::{Arc, RwLock, Mutex};

struct FileBackendInternal {
    path: String,
    lax_ide: Option<fstar_ide::FStarIDE>,
    ide: fstar_ide::FStarIDE,
    text: String,
    send_full_buffer_msg: mpsc::UnboundedSender<IdeFullBufferMessage>,
}

struct SymbolInfo {
    symbol: String,
    range: Range,
}

#[derive (Deserialize)]
struct Config {
    fstar_exe: Option<String>,
    options: Option<Vec<String>>,
    include_dirs: Option<Vec<String>>,
    cwd: Option<String>,
}

fn find_config_file(path: &str) -> Option<(Config, std::path::PathBuf)> {
    let base_directory_path = std::path::Path::new(path).parent()?;
    for dir in base_directory_path.ancestors() {
        if let Ok(dir_content) = std::fs::read_dir(dir) {
            for entry in dir_content {
                if let Ok(entry) = entry {
                    if let Some(file_name) = entry.file_name().to_str() {
                        if file_name.ends_with(".fst.config.json") {
                            match std::fs::read_to_string(entry.path()) {
                                Err(e) => {
                                    error!("Found config file {} but could not read it: {}", entry.path().display(), e);
                                    return None
                                },
                                Ok(contents) => {
                                    match serde_json::from_str::<Config>(&contents) {
                                        Err(e) => {
                                            error!("Found config file {} but could not parse it: {}", entry.path().display(), e);
                                            return None;
                                        }
                                        Ok(config) => {
                                            return Some((config, dir.to_path_buf()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

impl FileBackendInternal {
    fn new(path: &str, send_full_buffer_msg: mpsc::UnboundedSender<IdeFullBufferMessage>) -> Self {
        let config = find_config_file(path);
        let additional_options = match &config {
            None => vec![],
            Some((config, _)) => {
                let options: Vec<&str> = match &config.options {
                    None => vec![],
                    Some(opts) => opts.iter().map(|opt| opt.as_str()).collect(),
                };
                let includes: Vec<&str> = match &config.include_dirs {
                    None => vec![],
                    Some(include_dirs) => {
                        include_dirs
                            .iter()
                            .flat_map(|include_dir| vec!["--include", include_dir.as_str()].into_iter())
                            .collect()
                    },
                };
                [options, includes].concat()
            },
        };
        let working_dir = match &config {
            None => std::path::Path::new(path).parent().unwrap_or(std::path::Path::new(".")).to_path_buf(),
            Some((config, config_dir)) => {
                let config_dir = config_dir.clone();
                match &config.cwd {
                    None => config_dir,
                    Some(cwd) => config_dir.as_path().join(std::path::Path::new(cwd.as_str()))
                }
            },
        };

        FileBackendInternal {
            path: path.to_string(),
            lax_ide: Some(fstar_ide::FStarIDE::new("fstar.exe", [vec![path, "--admit_smt_queries", "true"], additional_options.clone()].concat(), working_dir.as_path())),
            ide: fstar_ide::FStarIDE::new("fstar.exe", [vec![path], additional_options].concat(), working_dir.as_path()),
            text: String::from(""),
            send_full_buffer_msg,
        }
    }

    fn get_symbol_at(&self, pos: Position) -> Option<SymbolInfo> {
        let line = self.text.lines().skip(pos.line as usize).next().unwrap();
        let mut range = None;
        let mut range_start = None;
        for (i, c) in line.char_indices().chain(std::iter::repeat((line.len(), '\n')).take(1)) {
            let c_is_ident_char = c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '\'';
            if let Some(start) = range_start {
                if !c_is_ident_char && start <= pos.character as usize && (pos.character as usize) <= i {
                    range = Some((start, i));
                    break;
                }
            }
            if range_start == None && c_is_ident_char {
                range_start = Some(i)
            }
            if !c_is_ident_char {
                range_start = None
            }
        }
        range.map(|(start, end)| SymbolInfo {
            symbol: line.chars().skip(start).take(end-start).collect(),
            range: Range {
                start: Position { character: start as u32, ..pos },
                end: Position { character: end as u32, ..pos },
            }
        })
    }

    fn get_flycheck_ide(&mut self) -> &mut fstar_ide::FStarIDE {
        match self.lax_ide.as_mut() {
            None => {
                &mut self.ide
            },
            Some(lax_ide) => {
                lax_ide
            }
        }
    }
}

#[derive(Clone)]
struct FileBackend {
    shared: Arc<Mutex<FileBackendInternal>>,
}

enum HoverResultText {
    NoDocumentation {
        full_name: String,
        type_: String,
    },
    WithDocumentation {
        full_name: String,
        type_: String,
        documentation: String,
    }
}

struct HoverResult {
    result: HoverResultText,
    range: Range,
}

enum GotoDefinitionResultFile {
    ThisFile,
    OtherFile(String),
}

struct GotoDefinitionResult {
    file: GotoDefinitionResultFile,
    range: Range,
}

#[derive (Clone, Debug)]
struct CompleteResultItem {
    match_length: u32,
    annotation: String,
    candidate: String,
}

type CompleteResult = Vec<CompleteResultItem>;

#[derive (PartialEq, Copy, Clone, Debug, Serialize, Deserialize)]
enum FragmentStatus {
    InProgress,
    LaxOk,
    Ok,
    Failed,
    Canceled,
}

#[derive (Clone, Debug)]
struct FragmentStatusUpdate {
    status_type: FragmentStatus,
    range: Range,
}

#[derive (Clone, Debug)]
enum FullBufferMessage {
    Started,
    FragmentStatusUpdate(FragmentStatusUpdate),
    Finished,
    Error(fstar_ide::VerificationFailureResponse),
}

#[derive (PartialEq, Copy, Clone, Debug)]
enum IdeType {
    Lax,
    Full,
}

#[derive (Clone, Debug)]
struct IdeFullBufferMessage {
    ide_type: IdeType,
    message: FullBufferMessage,
}

impl FileBackend {
    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, FileBackendInternal>> {
        self.shared.lock()
    }

    fn new(path: &str) -> (Self, mpsc::UnboundedReceiver<IdeFullBufferMessage>) {
        let (send, recv) = mpsc::unbounded_channel();
        (FileBackend {
            shared: Arc::new(Mutex::new( FileBackendInternal::new(path, send))),
        }, recv)
    }

    async fn init(&self, text: &str) {
        let query =
            fstar_ide::Query::VfsAdd(fstar_ide::VfsAddQuery{
                filename: None,
                contents: text.to_string(),
            })
        ;
        let mut ch = self.lock().unwrap().ide.send_query_nosync(query.clone());
        //TODO: check the response?
        while let Some(_) = ch.recv().await {}

        let opt_ch = self.lock().unwrap().lax_ide.as_mut().map(|lax_ide| lax_ide.send_query_nosync(query));

        if let Some(mut ch) = opt_ch {
            while let Some(_) = ch.recv().await {}
        }

        self.handle_full_buffer_change(text);
    }

    async fn handle_full_buffer_messages_loop(mut ch: sync_channel::Receiver<fstar_ide::ResponseOrMessage>, send: mpsc::UnboundedSender<IdeFullBufferMessage>, ide_type: IdeType) {
        while let Some((resp_or_msg, acker)) = ch.recv().await {
            let opt_message = match resp_or_msg {
                fstar_ide::ResponseOrMessage::Message(fstar_ide::Message::Progress(fstar_ide::ProgressMessageOrNull::Some(data))) => {
                    match data {
                        fstar_ide::ProgressMessage::FullBufferFragmentStarted{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::InProgress,
                                range: ranges.into(),
                            }))
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentFailed{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::Failed,
                                range: ranges.into(),
                            }))
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentLaxOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::LaxOk,
                                range: fragment.ranges.into(),
                            }))
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: if ide_type == IdeType::Lax { FragmentStatus::LaxOk } else { FragmentStatus::Ok },
                                range: fragment.ranges.into(),
                            }))
                        },
                        fstar_ide::ProgressMessage::FullBufferStarted => {
                            Some(FullBufferMessage::Started)
                        },
                        fstar_ide::ProgressMessage::FullBufferFinished => {
                            Some(FullBufferMessage::Finished)
                        },
                        _ => None
                    }
                }
                fstar_ide::ResponseOrMessage::Response(fstar_ide::Response{status: _, response}) => {
                    match serde_json::from_value::<fstar_ide::VerificationFailureResponse>(response) {
                        Ok(x) => Some(FullBufferMessage::Error(x)),
                        Err(e) => {
                            error!("[full-buffer] Couldn't parse failure response: {}", e);
                            None
                        }
                    }
                }
                _ => None
            };
            if let Some(message) = opt_message {
                send.send(IdeFullBufferMessage{
                    ide_type,
                    message,
                }).unwrap();
            }
            acker.ack();
        }
    }

    fn send_full_buffer_query(&self, ide_type: IdeType, kind: fstar_ide::FullBufferKind) {
        let code = self.lock().unwrap().text.clone();
        let full_buffer_message = 
            fstar_ide::Query::FullBuffer(fstar_ide::FullBufferQuery{
                code,
                kind,
                with_symbols: false,
            })
        ;

        let opt_ch = match ide_type {
            IdeType::Full => {
                Some(self.lock().unwrap().ide.send_query_sync(full_buffer_message))
            },
            IdeType::Lax => {
                self.lock().unwrap().lax_ide.as_mut().map(|lax_ide| lax_ide.send_query_sync(full_buffer_message))
            },
        };

        match opt_ch {
            None => (),
            Some(ch) => {
                let msg_send = self.lock().unwrap().send_full_buffer_msg.clone();
                tokio::spawn(Self::handle_full_buffer_messages_loop(ch, msg_send, ide_type));
            }
        }
    }

    fn handle_full_buffer_change(&self, text: &str) {
        self.lock().unwrap().text = text.to_string();

        self.send_full_buffer_query(IdeType::Full, fstar_ide::FullBufferKind::Cache);
        self.send_full_buffer_query(IdeType::Lax, fstar_ide::FullBufferKind::Full);
    }

    fn verify_full_buffer(&self) {
        self.send_full_buffer_query(IdeType::Full, fstar_ide::FullBufferKind::Full);
    }

    fn verify_to_position(&self, pos: Position) {
        self.send_full_buffer_query(IdeType::Full, fstar_ide::FullBufferKind::VerifyToPosition(fstar_ide::BarePosition::from(pos)))
    }

    fn lax_to_position(&self, pos: Position) {
        self.send_full_buffer_query(IdeType::Full, fstar_ide::FullBufferKind::LaxToPosition(fstar_ide::BarePosition::from(pos)))
    }


    async fn cancel_all(&self) {
        let _ch = self.lock().unwrap().ide.send_query_nosync(fstar_ide::Query::Cancel(fstar_ide::CancelPosition{cancel_line: 1, cancel_column: 0}));
        // the Cancel query doesn't send back any message -- ignore
    }

    async fn hover(&self, pos: Position) -> Option<HoverResult> {
        let (response, range) = self.lookup_query(vec![fstar_ide::LookupRequest::Type], pos).await?;
        let response_type = match response.type_ {
            Some(x) => x,
            None => {
                error!("[hover] expected a type to be present");
                return None;
            }
        };
        Some(HoverResult {
            result: HoverResultText::NoDocumentation {
                full_name: response.name,
                type_: response_type,
            },
            range,
        })
    }

    async fn goto_definition(&self, pos: Position) -> Option<GotoDefinitionResult> {
        let (response, _symbol_range) = self.lookup_query(vec![fstar_ide::LookupRequest::DefinedAt], pos).await?;
        let response_defined_at = match response.defined_at {
            Some(x) => x,
            None => {
                error!("[goto_definition] expected a definition location to be present");
                return None;
            }
        };
        Some(GotoDefinitionResult {
            file:
                if response_defined_at.fname == "<input>" {
                    GotoDefinitionResultFile::ThisFile
                } else {
                    GotoDefinitionResultFile::OtherFile(response_defined_at.fname.clone())
                },
            range: response_defined_at.into(),
        })
    }

    async fn lookup_query(&self, info: Vec<fstar_ide::LookupRequest>, pos: Position) -> Option<(fstar_ide::LookupResponse, Range)> {
        let symbol = self.lock().unwrap().get_symbol_at(pos);
        let Some(symbol) = symbol else {
            return None
        };
        let path = self.lock().unwrap().path.clone();
        let mut ch = self.lock().unwrap().get_flycheck_ide().send_query_nosync(
            fstar_ide::Query::Lookup(fstar_ide::LookupQuery {
                context: fstar_ide::LookupContext::Code,
                symbol: symbol.symbol.clone(),
                requested_info: info,
                location: Some(fstar_ide::Position::from(path, pos)),
            })
        );
        let resp_or_msg = ch.recv().await;
        let Some(fstar_ide::ResponseOrMessage::Response(fstar_ide::Response{status, response})) = resp_or_msg else { panic!("lookup A"); };
        if status != fstar_ide::ResponseStatus::Success {
            return None
        }
        let response = match serde_json::from_value::<fstar_ide::LookupResponse>(response) {
            Ok(x) => x,
            Err(e) => {
                error!("[lookup] Couldn't parse response: {}", e);
                return None
            }
        };
        if response.kind != *"symbol" {
            error!("[lookup] Response's kind is not symbol: {}", response.kind);
            return None
        }
        Some((response, symbol.range))
    }

    async fn complete(&self, pos: Position) -> Option<CompleteResult> {
        let symbol = self.lock().unwrap().get_symbol_at(pos);
        let Some(symbol) = symbol else {
            return None
        };
        let mut ch = self.lock().unwrap().get_flycheck_ide().send_query_nosync(
            fstar_ide::Query::AutoComplete(fstar_ide::AutoCompleteQuery {
                context: fstar_ide::AutoCompleteContext::Code,
                partial_symbol: symbol.symbol,
            })
        );
        let resp_or_msg = ch.recv().await;

        let Some(fstar_ide::ResponseOrMessage::Response(fstar_ide::Response{status, response})) = resp_or_msg
        else {
            error!("[complete] expected a response, got {:?}", resp_or_msg);
            return None
        };
        if status != fstar_ide::ResponseStatus::Success {
            return None
        }
        match serde_json::from_value::<fstar_ide::AutoCompleteResponse>(response) {
            Ok(response) => {
                Some(
                    response.into_iter()
                        .map(|fstar_ide::AutoCompleteResponseItem(match_length, annotation, candidate)| CompleteResultItem {match_length, annotation, candidate})
                        .filter(|cri| cri.annotation != "<search term>")
                        .collect()
                )
            }
            Err(err) => {
                error!("[complete] couldn't parse response: {}", err);
                return None
            }
        }
    }
}

struct DiagnosticsStateMachine {
    uri: Url,
    client: Arc<Client>,
    lax_diagnostics: Vec<Diagnostic>,
    full_diagnostics: Vec<Diagnostic>,
}

impl DiagnosticsStateMachine {
    fn new(uri: Url, client: Arc<Client>) -> Self {
        DiagnosticsStateMachine {
            uri,
            client,
            lax_diagnostics: vec![],
            full_diagnostics: vec![],
        }
    }

    fn get_vec_for(&mut self, ide_type: IdeType) -> &mut Vec<Diagnostic> {
        match ide_type {
            IdeType::Lax => &mut self.lax_diagnostics,
            IdeType::Full => &mut self.full_diagnostics,
        }
    }

    fn start(&mut self, ide_type: IdeType) {
        self.get_vec_for(ide_type).clear();
    }

    fn process_error(&mut self, ide_type: IdeType, error: fstar_ide::VerificationFailureResponseItem) {
        let severity = match error.level {
            fstar_ide::VerificationFailureLevel::Error => DiagnosticSeverity::ERROR,
            fstar_ide::VerificationFailureLevel::Warning => DiagnosticSeverity::WARNING,
            fstar_ide::VerificationFailureLevel::Info => DiagnosticSeverity::INFORMATION,
        };
        let (range, message) = {
            let range0 = &error.ranges[0];
            if range0.fname == "<input>" {
                (range0.clone().into(), error.message.clone())
            } else {
                (Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 0 },
                },
                format!("In dependency {}: {}", range0, error.message))
            }
        };
        let message = vec![message].into_iter().chain(
            error.ranges.iter()
            .skip(1)
            .map(|x| format!("See also: {}", x))
        ).collect::<Vec<String>>().join("\n");
        let diagnostic = Diagnostic {
            range,
            severity: Some(severity),
            code: Some(NumberOrString::Number(error.number as i32)),
            code_description: None,
            source: None,
            message,
            related_information: None,
            tags: None,
            data: None,
        };
        self.get_vec_for(ide_type).push(diagnostic);
    }

    async fn finish(&self, ide_type: IdeType) {
        let all_diagnostics: Vec<Diagnostic> =
            self.lax_diagnostics.clone().into_iter()
            .chain(self.full_diagnostics.clone().into_iter())
            .unique_by(|diag| (diag.range.start.line, diag.range.start.character, diag.range.end.line, diag.range.end.character, diag.message.clone())) // send a tuple to be able to hash
            .collect()
        ;
        self.client.publish_diagnostics(self.uri.clone(), all_diagnostics, None).await;
    }
}

struct StatusStateMachine {
    uri: Url,
    client: Arc<Client>,
    last_status: Option<FragmentStatusUpdate>,
}

impl StatusStateMachine {
    fn new(uri: Url, client: Arc<Client>) -> Self {
        StatusStateMachine {
            uri,
            client,
            last_status: None,
        }
    }

    async fn start(&mut self) {
        self.last_status = None;
        self.client.send_notification::<ClearStatusNotification>(ClearStatusNotificationParams{
            uri: self.uri.clone(),
        }).await;
    }

    async fn process_fragment_status_update(&mut self, msg: FragmentStatusUpdate) {
        self.client.send_notification::<SetStatusNotification>(SetStatusNotificationParams {
            uri: self.uri.clone(),
            status_type: msg.status_type,
            range: msg.range,
        }).await;
        self.last_status = Some(msg)
    }

    async fn finish(&self) {
        if let Some(last_status) = &self.last_status {
            if last_status.status_type == FragmentStatus::InProgress {
                self.client.send_notification::<SetStatusNotification>(SetStatusNotificationParams {
                    uri: self.uri.clone(),
                    status_type: FragmentStatus::Canceled,
                    range: last_status.range,
                }).await;
            }
        }
    }
}


struct Backend {
    client: Arc<Client>,
    ides: RwLock<std::collections::HashMap<String, FileBackend>>,
}

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct VerifyAllParams {
    text_document: TextDocumentIdentifier,
}

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct LaxToPositionParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct VerifyToPositionParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct CancelAllParams {
    text_document: TextDocumentIdentifier,
}

struct ClearStatusNotification {}
#[derive(Serialize, Deserialize)]
struct ClearStatusNotificationParams {
    uri: Url,
}

impl Notification for ClearStatusNotification {
    type Params = ClearStatusNotificationParams;
    const METHOD: &'static str = "fstar-lsp/clearStatus";
}

struct SetStatusNotification {}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetStatusNotificationParams {
    uri: Url,
    status_type: FragmentStatus,
    range: Range,
}

impl Notification for SetStatusNotification {
    type Params = SetStatusNotificationParams;
    const METHOD: &'static str = "fstar-lsp/setStatus";
}


impl Backend {
    async fn receive_full_buffer_message_loop(uri: Url, client: Arc<Client>, mut recv: mpsc::UnboundedReceiver<IdeFullBufferMessage>) {
        let mut diagnostic_state_machine = DiagnosticsStateMachine::new(uri.clone(), client.clone());
        let mut status_state_machine = StatusStateMachine::new(uri.clone(), client.clone());

        while let Some(msg) = recv.recv().await {
            info!("got msg {:?}", msg);
            // Handle diagnostics
            match msg.message.clone() {
                FullBufferMessage::Started => {
                    diagnostic_state_machine.start(msg.ide_type);
                }
                FullBufferMessage::Error(errors) => {
                    for error in errors {
                        diagnostic_state_machine.process_error(msg.ide_type, error);
                    }
                },
                FullBufferMessage::Finished => {
                    diagnostic_state_machine.finish(msg.ide_type).await;
                }
                _ => ()
            }

            // Handle verification status
            if msg.ide_type == IdeType::Full {
                match msg.message {
                    FullBufferMessage::Started => {
                        status_state_machine.start().await
                    }
                    FullBufferMessage::FragmentStatusUpdate(upd) => {
                        status_state_machine.process_fragment_status_update(upd).await;
                    }
                    FullBufferMessage::Finished => {
                        status_state_machine.finish().await;
                    }
                    _ => ()
                }
            }
        }
    }

    async fn verify_all(&self, params: serde_json::Value) {
        let params: VerifyAllParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("[verify_all]: couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.verify_full_buffer();
    }

    async fn lax_to_position(&self, params: serde_json::Value) {
        let params: LaxToPositionParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("[lax_to_position]: couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.lax_to_position(params.position);
    }

    async fn verify_to_position(&self, params: serde_json::Value) {
        let params: VerifyToPositionParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("[verify_to_position]: couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.verify_to_position(params.position);
    }

    async fn cancel_all(&self, params: serde_json::Value) {
        let params: CancelAllParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("[cancel_all]: couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.cancel_all().await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    // TODO: do incremental!
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                // document range formatting?
                // TODO keep workspace?
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..ServerCapabilities::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // async fn did_change_workspace_folders(&self, _: DidChangeWorkspaceFoldersParams) {
    //     self.client
    //         .log_message(MessageType::INFO, "workspace folders changed!")
    //         .await;
    // }
    //
    // async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
    //     self.client
    //         .log_message(MessageType::INFO, "configuration changed!")
    //         .await;
    // }
    //
    // async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
    //     self.client
    //         .log_message(MessageType::INFO, "watched files have changed!")
    //         .await;
    // }
    //
    // async fn execute_command(&self, _: ExecuteCommandParams) -> Result<Option<Value>> {
    //     self.client
    //         .log_message(MessageType::INFO, "command executed!")
    //         .await;
    //
    //     match self.client.apply_edit(WorkspaceEdit::default()).await {
    //         Ok(res) if res.applied => self.client.log_message(MessageType::INFO, "applied").await,
    //         Ok(_) => self.client.log_message(MessageType::INFO, "rejected").await,
    //         Err(err) => self.client.log_message(MessageType::ERROR, err).await,
    //     }
    //
    //     Ok(None)
    // }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let path = params.text_document.uri.path();
        let (ide, recv) = FileBackend::new(path);
        self.ides.write().unwrap().insert(path.to_string(), ide.clone());
        ide.init(&params.text_document.text).await;
        tokio::spawn(Self::receive_full_buffer_message_loop(params.text_document.uri, self.client.clone(), recv));
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        assert!(params.content_changes.len() == 1);
        assert!(params.content_changes[0].range.is_none());
        assert!(params.content_changes[0].range_length.is_none());
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let new_text = &params.content_changes[0].text;
        ide.handle_full_buffer_change(new_text);
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let path = params.text_document.uri.path();
        let _ = self.ides.write().unwrap().remove(path);
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let path = params.text_document_position.text_document.uri.path();
        let pos = params.text_document_position.position;
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        match ide.complete(pos).await {
            None => Ok(None),
            Some(completion_result) => {
                Ok(Some(CompletionResponse::Array(
                    completion_result.into_iter()
                        .map(|item|
                            CompletionItem {
                                label: item.candidate,
                                label_details: Some(CompletionItemLabelDetails {
                                    detail: Some(item.annotation),
                                    description: None,
                                }),
                                kind: Some(CompletionItemKind::FUNCTION),
                                ..Default::default()
                            }
                        )
                        .collect()
                )))
            }
        }
    }

    async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let path = params.text_document_position_params.text_document.uri.path();
        let pos = params.text_document_position_params.position;
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let result = ide.goto_definition(pos).await;
        match result {
            None => Ok(None),
            Some(GotoDefinitionResult { file, range }) => {
                let uri = match file {
                    GotoDefinitionResultFile::ThisFile =>
                        params.text_document_position_params.text_document.uri,
                    GotoDefinitionResultFile::OtherFile(path) =>
                        Url::from_file_path(std::path::Path::new(&path)).unwrap(),
                };
                Ok(Some(GotoDefinitionResponse::Scalar(
                    Location{
                        uri,
                        range,
                    }
                )))
            }
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let path = params.text_document_position_params.text_document.uri.path();
        let pos = params.text_document_position_params.position;
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let hover_result = ide.hover(pos).await;
        match hover_result {
            None => Ok(None),
            Some(HoverResult { result, range }) => {
                match result {
                    HoverResultText::NoDocumentation { full_name, type_ } => {
                        let res = format!("{}: {}", full_name, type_);
                        Ok(Some(Hover{
                            contents: HoverContents::Scalar(MarkedString::from_language_code(String::from("fstar"), res)),
                            //contents: HoverContents::Scalar(MarkedString::from_markdown(res)),
                            range: Some(range),
                        }))
                    }
                    HoverResultText::WithDocumentation { full_name, type_, documentation } => {
                        panic!()
                    }
                }
            }
        }
    }

}

#[tokio::main]
async fn main() {
    //tracing_subscriber::fmt().init();

    logging::set_log_level(logging::LogLevel::Info);
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let (service, socket) = LspService::build(
        |client| Backend { client: Arc::new(client), ides: RwLock::new(std::collections::HashMap::new()) })
        .custom_method("fstar-lsp/verifyAll", Backend::verify_all)
        .custom_method("fstar-lsp/laxToPosition", Backend::lax_to_position)
        .custom_method("fstar-lsp/verifyToPosition", Backend::verify_to_position)
        .custom_method("fstar-lsp/cancelAll", Backend::cancel_all)
        .finish()
    ;
    Server::new(stdin, stdout, socket).serve(service).await;
}
