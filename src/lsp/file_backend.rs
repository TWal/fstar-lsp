use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_lsp::lsp_types::*;
use std::sync::{Arc, Mutex};
use crate::fstar;
use crate::sync_channel;

use tracing::{
    // trace,
    // debug,
    // info,
    warn,
    error,
};


#[derive (Deserialize)]
struct Config {
    #[allow(dead_code)]
    fstar_exe: Option<String>,
    options: Option<Vec<String>>,
    include_dirs: Option<Vec<String>>,
    cwd: Option<String>,
}

fn find_config_file(path: &str) -> Option<(Config, std::path::PathBuf)> {
    let base_directory_path = std::path::Path::new(path).parent()?;
    for dir in base_directory_path.ancestors() {
        if let Ok(dir_content) = std::fs::read_dir(dir) {
            for entry in dir_content.flatten() {
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
    None
}

struct FileBackendInternal {
    path: String,
    working_dir: std::path::PathBuf,
    lax_ide: Option<fstar::ide::FStarIDE>,
    ide: fstar::ide::FStarIDE,
    text: String,
    send_full_buffer_msg: mpsc::UnboundedSender<IdeFullBufferMessage>,
}

struct SymbolInfo {
    symbol: String,
    range: Range,
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
            working_dir: working_dir.clone(),
            lax_ide: Some(fstar::ide::FStarIDE::new("fstar.exe", [vec![path, "--admit_smt_queries", "true"], additional_options.clone()].concat(), working_dir.as_path())),
            ide: fstar::ide::FStarIDE::new("fstar.exe", [vec![path], additional_options].concat(), working_dir.as_path()),
            text: String::from(""),
            send_full_buffer_msg,
        }
    }

    fn get_symbol_at(&self, pos: Position) -> Option<SymbolInfo> {
        let line = self.text.lines().nth(pos.line as usize).unwrap();
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
            if range_start.is_none() && c_is_ident_char {
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

    fn get_flycheck_ide(&mut self) -> &mut fstar::ide::FStarIDE {
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
pub struct FileBackend {
    shared: Arc<Mutex<FileBackendInternal>>,
}

#[derive(Debug)]
pub enum HoverResultText {
    NoDocumentation {
        full_name: String,
        type_: String,
    },
    #[allow(dead_code)]
    WithDocumentation {
        full_name: String,
        type_: String,
        documentation: String,
    }
}

#[derive(Debug)]
pub struct HoverResult {
    pub result: HoverResultText,
    pub range: Range,
}

#[derive (Debug)]
pub enum GotoDefinitionResultFile {
    ThisFile,
    OtherFile(String),
}

#[derive (Debug)]
pub struct GotoDefinitionResult {
    pub file: GotoDefinitionResultFile,
    pub range: Range,
}

#[derive (Clone, Debug)]
pub struct CompleteResultItem {
    pub match_length: u32,
    pub annotation: String,
    pub candidate: String,
}

pub type CompleteResult = Vec<CompleteResultItem>;

#[derive (PartialEq, Copy, Clone, Debug, Serialize, Deserialize)]
pub enum FragmentStatus {
    InProgress,
    LaxOk,
    Ok,
    Failed,
    Canceled,
}

#[derive (Clone, Debug)]
pub struct FragmentStatusUpdate {
    pub status_type: FragmentStatus,
    pub range: Range,
}

#[derive (Clone, Debug)]
pub enum FullBufferMessage {
    Started,
    FragmentStatusUpdate(FragmentStatusUpdate),
    Finished,
    Error(fstar::VerificationFailureResponse),
}

#[derive (PartialEq, Copy, Clone, Debug)]
pub enum IdeType {
    Lax,
    Full,
}

#[derive (Clone, Debug)]
pub struct IdeFullBufferMessage {
    pub ide_type: IdeType,
    pub message: FullBufferMessage,
}

impl FileBackend {
    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, FileBackendInternal>> {
        self.shared.lock()
    }

    pub fn new(path: &str) -> (Self, mpsc::UnboundedReceiver<IdeFullBufferMessage>) {
        let (send, recv) = mpsc::unbounded_channel();
        (FileBackend {
            shared: Arc::new(Mutex::new( FileBackendInternal::new(path, send))),
        }, recv)
    }

    #[tracing::instrument(skip(self))]
    pub async fn init(&self, text: &str) {
        let query =
            fstar::Query::VfsAdd(fstar::VfsAddQuery{
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

    pub fn get_working_dir(&self) -> std::path::PathBuf {
        self.lock().unwrap().working_dir.clone()
    }

    #[tracing::instrument(skip_all)]
    async fn handle_full_buffer_messages_loop(mut ch: sync_channel::Receiver<fstar::ResponseOrMessage>, send: mpsc::UnboundedSender<IdeFullBufferMessage>, ide_type: IdeType) {
        while let Some((resp_or_msg, acker)) = ch.recv().await {
            let opt_message = match resp_or_msg {
                fstar::ResponseOrMessage::Message(fstar::Message::Progress(fstar::ProgressMessageOrNull::Some(data))) => {
                    match data {
                        fstar::ProgressMessage::FullBufferFragmentStarted{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::InProgress,
                                range: ranges.into(),
                            }))
                        },
                        fstar::ProgressMessage::FullBufferFragmentFailed{ranges} => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::Failed,
                                range: ranges.into(),
                            }))
                        },
                        fstar::ProgressMessage::FullBufferFragmentLaxOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: FragmentStatus::LaxOk,
                                range: fragment.ranges.into(),
                            }))
                        },
                        fstar::ProgressMessage::FullBufferFragmentOk(fragment) => {
                            Some(FullBufferMessage::FragmentStatusUpdate (FragmentStatusUpdate {
                                status_type: if ide_type == IdeType::Lax { FragmentStatus::LaxOk } else { FragmentStatus::Ok },
                                range: fragment.ranges.into(),
                            }))
                        },
                        fstar::ProgressMessage::FullBufferStarted => {
                            Some(FullBufferMessage::Started)
                        },
                        fstar::ProgressMessage::FullBufferFinished => {
                            Some(FullBufferMessage::Finished)
                        },
                        _ => None
                    }
                }
                fstar::ResponseOrMessage::Response(fstar::Response{status: _, response}) => {
                    match serde_json::from_value::<fstar::VerificationFailureResponse>(response) {
                        Ok(x) => Some(FullBufferMessage::Error(x)),
                        Err(e) => {
                            error!("Couldn't parse failure response: {}", e);
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

    #[tracing::instrument(skip(self))]
    fn send_full_buffer_query(&self, ide_type: IdeType, kind: fstar::FullBufferKind) {
        let code = self.lock().unwrap().text.clone();
        let full_buffer_message = 
            fstar::Query::FullBuffer(fstar::FullBufferQuery{
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

    #[tracing::instrument(skip(self))]
    pub fn handle_full_buffer_change(&self, text: &str) {
        self.lock().unwrap().text = text.to_string();

        self.send_full_buffer_query(IdeType::Full, fstar::FullBufferKind::Cache);
        self.send_full_buffer_query(IdeType::Lax, fstar::FullBufferKind::Full);
    }

    #[tracing::instrument(skip(self))]
    pub fn verify_full_buffer(&self) {
        self.send_full_buffer_query(IdeType::Full, fstar::FullBufferKind::Full);
    }

    #[tracing::instrument(skip(self))]
    pub fn verify_to_position(&self, pos: Position) {
        self.send_full_buffer_query(IdeType::Full, fstar::FullBufferKind::VerifyToPosition(fstar::BarePosition::from(pos)))
    }

    #[tracing::instrument(skip(self))]
    pub fn lax_to_position(&self, pos: Position) {
        self.send_full_buffer_query(IdeType::Full, fstar::FullBufferKind::LaxToPosition(fstar::BarePosition::from(pos)))
    }

    #[tracing::instrument(skip(self))]
    pub async fn cancel_all(&self) {
        let _ch = self.lock().unwrap().ide.send_query_nosync(fstar::Query::Cancel(fstar::CancelPosition{cancel_line: 1, cancel_column: 0}));
        // the Cancel query doesn't send back any message -- ignore
    }

    #[tracing::instrument(skip(self))]
    pub fn reload_dependencies(&self) {
        self.send_full_buffer_query(IdeType::Lax, fstar::FullBufferKind::ReloadDeps);
        self.send_full_buffer_query(IdeType::Full, fstar::FullBufferKind::ReloadDeps);
    }

    #[tracing::instrument(skip(self))]
    pub fn restart_z3(&self) {
        let _ch = self.lock().unwrap().ide.send_query_nosync(fstar::Query::RestartSolver{});
        // the RestartSolver query doesn't send back any message -- ignore
    }

    #[tracing::instrument(skip(self))]
    pub async fn hover(&self, pos: Position) -> Option<HoverResult> {
        let (response, range) = self.lookup_query(vec![fstar::LookupRequest::Type], pos).await?;
        let response_type = match response.type_ {
            Some(x) => x,
            None => {
                error!("expected a type to be present");
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

    #[tracing::instrument(skip(self))]
    pub async fn goto_definition(&self, pos: Position) -> Option<GotoDefinitionResult> {
        let (response, _symbol_range) = self.lookup_query(vec![fstar::LookupRequest::DefinedAt], pos).await?;
        let response_defined_at = match response.defined_at {
            Some(x) => x,
            None => {
                error!("expected a definition location to be present");
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

    #[tracing::instrument(skip(self))]
    pub async fn lookup_query(&self, info: Vec<fstar::LookupRequest>, pos: Position) -> Option<(fstar::LookupResponse, Range)> {
        let symbol = self.lock().unwrap().get_symbol_at(pos);
        let Some(symbol) = symbol else {
            return None
        };
        let path = self.lock().unwrap().path.clone();
        let mut ch = self.lock().unwrap().get_flycheck_ide().send_query_nosync(
            fstar::Query::Lookup(fstar::LookupQuery {
                context: fstar::LookupContext::Code,
                symbol: symbol.symbol.clone(),
                requested_info: info,
                location: Some(fstar::Position::from(path, pos)),
            })
        );
        let resp_or_msg = ch.recv().await;
        let Some(fstar::ResponseOrMessage::Response(fstar::Response{status, response})) = resp_or_msg else {
            error!("expected response, got {:?}", resp_or_msg);
            return None
        };
        if status != fstar::ResponseStatus::Success {
            return None
        }
        let response = match serde_json::from_value::<fstar::LookupResponse>(response) {
            Ok(x) => x,
            Err(e) => {
                error!("Couldn't parse response: {}", e);
                return None
            }
        };
        if response.kind != *"symbol" {
            error!("Response's kind is not symbol: {}", response.kind);
            return None
        }
        Some((response, symbol.range))
    }

    #[tracing::instrument(skip(self))]
    pub async fn complete(&self, pos: Position) -> Option<CompleteResult> {
        let symbol = self.lock().unwrap().get_symbol_at(pos);
        let Some(symbol) = symbol else {
            return None
        };
        let mut ch = self.lock().unwrap().get_flycheck_ide().send_query_nosync(
            fstar::Query::AutoComplete(fstar::AutoCompleteQuery {
                context: fstar::AutoCompleteContext::Code,
                partial_symbol: symbol.symbol,
            })
        );
        let resp_or_msg = ch.recv().await;

        let Some(fstar::ResponseOrMessage::Response(fstar::Response{status, response})) = resp_or_msg
        else {
            error!("expected a response, got {:?}", resp_or_msg);
            return None
        };
        if status != fstar::ResponseStatus::Success {
            return None
        }
        match serde_json::from_value::<fstar::AutoCompleteResponse>(response) {
            Ok(response) => {
                Some(
                    response.into_iter()
                        .map(|fstar::AutoCompleteResponseItem(match_length, annotation, candidate)| CompleteResultItem {match_length, annotation, candidate})
                        .filter(|cri| cri.annotation != "<search term>")
                        .collect()
                )
            }
            Err(err) => {
                error!("couldn't parse response: {}", err);
                None
            }
        }
    }
}
