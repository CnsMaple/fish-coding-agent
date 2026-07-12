use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsRequest {
    #[prost(string, repeated, tag = "1")]
    pub custom_model_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub models: Vec<ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1, 2, 3, 5, 7")]
    pub message: Option<agent_client_message::Message>,
}
pub mod agent_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::AgentRunRequest),
        #[prost(message, tag = "2")]
        ExecClientMessage(super::ExecClientMessage),
        #[prost(message, tag = "3")]
        KvClientMessage(super::KvClientMessage),
        #[prost(message, tag = "5")]
        ExecClientControlMessage(super::ExecClientControlMessage),
        #[prost(message, tag = "7")]
        ClientHeartbeat(super::ClientHeartbeat),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
pub struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    pub model_details: Option<ModelDetails>,
    #[prost(string, optional, tag = "5")]
    pub conversation_id: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub custom_system_prompt: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "5")]
    pub token_details: Option<ConversationTokenDetails>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConversationTokenDetails {
    #[prost(uint32, tag = "1")]
    pub used_tokens: u32,
    #[prost(uint32, tag = "2")]
    pub max_tokens: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1")]
    pub action: Option<conversation_action::Action>,
}
pub mod conversation_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Action {
        #[prost(message, tag = "1")]
        UserMessageAction(super::UserMessageAction),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}
