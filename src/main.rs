mod fstar_ide;
mod logging;

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
    send_full_buffer_msg: mpsc::UnboundedSender<FullBufferMessage>,
}

struct SymbolInfo {
    symbol: String,
    range: Range,
}

impl FileBackendInternal {
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

#[derive (Copy, Clone, Debug)]
enum FragmentStatus {
    InProgress,
    LaxOk,
    Ok,
    Failed,
}

#[derive (Clone, Debug)]
enum FullBufferMessage {
    FragmentStatusUpdate {
        status_type: FragmentStatus,
        range: Range,
    },
    Error(fstar_ide::VerificationFailureResponse),
}

impl FileBackend {
    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, FileBackendInternal>> {
        self.shared.lock()
    }

    fn new(path: &str) -> (Self, mpsc::UnboundedReceiver<FullBufferMessage>) {
        let (send, recv) = mpsc::unbounded_channel();
        (FileBackend {
            shared: Arc::new(Mutex::new(
                FileBackendInternal {
                    path: path.to_string(),
                    lax_ide: Some(fstar_ide::FStarIDE::new("fstar.exe", vec![path, "--admit_smt_queries", "true"])),
                    ide: fstar_ide::FStarIDE::new("fstar.exe", vec![path]),
                    text: String::from(""),
                    send_full_buffer_msg: send,
                }
            )),
        }, recv)
    }

    async fn init(&self, text: &str) {
        let query = 
            fstar_ide::Query::VfsAdd(fstar_ide::VfsAddQuery{
                filename: None,
                contents: text.to_string(),
            })
        ;
        let mut ch = self.lock().unwrap().ide.send_query(query.clone());
        //TODO: check the response?
        while let Some(_) = ch.recv().await {}

        let opt_ch = self.lock().unwrap().lax_ide.as_mut().map(|lax_ide| lax_ide.send_query(query));

        if let Some(mut ch) = opt_ch {
            while let Some(_) = ch.recv().await {}
        }

        self.load_full_buffer(text).await;
    }

    async fn handle_full_buffer_messages_loop(mut ch: mpsc::Receiver<fstar_ide::ResponseOrMessage>, send: mpsc::UnboundedSender<FullBufferMessage>, is_lax: bool) {
        while let Some(resp_or_msg) = ch.recv().await {
            let opt_to_send = match resp_or_msg {
                fstar_ide::ResponseOrMessage::Message(fstar_ide::Message::Progress(fstar_ide::ProgressMessageOrNull::Some(data))) => {
                    match data {
                        fstar_ide::ProgressMessage::FullBufferFragmentStarted{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate {
                                status_type: FragmentStatus::InProgress,
                                range: ranges.into(),
                            })
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentFailed{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate {
                                status_type: FragmentStatus::Failed,
                                range: ranges.into(),
                            })
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentLaxOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate {
                                status_type: FragmentStatus::LaxOk,
                                range: fragment.ranges.into(),
                            })
                        },
                        fstar_ide::ProgressMessage::FullBufferFragmentOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate {
                                status_type: if is_lax { FragmentStatus::LaxOk } else { FragmentStatus::Ok },
                                range: fragment.ranges.into(),
                            })
                        },
                        _ => None
                    }
                }
                fstar_ide::ResponseOrMessage::Response(fstar_ide::Response{status: fstar_ide::ResponseStatus::Failure, response}) => {
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
            if let Some(to_send) = opt_to_send {
                send.send(to_send).unwrap();
            }
        }
    }

    async fn load_full_buffer(&self, text: &str) {
        self.lock().unwrap().text = text.to_string();
        let ch = self.lock().unwrap().ide.send_query(
            fstar_ide::Query::FullBuffer(fstar_ide::FullBufferQuery{
                code: text.to_string(),
                kind: fstar_ide::FullBufferKind::Cache,
                with_symbols: false,
            })
        );
        //TODO: do something with the responses
        let msg_send = self.lock().unwrap().send_full_buffer_msg.clone();
        tokio::spawn(Self::handle_full_buffer_messages_loop(ch, msg_send.clone(), false));

        let opt_ch_lax = self.lock().unwrap().lax_ide.as_mut().map(|lax_ide|
            lax_ide.send_query(
                fstar_ide::Query::FullBuffer(fstar_ide::FullBufferQuery{
                    code: text.to_string(),
                    kind: fstar_ide::FullBufferKind::Full,
                    with_symbols: false,
                })
            )
        );

        let msg_send = self.lock().unwrap().send_full_buffer_msg.clone();
        match opt_ch_lax {
            None => (),
            Some(ch_lax) => {
                tokio::spawn(Self::handle_full_buffer_messages_loop(ch_lax, msg_send.clone(), true));
            }
        }

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
        let mut ch = self.lock().unwrap().get_flycheck_ide().send_query(
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
}

struct Backend {
    client: Arc<Client>,
    ides: RwLock<std::collections::HashMap<String, FileBackend>>,
}

impl Backend {
    async fn receive_full_buffer_message_loop(client: Arc<Client>, mut recv: mpsc::UnboundedReceiver<FullBufferMessage>) {
        while let Some(msg) = recv.recv().await {
            info!("got msg {:?}", msg);
        }
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
        tokio::spawn(Self::receive_full_buffer_message_loop(self.client.clone(), recv));
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        assert!(params.content_changes.len() == 1);
        assert!(params.content_changes[0].range.is_none());
        assert!(params.content_changes[0].range_length.is_none());
        let path = params.text_document.uri.path();
        let ide = self.ides.read().unwrap().get(path).unwrap().clone();
        let new_text = &params.content_changes[0].text;
        ide.load_full_buffer(new_text).await;
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let path = params.text_document.uri.path();
        let _ = self.ides.write().unwrap().remove(path);
    }

    async fn completion(&self, _: CompletionParams) -> Result<Option<CompletionResponse>> {
        Ok(Some(CompletionResponse::Array(vec![
            CompletionItem::new_simple("Hello".to_string(), "Some detail".to_string()),
            CompletionItem::new_simple("Bye".to_string(), "More detail".to_string()),
        ])))
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
    let (service, socket) = LspService::new(|client| Backend { client: Arc::new(client), ides: RwLock::new(std::collections::HashMap::new()) });
    Server::new(stdin, stdout, socket).serve(service).await;
}
