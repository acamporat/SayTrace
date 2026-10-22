use crate::{
    local_agent::{self, GroundedReply},
    models::{AgentContextTurn, TranscriptChatMessage},
};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_LINE: u64 = 2 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptModel {
    pub id: String,
    pub display_name: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptStatus {
    pub available: bool,
    pub signed_in: bool,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub models: Vec<ChatGptModel>,
    pub error: Option<String>,
}

// This home is owned by SayTrace. Desktop Codex credentials/config are never read.
pub struct CodexAgent {
    home: PathBuf,
    session: Mutex<Option<Session>>,
}
impl CodexAgent {
    pub fn new(root: &Path) -> Self {
        Self {
            home: root.join("chatgpt-agent"),
            session: Mutex::new(None),
        }
    }
    fn with_session<T>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut slot = self
            .session
            .try_lock()
            .ok_or("ChatGPT is busy. Try again when the current request finishes.")?;
        if slot.is_none() {
            *slot = Some(Session::start(&self.home)?);
        }
        let result = f(slot.as_mut().unwrap());
        if result.is_err() {
            *slot = None;
        }
        result
    }
    pub fn status(&self) -> ChatGptStatus {
        let result = self.with_session(|session| {
            let response = session.rpc("account/read", json!({"refreshToken":false}))?;
            let account = &response["account"];
            let signed_in = account["type"] == "chatgpt";
            let models = if signed_in {
                let response =
                    session.rpc("model/list", json!({"limit":100,"includeHidden":false}))?;
                response["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|model| {
                        Some(ChatGptModel {
                            id: model["model"]
                                .as_str()
                                .or_else(|| model["id"].as_str())?
                                .to_owned(),
                            display_name: model["displayName"]
                                .as_str()
                                .unwrap_or("Codex")
                                .to_owned(),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(ChatGptStatus {
                available: true,
                signed_in,
                email: account["email"].as_str().map(str::to_owned),
                plan_type: account["planType"].as_str().map(str::to_owned),
                models,
                error: None,
            })
        });
        result.unwrap_or_else(|error| ChatGptStatus {
            available: false,
            signed_in: false,
            email: None,
            plan_type: None,
            models: vec![],
            error: Some(error),
        })
    }
    pub fn sign_in(&self) -> Result<String, String> {
        self.with_session(|s| {
            let response = s.rpc("account/login/start", json!({"type":"chatgpt"}))?;
            let url = response["authUrl"]
                .as_str()
                .ok_or("Codex did not return a sign-in URL.")?;
            if !url.starts_with("https://auth.openai.com/")
                && !url.starts_with("https://chatgpt.com/")
            {
                return Err("Codex returned an unexpected sign-in URL.".into());
            }
            Ok(url.to_owned())
        })
    }
    pub fn sign_out(&self) -> Result<(), String> {
        self.with_session(|s| {
            s.rpc("account/logout", json!({}))?;
            Ok(())
        })?;
        *self.session.lock() = None;
        Ok(())
    }
    pub fn ask(
        &self,
        consent: bool,
        model: Option<&str>,
        question: &str,
        turns: &[AgentContextTurn],
        history: &[TranscriptChatMessage],
    ) -> Result<GroundedReply, String> {
        require_consent(consent)?;
        let (messages, context) =
            local_agent::grounded_messages(question, turns, history).map_err(|e| e.to_string())?;
        let raw = self.with_session(|s| {
            let account = s.rpc("account/read",json!({"refreshToken":false}))?;
            if account["account"]["type"] != "chatgpt" { return Err("Sign in with ChatGPT before asking a cloud question.".into()); }
            let response = s.rpc("thread/start",json!({
                "model":model,"cwd":s.cwd,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true,
                "baseInstructions":messages[0]["content"],
                "developerInstructions":"No tools, filesystem access, web search, or external actions. Answer from supplied meeting excerpts only. Return only the requested grounded JSON object.",
                "config":{"features.shell_tool":false,"features.apply_patch_freeform":false,"features.multi_agent":false,"web_search":"disabled","mcp_servers":{}}
            }))?;
            let thread_id = response["thread"]["id"].as_str().ok_or("Codex did not start a thread.")?.to_owned();
            let prompt = messages.iter().skip(1).map(|m| format!("{}: {}",m["role"].as_str().unwrap_or("user"),m["content"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n\n");
            let response = s.rpc("turn/start",json!({"threadId":thread_id,"input":[{"type":"text","text":prompt,"text_elements":[]}],"outputSchema":grounded_schema()}))?;
            let turn_id = response["turn"]["id"].as_str().ok_or("Codex did not start an answer.")?.to_owned();
            let deadline = Instant::now()+TURN_TIMEOUT;
            let mut answer = String::new();
            loop {
                let event = s.receive(deadline)?;
                if s.deny_request(&event)? { continue; }
                if event["params"]["threadId"] != thread_id { continue; }
                if event["method"] == "item/completed" && event["params"]["turnId"] == turn_id {
                    let item = &event["params"]["item"];
                    if item["type"] == "agentMessage" && item["phase"] != "commentary" { answer = item["text"].as_str().unwrap_or("").to_owned(); }
                }
                if event["method"] == "turn/completed" && event["params"]["turn"]["id"] == turn_id {
                    return completed_answer(&event["params"]["turn"], answer);
                }
            }
        })?;
        local_agent::parse_grounded_reply(&raw, question, &context).map_err(|e| e.to_string())
    }
}
fn completed_answer(turn: &Value, answer: String) -> Result<String, String> {
    if turn["status"] != "completed" {
        return Err(
            "Codex could not complete this answer. Check your account limits and retry.".into(),
        );
    }
    if answer.trim().is_empty() || answer.len() > 96_000 {
        return Err("Codex returned no grounded answer or exceeded the response limit.".into());
    }
    Ok(answer)
}
fn require_consent(consent: bool) -> Result<(), String> {
    if consent {
        Ok(())
    } else {
        Err("Allow sending this meeting's selected transcript excerpts and chat history to ChatGPT before using Codex credits.".into())
    }
}

struct Session {
    child: Child,
    input: ChildStdin,
    output: Receiver<Result<Value, String>>,
    next_id: u64,
    pending: VecDeque<Value>,
    cwd: PathBuf,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Session {
    fn start(home: &Path) -> Result<Self, String> {
        let cwd = home.join("empty-workspace");
        fs::create_dir_all(&cwd).map_err(|_| "Could not create the isolated ChatGPT workspace.")?;
        let mut command = Command::new(
            codex_executable().ok_or("Install Codex for Windows to enable ChatGPT sign-in.")?,
        );
        command
            .args([
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "-c",
                "features.shell_tool=false",
                "-c",
                "features.apply_patch_freeform=false",
                "-c",
                "features.multi_agent=false",
                "-c",
                "web_search=\"disabled\"",
                "-c",
                "analytics.enabled=false",
            ])
            .env("CODEX_HOME", home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("OPENAI_ORG_ID")
            .env_remove("OPENAI_ORGANIZATION")
            .env_remove("OPENAI_PROJECT_ID")
            .env_remove("CHATGPT_ACCOUNT_ID")
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command.args(["-c", "features.apps=false"]);
        command.args(["-c", "features.browser_use=false"]);
        command.args(["-c", "features.browser_use_external=false"]);
        command.args(["-c", "features.computer_use=false"]);
        command.args(["-c", "features.image_generation=false"]);
        command.args(["-c", "features.view_image=false"]);
        command.args(["-c", "features.hooks=false"]);
        command.args(["-c", "features.plugins=false"]);
        command.args(["-c", "features.remote_plugin=false"]);
        command.args(["-c", "features.skill_search=false"]);
        command.args(["-c", "features.memories=false"]);
        command.args(["-c", "features.code_mode_host=false"]);
        command.args(["-c", "features.tool_suggest=false"]);
        command.args(["-c", "features.unified_exec=false"]);
        command.args(["-c", "features.workspace_dependencies=false"]);
        command.args(["-c", "features.sleep_tool=false"]);
        command.args(["-c", "features.goals=false"]);
        command.args(["-c", "features.shell_snapshot=false"]);
        command.args(["-c", "features.skip_host_skill_discovery=true"]);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if (name.starts_with("CODEX_") && name != "CODEX_HOME")
                || name.starts_with("OPENAI_")
                || name.starts_with("CHATGPT_")
            {
                command.env_remove(key);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Could not start the installed Codex app server.")?;
        let input = child.stdin.take().ok_or("Codex input unavailable.")?;
        let stdout = child.stdout.take().ok_or("Codex output unavailable.")?;
        let (tx, rx) = mpsc::sync_channel(256);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match reader
                    .by_ref()
                    .take(MAX_LINE + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() as u64 > MAX_LINE => {
                        let _ =
                            tx.send(Err("Codex protocol message exceeded the safe limit.".into()));
                        break;
                    }
                    Ok(_) => {
                        let message = serde_json::from_slice(&line)
                            .map_err(|_| "Codex returned an invalid protocol message.".into());
                        if tx.send(message).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut session = Self {
            child,
            input,
            output: rx,
            next_id: 1,
            pending: VecDeque::new(),
            cwd,
        };
        session.rpc("initialize",json!({"clientInfo":{"name":"saytrace","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}}))?;
        session.send(&json!({"method":"initialized"}))?;
        Ok(session)
    }
    fn send(&mut self, message: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, message)
            .map_err(|_| "Could not send a request to Codex.")?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|_| "Codex disconnected.".into())
    }
    fn receive(&mut self, deadline: Instant) -> Result<Value, String> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        self.receive_raw(deadline)
    }
    fn receive_raw(&self, deadline: Instant) -> Result<Value, String> {
        self.output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "Codex did not respond in time. Retry the request.".to_owned())?
    }
    fn deny_request(&mut self, message: &Value) -> Result<bool, String> {
        if message.get("method").is_some() && message.get("id").is_some() {
            self.send(&json!({"id":message["id"],"error":{"code":-32601,"message":"SayTrace does not permit tools or external actions."}}))?;
            return Ok(true);
        }
        Ok(false)
    }
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let message = self.receive_raw(deadline)?;
            if self.deny_request(&message)? {
                continue;
            }
            if message.get("method").is_some() {
                if self.pending.len() >= 256 {
                    self.pending.pop_front();
                }
                self.pending.push_back(message);
                continue;
            }
            if message["id"] == id {
                if message.get("error").is_some() {
                    return Err(format!("Codex could not complete {method}. Check sign-in, account limits, or update Codex."));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or("Codex response is missing a result.".into());
            }
        }
    }
}
fn codex_executable() -> Option<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local).join("Programs/OpenAI/Codex/bin/codex.exe");
        if path.is_file() {
            return Some(path);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(if cfg!(windows) { "codex.exe" } else { "codex" }))
            .find(|p| p.is_file())
    })
}
fn grounded_schema() -> Value {
    json!({"type":"object","properties":{
    "lead":{"type":"string"},"sections":{"type":"array","items":{"type":"object","properties":{"heading":{"type":"string"},"items":{"type":"array","items":{"type":"object","properties":{"kind":{"type":"string","enum":["decision","proposal","action","fact","open_question","detail"]},"title":{"type":"string"},"detail":{"type":"string"},"timing":{"type":"string"}},"required":["kind","title","detail","timing"],"additionalProperties":false}}},"required":["heading","items"],"additionalProperties":false}},"citations":{"type":"array","items":{"type":"string"}}},"required":["lead","sections","citations"],"additionalProperties":false})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_requires_explicit_consent() {
        let temp = tempfile::tempdir().unwrap();
        let agent = CodexAgent::new(temp.path());
        assert!(agent.ask(false, None, "Question", &[], &[]).is_err());
        assert!(!agent.home.exists());
        assert!(agent.session.lock().is_none());
        assert!(require_consent(false).is_err());
        assert!(require_consent(true).is_ok());
    }
    #[test]
    #[ignore = "requires installed Codex; no login or inference"]
    fn isolated_app_server_starts_without_desktop_account() {
        let temp = tempfile::tempdir().unwrap();
        let agent = CodexAgent::new(temp.path());
        let status = agent.status();
        assert!(status.available, "{:?}", status.error);
        assert!(!status.signed_in);
        assert!(status.models.is_empty());
    }
    #[test]
    fn omitted_provider_and_consent_stay_local() {
        let request: crate::models::AskTranscriptRequest = serde_json::from_value(
            json!({"meetingId":"meeting", "question":"What did we decide?"}),
        )
        .unwrap();
        assert!(request.provider.is_none());
        assert!(!request.cloud_consent);
    }
    #[test]
    fn failed_or_incomplete_turns_never_return_an_answer() {
        for status in ["failed", "interrupted", "inProgress"] {
            assert!(completed_answer(&json!({"status":status}), "partial answer".into()).is_err());
        }
        assert!(completed_answer(&json!({"status":"completed"}), "".into()).is_err());
        assert_eq!(
            completed_answer(&json!({"status":"completed"}), "final answer".into()).unwrap(),
            "final answer"
        );
    }
    #[test]
    fn schema_requires_grounded_fields() {
        let schema = grounded_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["lead", "sections", "citations"]));
    }
}