#[derive(Clone, PartialEq, Message)]
pub struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub message_id: String,
    #[prost(int32, tag = "4")]
    pub mode: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct ModelDetails {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(string, tag = "3")]
    pub display_model_id: String,
    #[prost(string, tag = "4")]
    pub display_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(oneof = "exec_server_message::Message", tags = "2, 5, 7, 8, 10, 14")]
    pub message: Option<exec_server_message::Message>,
}
pub mod exec_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    #[allow(clippy::enum_variant_names)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellArgs(super::ShellArgs),
        #[prost(message, tag = "5")]
        GrepArgs(super::GrepArgs),
        #[prost(message, tag = "7")]
        ReadArgs(super::ReadArgs),
        #[prost(message, tag = "8")]
        LsArgs(super::LsArgs),
        #[prost(message, tag = "10")]
        RequestContextArgs(super::RequestContextArgs),
        #[prost(message, tag = "14")]
        ShellStreamArgs(super::ShellArgs),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub timeout: i32,
    #[prost(string, tag = "4")]
    pub tool_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellResult {
    #[prost(oneof = "shell_result::Result", tags = "1, 2")]
    pub result: Option<shell_result::Result>,
}
pub mod shell_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ShellSuccess),
        #[prost(message, tag = "2")]
        Failure(super::ShellFailure),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellSuccess {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub exit_code: i32,
    #[prost(string, tag = "4")]
    pub signal: String,
    #[prost(string, tag = "5")]
    pub stdout: String,
    #[prost(string, tag = "6")]
    pub stderr: String,
    #[prost(int32, tag = "7")]
    pub execution_time: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellFailure {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub exit_code: i32,
    #[prost(string, tag = "4")]
    pub signal: String,
    #[prost(string, tag = "5")]
    pub stdout: String,
    #[prost(string, tag = "6")]
    pub stderr: String,
    #[prost(int32, tag = "7")]
    pub execution_time: i32,
    #[prost(bool, tag = "11")]
    pub aborted: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellStream {
    #[prost(oneof = "shell_stream::Event", tags = "1, 2, 3, 4")]
    pub event: Option<shell_stream::Event>,
}
pub mod shell_stream {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Stdout(super::ShellStreamStdout),
        #[prost(message, tag = "2")]
        Stderr(super::ShellStreamStderr),
        #[prost(message, tag = "3")]
        Exit(super::ShellStreamExit),
        #[prost(message, tag = "4")]
        Start(super::ShellStreamStart),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStart {}
#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStdout {
    #[prost(string, tag = "1")]
    pub data: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStderr {
    #[prost(string, tag = "1")]
    pub data: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamExit {
    #[prost(uint32, tag = "1")]
    pub code: u32,
    #[prost(string, tag = "2")]
    pub cwd: String,
    #[prost(bool, tag = "4")]
    pub aborted: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "1")]
    pub message: Option<exec_client_control_message::Message>,
}
pub mod exec_client_control_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        StreamClose(super::ExecClientStreamClose),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub tool_call_id: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadResult {
    #[prost(oneof = "read_result::Result", tags = "1, 2, 3, 4, 5, 6")]
    pub result: Option<read_result::Result>,
}
pub mod read_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ReadSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadError),
        #[prost(message, tag = "3")]
        Rejected(super::ReadRejected),
        #[prost(message, tag = "4")]
        FileNotFound(super::ReadFileNotFound),
        #[prost(message, tag = "5")]
        PermissionDenied(super::ReadPermissionDenied),
        #[prost(message, tag = "6")]
        InvalidFile(super::ReadInvalidFile),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int32, tag = "3")]
    pub total_lines: i32,
    #[prost(int64, tag = "4")]
    pub file_size: i64,
    #[prost(bool, tag = "6")]
    pub truncated: bool,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub output_blob_id: Option<Vec<u8>>,
    #[prost(oneof = "read_success::Output", tags = "2, 5")]
    pub output: Option<read_success::Output>,
}
pub mod read_success {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Output {
        #[prost(string, tag = "2")]
        Content(String),
        #[prost(bytes, tag = "5")]
        Data(Vec<u8>),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadError {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub error: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadFileNotFound {
    #[prost(string, tag = "1")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadPermissionDenied {
    #[prost(string, tag = "1")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadInvalidFile {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct LsArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, repeated, tag = "2")]
    pub ignore: Vec<String>,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(uint32, optional, tag = "5")]
    pub timeout_ms: Option<u32>,
}
#[derive(Clone, PartialEq, Message)]
pub struct LsResult {
    #[prost(oneof = "ls_result::Result", tags = "1, 2, 3, 4")]
    pub result: Option<ls_result::Result>,
}
pub mod ls_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::LsSuccess),
        #[prost(message, tag = "2")]
        Error(super::LsError),
        #[prost(message, tag = "3")]
        Rejected(super::LsRejected),
        #[prost(message, tag = "4")]
        Timeout(super::LsTimeout),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct LsSuccess {
    #[prost(message, optional, tag = "1")]
    pub directory_tree_root: Option<LsDirectoryTreeNode>,
}
#[derive(Clone, PartialEq, Message)]
pub struct LsDirectoryTreeNode {
    #[prost(string, tag = "1")]
    pub abs_path: String,
    #[prost(message, repeated, tag = "2")]
    pub children_dirs: Vec<LsDirectoryTreeNode>,
    #[prost(message, repeated, tag = "3")]
    pub children_files: Vec<LsDirectoryTreeNodeFile>,
    #[prost(bool, tag = "4")]
    pub children_were_processed: bool,
    #[prost(map = "string, int32", tag = "5")]
    pub full_subtree_extension_counts: std::collections::HashMap<String, i32>,
    #[prost(int32, tag = "6")]
    pub num_files: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct LsDirectoryTreeNodeFile {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub terminal_metadata: Option<TerminalMetadata>,
}
#[derive(Clone, PartialEq, Message)]
pub struct TerminalMetadata {}
#[derive(Clone, PartialEq, Message)]
pub struct LsError {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub error: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct LsRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct LsTimeout {
    #[prost(message, optional, tag = "1")]
    pub directory_tree_root: Option<LsDirectoryTreeNode>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GrepArgs {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub glob: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub output_mode: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GrepResult {
    #[prost(oneof = "grep_result::Result", tags = "1, 2")]
    pub result: Option<grep_result::Result>,
}
pub mod grep_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::GrepSuccess),
        #[prost(message, tag = "2")]
        Error(super::GrepError),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct GrepSuccess {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(string, tag = "3")]
    pub output_mode: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct GrepError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestContextArgs {}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(oneof = "exec_client_message::Message", tags = "2, 5, 7, 8, 10, 14")]
    pub message: Option<exec_client_message::Message>,
}
pub mod exec_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellResult(super::ShellResult),
        #[prost(message, tag = "5")]
        GrepResult(super::GrepResult),
        #[prost(message, tag = "7")]
        ReadResult(super::ReadResult),
        #[prost(message, tag = "8")]
        LsResult(super::LsResult),
        #[prost(message, tag = "10")]
        RequestContextResult(super::RequestContextResult),
        #[prost(message, tag = "14")]
        ShellStream(super::ShellStream),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextResult {
    #[prost(oneof = "request_context_result::Result", tags = "1")]
    pub result: Option<request_context_result::Result>,
}
pub mod request_context_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::RequestContextSuccess),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    pub request_context: Option<RequestContext>,
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContext {}

#[derive(Clone, PartialEq, Message)]
pub struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1, 2, 3, 4")]
    pub message: Option<agent_server_message::Message>,
}
pub mod agent_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        InteractionUpdate(super::InteractionUpdate),
        #[prost(message, tag = "2")]
        ExecServerMessage(super::ExecServerMessage),
        #[prost(message, tag = "3")]
        ConversationCheckpointUpdate(super::ConversationStateStructure),
        #[prost(message, tag = "4")]
        KvServerMessage(super::KvServerMessage),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(
        oneof = "interaction_update::Message",
        tags = "1, 4, 5, 8, 13, 14, 16, 17"
    )]
    pub message: Option<interaction_update::Message>,
}
pub mod interaction_update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        TextDelta(super::TextDeltaUpdate),
        #[prost(message, tag = "4")]
        ThinkingDelta(super::ThinkingDeltaUpdate),
        #[prost(message, tag = "5")]
        ThinkingCompleted(super::ThinkingCompletedUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(super::TokenDeltaUpdate),
        #[prost(message, tag = "13")]
        Heartbeat(super::HeartbeatUpdate),
        #[prost(message, tag = "14")]
        TurnEnded(super::TurnEndedUpdate),
        #[prost(message, tag = "16")]
        StepStarted(super::StepStartedUpdate),
        #[prost(message, tag = "17")]
        StepCompleted(super::StepCompletedUpdate),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    pub text: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ThinkingDeltaUpdate {
    #[prost(string, tag = "1")]
    pub text: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ThinkingCompletedUpdate {
    #[prost(int32, tag = "1")]
    pub thinking_duration_ms: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct TokenDeltaUpdate {
    #[prost(int32, tag = "1")]
    pub tokens: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct HeartbeatUpdate {}
#[derive(Clone, PartialEq, Message)]
pub struct TurnEndedUpdate {}
#[derive(Clone, PartialEq, Message)]
pub struct StepStartedUpdate {}
#[derive(Clone, PartialEq, Message)]
pub struct StepCompletedUpdate {}
#[derive(Clone, PartialEq, Message)]
pub struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2, 3")]
    pub message: Option<kv_server_message::Message>,
}
pub mod kv_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobArgs(super::GetBlobArgs),
        #[prost(message, tag = "3")]
        SetBlobArgs(super::SetBlobArgs),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct GetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct SetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub blob_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2, 3")]
    pub message: Option<kv_client_message::Message>,
}
pub mod kv_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobResult(super::GetBlobResult),
        #[prost(message, tag = "3")]
        SetBlobResult(super::SetBlobResult),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct GetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub blob_data: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
pub struct SetBlobResult {}
