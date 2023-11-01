pub mod file_backend;
use file_backend::*;

use crate::fstar;

use tower_lsp::lsp_types::notification::Notification;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use std::sync::{Arc, RwLock};

use tracing::{
    trace,
    debug,
    info,
    warn,
    error,
};


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

    #[tracing::instrument(skip(self))]
    fn process_error(&mut self, ide_type: IdeType, error: fstar::VerificationFailureResponseItem) {
        let severity = match error.level {
            fstar::VerificationFailureLevel::Error => DiagnosticSeverity::ERROR,
            fstar::VerificationFailureLevel::Warning => DiagnosticSeverity::WARNING,
            fstar::VerificationFailureLevel::Info => DiagnosticSeverity::INFORMATION,
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


pub struct Backend {
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

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ReloadDependenciesParams {
    text_document: TextDocumentIdentifier,
}

#[derive (Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct RestartZ3Params {
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
    pub fn new(client: Client) -> Self {
        Backend {
            client: Arc::new(client),
            ides: RwLock::new(std::collections::HashMap::new())
        }
    }

    #[tracing::instrument(skip_all, fields(path = uri.path()))]
    async fn receive_full_buffer_message_loop(uri: Url, client: Arc<Client>, mut recv: mpsc::UnboundedReceiver<IdeFullBufferMessage>) {
        let mut diagnostic_state_machine = DiagnosticsStateMachine::new(uri.clone(), client.clone());
        let mut status_state_machine = StatusStateMachine::new(uri.clone(), client.clone());

        while let Some(msg) = recv.recv().await {
            debug!("received message {:?}", msg);
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

    #[tracing::instrument(skip(self))]
    pub async fn verify_all(&self, params: serde_json::Value) {
        debug!("start");
        let params: VerifyAllParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.verify_full_buffer();
    }

    #[tracing::instrument(skip(self))]
    pub async fn lax_to_position(&self, params: serde_json::Value) {
        debug!("start");
        let params: LaxToPositionParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.lax_to_position(params.position);
    }

    #[tracing::instrument(skip(self))]
    pub async fn verify_to_position(&self, params: serde_json::Value) {
        debug!("start");
        let params: VerifyToPositionParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.verify_to_position(params.position);
    }

    #[tracing::instrument(skip(self))]
    pub async fn cancel_all(&self, params: serde_json::Value) {
        debug!("start");
        let params: CancelAllParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.cancel_all().await;
    }

    #[tracing::instrument(skip(self))]
    pub async fn reload_dependencies(&self, params: serde_json::Value) {
        debug!("start");
        let params: ReloadDependenciesParams = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.reload_dependencies();
    }

    #[tracing::instrument(skip(self))]
    pub async fn restart_z3(&self, params: serde_json::Value) {
        debug!("start");
        let params: RestartZ3Params = match serde_json::from_value(params) {
            Ok(x) => x,
            Err(e) => {
                error!("couldn't parse: {}", e);
                return
            }
        };
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        ide.restart_z3();
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    #[tracing::instrument(skip(self))]
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

    #[tracing::instrument(skip(self))]
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "initialized!")
            .await;
    }

    #[tracing::instrument(skip(self))]
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

    #[tracing::instrument(skip(self))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let path = params.text_document.uri.path();
        let (ide, recv) = FileBackend::new(path);
        self.ides.write().unwrap().insert(path.to_string(), ide.clone());
        ide.init(&params.text_document.text).await;
        tokio::spawn(Self::receive_full_buffer_message_loop(params.text_document.uri, self.client.clone(), recv));
    }

    #[tracing::instrument(skip_all)]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        debug!("start");
        assert!(params.content_changes.len() == 1);
        assert!(params.content_changes[0].range.is_none());
        assert!(params.content_changes[0].range_length.is_none());
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let new_text = &params.content_changes[0].text;
        ide.handle_full_buffer_change(new_text);
    }

    #[tracing::instrument(skip(self))]
    async fn did_save(&self, _: DidSaveTextDocumentParams) {
    }

    #[tracing::instrument(skip(self))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let path = params.text_document.uri.path();
        let _ = self.ides.write().unwrap().remove(path);
    }

    #[tracing::instrument(skip_all)]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let path = params.text_document_position.text_document.uri.path();
        let pos = params.text_document_position.position;
        info!("on {:?} {:?}", path, pos);
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let result = ide.complete(pos).await ;
        info!("result: {:?}", result);
        match result {
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

    #[tracing::instrument(skip_all)]
    async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let path = params.text_document_position_params.text_document.uri.path();
        let pos = params.text_document_position_params.position;
        info!("on {:?} {:?}", path, pos);
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let result = ide.goto_definition(pos).await;
        info!("result: {:?}", result);
        match result {
            None => Ok(None),
            Some(GotoDefinitionResult { file, range }) => {
                let uri = match file {
                    GotoDefinitionResultFile::ThisFile =>
                        params.text_document_position_params.text_document.uri,
                    GotoDefinitionResultFile::OtherFile(path) => {
                        let path = std::path::Path::new(&path);
                        let absolute_path = 
                            if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                ide.get_working_dir().join(path)
                            }
                        ;
                        Url::from_file_path(absolute_path).unwrap()
                    }
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

    #[tracing::instrument(skip_all)]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let path = params.text_document_position_params.text_document.uri.path();
        let pos = params.text_document_position_params.position;
        info!("on {:?} {:?}", path, pos);
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let hover_result = ide.hover(pos).await;
        info!("result: {:?}", hover_result);
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
