use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::process;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use std::process::Stdio;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use crate::{log, info, warning, error};
use anyhow::Result;
use crate::sync_channel;

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct FullQuery {
    #[serde(rename = "query-id")]
    pub query_id: String,
    #[serde(flatten)]
    pub query: Query,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(tag = "query", content = "args")]
#[serde(rename_all = "kebab-case")]
pub enum Query {
    Exit{},
    DescribeProtocol{},
    DescribeRepl{},
    Segment(String),
    Pop{},
    Push(PushQuery),
    VfsAdd(VfsAddQuery),
    #[serde(rename = "autocomplete")]
    AutoComplete(AutoCompleteQuery),
    Lookup(LookupQuery),
    Compute(ComputeQuery),
    Search(String),
    FullBuffer(FullBufferQuery),
    Format(String),
    RestartSolver{},
    // In F*'s code, it looks like it is a Option<CancelPosition>.
    // However, the parsing code doesn't handle the "None" case.
    // Hence, here it is simply a "CancelPosition" (enforcing it to be a "Some")
    Cancel(CancelPosition),
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum PushKind {
    Syntax,
    Lax,
    Full,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct PushQuery {
    pub kind: PushKind,
    pub line: u32,
    pub column: u32,
    //peek_only: bool,
    pub code: String,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct VfsAddQuery {
    pub filename: Option<String>,
    pub contents: String,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum AutoCompleteContext {
    Code,
    //TODO
// | CKOption of bool (* #set-options (false) or #reset-options (true) *)
// | CKModuleOrNamespace of bool (* modules *) * bool (* namespaces *)
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct AutoCompleteQuery {
    #[serde(rename = "partial-symbol")]
    pub partial_symbol: String,
    pub context: AutoCompleteContext,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum LookupContext {
    SymbolOnly,
    Code,
    SetOptions,
    ResetOptions,
    Open,
    LetOpen,
    Include,
    ModuleAlias,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum LookupRequest {
    DefinedAt,
    Type,
    Documentation,
    Definition,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct LookupQuery {
    pub context: LookupContext,
    pub symbol: String,
    #[serde(rename = "requested-info")]
    pub requested_info: Vec<LookupRequest>,
    pub location: Option<Position>
    //symbol-range: arbitrary json data that is echoed back in the response, for some reason?
    //(optional so we omit it)
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct ComputeQuery {
    pub term: String,
    pub rules: Vec<String>,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum FullBufferKind {
    Full,
    Lax,
    Cache,
    ReloadDeps,
    VerifyToPosition(Position),
    LaxToPosition(Position),
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct FullBufferQuery {
    pub code: String,
    pub kind: FullBufferKind,
    #[serde(rename = "with-symbols")]
    pub with_symbols: bool,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct Position {
    pub filename: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct CancelPosition {
    pub cancel_line: u32,
    pub cancel_column: u32,
}


#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct FullResponseOrMessage {
    #[serde(rename = "query-id")]
    pub query_id: String,
    #[serde(flatten)]
    pub response_or_message: ResponseOrMessage,
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "kind")]
pub enum ResponseOrMessage {
    Response(Response),
    Message(Message),
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum ResponseStatus {
    Success,
    Failure,
    ProtocolViolation,
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct Response {
    pub status: ResponseStatus,
    pub response: serde_json::Value,
}

// {"kind":"symbol","name":"Test.f","defined-at":null,"type":"x: Mktoto?.ty a -> string","documentation":null,"definition":null}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct LookupResponse {
    pub kind: String, //TODO: this is an enum?
    pub name: String,
    #[serde(rename = "defined-at")]
    pub defined_at: Option<Range>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub documentation: Option<String>,
    pub definition: Option<String>,
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct AutoCompleteResponseItem (
    pub u32, // match_length
    pub String, // annotation
    pub String, // candidate
);

pub type AutoCompleteResponse = Vec<AutoCompleteResponseItem>;

#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationFailureLevel {
    Info,
    Warning,
    Error,
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct VerificationFailureResponseItem {
    pub level: VerificationFailureLevel,
    pub number: u32,
    pub message: String,
    pub ranges: Vec<Range>,
}

pub type VerificationFailureResponse = Vec<VerificationFailureResponseItem>;

#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "level", content = "contents")]
pub enum Message {
    Info(InfoMessage),
    Progress(ProgressMessageOrNull),
    ProofState(ProofStateMessage),
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct InfoMessage(String);

#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(tag = "stage")]
#[serde(rename_all = "kebab-case")]
pub enum ProgressMessage {
    FullBufferStarted,
    FullBufferFragmentStarted{ranges: Range},
    FullBufferFragmentLaxOk(FullBufferFragmentOk),
    FullBufferFragmentOk(FullBufferFragmentOk),
    FullBufferFragmentFailed{ranges: Range},
    LoadingDependency{modname: String},
    FullBufferFinished,
}

// Sometimes, F*-ide with full-buffer can respond a ProgressMessage with "stage: null".
// The derived Deserializer above expect the "stage" field to be a string.
// This hack with untagged allows us to catch the null case.
#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(untagged)]
pub enum ProgressMessageOrNull {
    Some(ProgressMessage),
    Null{stage: Option<()>},
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct FullBufferFragmentOk {
    pub ranges: Range,
    #[serde(rename = "code-fragment")]
    pub code_fragment: CodeFragment,
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct Range {
    pub fname: String,
    pub beg: (u32, u32),
    pub end: (u32, u32),
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}({},{}-{},{})", self.fname, self.beg.0, self.beg.1, self.end.0, self.end.1)
    }
}

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct CodeFragment {
    pub range: Range,
    #[serde(rename = "code-digest")]
    pub code_digest: String,
}

//TODO
#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct ProofStateMessage(serde_json::Value);

pub mod bare {
    use super::*;
    pub
    struct Sender {
        send: mpsc::UnboundedSender<FullQuery>,
    }

    pub
    struct Receiver {
        _child: tokio::process::Child,
        recv: mpsc::UnboundedReceiver<FullResponseOrMessage>,
    }

    pub fn channel<S, I>(path_to_fstar_exe: S, args: I, cwd: &std::path::Path) -> (Sender, Receiver)
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        let mut child = process::Command::new(path_to_fstar_exe)
            .arg("--ide")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn fstar ide");
        let fstar_stdin = child.stdin.take().unwrap();
        let fstar_stdout = child.stdout.take().unwrap();
        let (send_query, recv_query) = mpsc::unbounded_channel();
        let (send_response, recv_response) = mpsc::unbounded_channel();
        tokio::spawn(fstar_stdout_loop(send_response, fstar_stdout));
        tokio::spawn(fstar_stdin_loop(recv_query, fstar_stdin));
        (Sender {
            send: send_query,
        }, Receiver {
            _child: child,
            recv: recv_response,
        })
    }

    async fn fstar_stdin_loop(mut recv_query: mpsc::UnboundedReceiver<FullQuery>, mut fstar_stdin: tokio::process::ChildStdin) -> Result<()> {
        while let Some(query) = recv_query.recv().await {
            log!("[send] [high] {:?}", query);
            let line = serde_json::to_string(&query)?;
            log!("[send] [bare] {}", line);
            fstar_stdin.write_all(line.as_bytes()).await?;
            fstar_stdin.write_all(b"\n").await?;
            fstar_stdin.flush().await?;
        }
        Ok(())
    }

    async fn fstar_stdout_loop(send_response: mpsc::UnboundedSender<FullResponseOrMessage>, fstar_stdout: tokio::process::ChildStdout) -> Result<()> {
        let mut fstar_stdout_lines = tokio::io::BufReader::new(fstar_stdout).lines();
        let _ = fstar_stdout_lines.next_line().await?;
        while let Some(line) = fstar_stdout_lines.next_line().await? {
            log!("[recv] [bare] {}", line);
            match serde_json::from_str(&line) {
                Err(e) => {
                    error!("[recv] Cannot parse F*'s stdout: {}", e)
                }
                Ok(response) => {
                    log!("[recv] [high] {:?}", response);
                    send_response.send(response)?;
                }
            };
        }
        Ok(())
    }

    impl Sender {
        pub fn send(&self, value: FullQuery) -> Result<()> {
            Ok(self.send.send(value)?)
        }
    }

    impl Receiver {
        pub async fn recv(&mut self) -> Option<FullResponseOrMessage> {
            self.recv.recv().await
        }
    }
}

#[derive (Copy, Clone)]
enum LastMessageDecider {
    FullBuffer,
    Other,
}

impl LastMessageDecider {
    fn new(query: &Query) -> Self {
        match query {
            Query::FullBuffer(_) => LastMessageDecider::FullBuffer,
            _ => LastMessageDecider::Other,
        }
    }

    fn decide(&self, resp_or_msg: &ResponseOrMessage) -> bool {
        match self {
            LastMessageDecider::Other => {
                match resp_or_msg {
                    ResponseOrMessage::Response(_) => true,
                    _ => false,
                }
            }
            LastMessageDecider::FullBuffer => {
                match resp_or_msg {
                    ResponseOrMessage::Message(Message::Progress(ProgressMessageOrNull::Some(ProgressMessage::FullBufferFinished))) => true,
                    _ => false,
                }
            }
        }
    }
}

#[derive (Clone)]
enum FStarIDESender {
    NoSync(mpsc::Sender<ResponseOrMessage>),
    Sync(sync_channel::Sender<ResponseOrMessage>),
}

impl FStarIDESender {
    async fn send(&self, x: ResponseOrMessage) -> Result<()> {
        match self {
            FStarIDESender::NoSync(send) => {
                send.send(x).await?;
                Ok(())
            }
            FStarIDESender::Sync(send) => {
                send.send(x).await
            }
        }
    }
}

pub struct FStarIDE {
    send: bare::Sender,
    current_query_id: u64,
    channels: Arc<Mutex<std::collections::HashMap<String, (FStarIDESender, LastMessageDecider)>>>,
}

impl FStarIDE {
    pub fn new<S, I>(path_to_fstar_exe: S, args: I, cwd: &std::path::Path) -> Self
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        let (send, recv) = bare::channel(path_to_fstar_exe, args, cwd);
        let channels = Arc::new(Mutex::new(std::collections::HashMap::new()));
        tokio::spawn(Self::receive_loop(recv, channels.clone()));
        FStarIDE {
            send,
            current_query_id: 0,
            channels,
        }
    }

    pub fn send_query_nosync(&mut self, query: Query) -> mpsc::Receiver<ResponseOrMessage> {
        let (send, recv) = mpsc::channel(100);
        self.send_query_internal(query, FStarIDESender::NoSync(send));
        recv
    }

    pub fn send_query_sync(&mut self, query: Query) -> sync_channel::Receiver<ResponseOrMessage> {
        let (send, recv) = sync_channel::channel();
        self.send_query_internal(query, FStarIDESender::Sync(send));
        recv
    }

    fn send_query_internal(&mut self, query: Query, send: FStarIDESender) {
        let query_id = self.current_query_id.to_string();
        self.current_query_id += 1;
        let last_message_decider = LastMessageDecider::new(&query);
        let full_query = FullQuery {
            query_id: query_id.clone(),
            query,
        };
        self.send.send(full_query).unwrap();
        self.channels.lock().unwrap().insert(query_id, (send, last_message_decider));
    }

    fn normalize_query_id(query_id: &str) -> &str {
        match query_id.split_once('.') {
            None => query_id,
            Some((x, _)) => x,
        }
    }

    async fn receive_loop(mut recv: bare::Receiver, channels: Arc<Mutex<std::collections::HashMap<String, (FStarIDESender, LastMessageDecider)>>>,) -> Result<()> {
        while let Some(data) = recv.recv().await {
            let opt_send = channels.lock().unwrap().get(Self::normalize_query_id(&data.query_id)).cloned();
            if let Some((send, last_message_decider)) = opt_send {
                let is_last_data = last_message_decider.decide(&data.response_or_message);
                send.send(data.response_or_message).await?;
                if is_last_data  {
                    info!("[recv loop] deleting channel for query {}", Self::normalize_query_id(&data.query_id));
                    channels.lock().unwrap().remove(&data.query_id);
                }
            } else {
                error!("no send in the channels map!")
            }
        }
        Ok(())
    }
}


impl Position {
    pub fn from(filename: String, pos: tower_lsp::lsp_types::Position) -> Self {
        Position{
            filename,
            line: pos.line + 1,
            column: pos.character,
        }
    }

    pub fn into(self) -> tower_lsp::lsp_types::Position {
        tower_lsp::lsp_types::Position::new(
            if self.line > 0 { self.line - 1 } else { self.line },
            self.column
        )
    }
}

impl Range {
    pub fn from(filename: String, range: tower_lsp::lsp_types::Range) -> Self {
        Range {
            fname: filename,
            beg: (range.start.line + 1, range.start.character),
            end: (range.end.line + 1, range.end.character),
        }
    }

    pub fn into(self) -> tower_lsp::lsp_types::Range {
        tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: if self.beg.0 > 0 { self.beg.0 - 1} else { self.beg.0 },
                character: self.beg.1,
            },
            end: tower_lsp::lsp_types::Position {
                line: if self.end.0 > 0 { self.end.0 - 1} else { self.end.0 },
                character: self.end.1,
            },
        }
    }
}
