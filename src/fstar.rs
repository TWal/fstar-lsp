mod bare_ide;
pub mod ide;

use serde::{Deserialize, Serialize};

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
#[serde(tag = "kind", content = "to-position")]
pub enum FullBufferKind {
    Full,
    Lax,
    Cache,
    ReloadDeps,
    VerifyToPosition(BarePosition),
    LaxToPosition(BarePosition),
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct FullBufferQuery {
    pub code: String,
    #[serde(flatten)]
    pub kind: FullBufferKind,
    #[serde(rename = "with-symbols")]
    pub with_symbols: bool,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct BarePosition {
    pub line: u32,
    pub column: u32,
}


#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct Position {
    pub filename: String,
    #[serde(flatten)]
    position: BarePosition,
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

#[derive(Deserialize, PartialEq, Clone, Debug)]
pub struct Null {
}

// Sometimes, F*-ide with full-buffer can respond a ProgressMessage with "stage: null".
// The derived Deserializer above expect the "stage" field to be a string.
// This hack with untagged allows us to catch the null case.
#[derive(Deserialize, PartialEq, Clone, Debug)]
#[serde(untagged)]
pub enum ProgressMessageOrNull {
    Some(ProgressMessage),
    Null{stage: Null},
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


impl BarePosition {
    pub fn from(pos: tower_lsp::lsp_types::Position) -> Self {
        BarePosition{
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


impl Position {
    pub fn from(filename: String, pos: tower_lsp::lsp_types::Position) -> Self {
        Position{
            filename,
            position: BarePosition::from(pos),
        }
    }

    pub fn into(self) -> tower_lsp::lsp_types::Position {
        self.position.into()
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
