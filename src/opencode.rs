use std::{
    env,
    ffi::OsStr,
    hash::{BuildHasher, Hasher},
    io::{BufRead, BufReader},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

pub const DEFAULT_PROVIDER: &str = "opencode-go";
pub const DEFAULT_MODEL: &str = "deepseek-v4.1-flash";
pub const EXTERNAL_SERVER_ENV: &str = "OPENCODE_SERVER_URL";
pub const OPENCODE_BIN_ENV: &str = "OPENCODE_BIN";

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
}

impl ModelChoice {
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    pub fn display(&self) -> String {
        if self.model_name.eq_ignore_ascii_case(&self.model_id) {
            self.key()
        } else {
            format!("{} - {}", self.key(), self.model_name)
        }
    }

    pub fn matches_filter(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }

        let filter = filter.to_lowercase();
        self.provider_id.to_lowercase().contains(&filter)
            || self.model_id.to_lowercase().contains(&filter)
            || self.model_name.to_lowercase().contains(&filter)
            || self.provider_name.to_lowercase().contains(&filter)
    }
}

pub fn default_model_choice() -> ModelChoice {
    ModelChoice {
        provider_id: DEFAULT_PROVIDER.to_string(),
        provider_name: "OpenCode Go".to_string(),
        model_id: DEFAULT_MODEL.to_string(),
        model_name: DEFAULT_MODEL.to_string(),
    }
}

pub fn pick_default_model(models: &[ModelChoice]) -> Option<usize> {
    models
        .iter()
        .position(|model| model.provider_id == DEFAULT_PROVIDER && model.model_id == DEFAULT_MODEL)
        .or_else(|| {
            models
                .iter()
                .position(|model| model.model_id.eq_ignore_ascii_case(DEFAULT_MODEL))
        })
}

#[derive(Clone)]
pub struct OpencodeClient {
    agent: ureq::Agent,
    base_url: String,
    auth: Option<String>,
}

impl OpencodeClient {
    pub fn new(base_url: String, auth: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_write(Duration::from_secs(15))
            .build();

        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn with_auth(&self, request: ureq::Request) -> ureq::Request {
        match &self.auth {
            Some(auth) => request.set("Authorization", auth),
            None => request,
        }
    }

    pub fn health(&self) -> Result<Value, String> {
        let response = self
            .with_auth(self.agent.get(&self.url("/global/health")))
            .timeout(HEALTH_TIMEOUT)
            .call()
            .map_err(describe_ureq_error)?;
        read_json(response)
    }

    pub fn get_json(&self, path: &str) -> Result<Value, String> {
        let response = self
            .with_auth(self.agent.get(&self.url(path)))
            .timeout(REQUEST_TIMEOUT)
            .call()
            .map_err(describe_ureq_error)?;
        read_json(response)
    }

    pub fn post_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .with_auth(self.agent.post(&self.url(path)))
            .timeout(REQUEST_TIMEOUT)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(describe_ureq_error)?;
        read_json(response)
    }

    pub fn post_empty(&self, path: &str) -> Result<(), String> {
        self.with_auth(self.agent.post(&self.url(path)))
            .timeout(REQUEST_TIMEOUT)
            .send_string("")
            .map(|_| ())
            .map_err(describe_ureq_error)
    }

    pub fn fetch_models(&self) -> Result<Vec<ModelChoice>, String> {
        let value = self.get_json("/config/providers")?;
        let providers = value
            .get("providers")
            .and_then(Value::as_array)
            .ok_or_else(|| "provider response did not contain a providers array".to_string())?;

        let mut models = Vec::new();
        for provider in providers {
            let provider_id = provider
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if provider_id.is_empty() {
                continue;
            }

            let provider_name = provider
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&provider_id)
                .to_string();

            let Some(provider_models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };

            for (model_id, model) in provider_models {
                let model_name = model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id)
                    .to_string();

                models.push(ModelChoice {
                    provider_id: provider_id.clone(),
                    provider_name: provider_name.clone(),
                    model_id: model_id.clone(),
                    model_name,
                });
            }
        }

        models.sort_by(|left, right| {
            left.provider_id
                .to_lowercase()
                .cmp(&right.provider_id.to_lowercase())
                .then_with(|| {
                    left.model_id
                        .to_lowercase()
                        .cmp(&right.model_id.to_lowercase())
                })
        });

        Ok(models)
    }

    pub fn create_session(
        &self,
        title: &str,
        model: Option<&ModelChoice>,
    ) -> Result<String, String> {
        let mut body = json!({ "title": title });
        if let Some(model) = model {
            body["model"] = json!({
                "id": model.model_id,
                "providerID": model.provider_id,
            });
        }

        let response = self.post_json("/session", &body)?;
        response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "session response did not contain an id".to_string())
    }

    pub fn prompt_async(
        &self,
        session_id: &str,
        model: Option<&ModelChoice>,
        texts: &[String],
    ) -> Result<(), String> {
        let parts = texts
            .iter()
            .map(|text| json!({ "type": "text", "text": text }))
            .collect::<Vec<_>>();

        let mut body = json!({ "parts": parts });
        if let Some(model) = model {
            body["model"] = json!({
                "providerID": model.provider_id,
                "modelID": model.model_id,
            });
        }

        self.post_json(&format!("/session/{session_id}/prompt_async"), &body)
            .map(|_| ())
    }

    pub fn abort(&self, session_id: &str) -> Result<(), String> {
        self.post_empty(&format!("/session/{session_id}/abort"))
    }

    pub fn reply_permission(&self, request_id: &str, reply: &str) -> Result<(), String> {
        let body = json!({ "reply": reply });
        self.post_json(&format!("/permission/{request_id}/reply"), &body)
            .map(|_| ())
    }

    pub fn reject_question(&self, request_id: &str) -> Result<(), String> {
        self.post_empty(&format!("/question/{request_id}/reject"))
    }

    pub fn open_event_stream(&self) -> Result<Box<dyn BufRead + Send>, String> {
        let response = self
            .with_auth(self.agent.get(&self.url("/event")))
            .call()
            .map_err(describe_ureq_error)?;
        let reader = response.into_reader();
        Ok(Box::new(BufReader::new(reader)))
    }
}

fn read_json(response: ureq::Response) -> Result<Value, String> {
    let text = response
        .into_string()
        .map_err(|error| format!("could not read response body: {error}"))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }

    serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse response JSON: {error}: {}",
            truncate(&text, 400)
        )
    })
}

fn describe_ureq_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            let body = body.trim();
            if body.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {}", truncate(body, 800))
            }
        }
        ureq::Error::Transport(transport) => format!("connection error: {transport}"),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub struct OpencodeServer {
    child: Option<Child>,
    base_url: String,
    auth: Option<String>,
    pub version: String,
}

impl OpencodeServer {
    pub fn connect_or_spawn(cwd: &Path) -> Result<Self, String> {
        if let Some(base_url) = env::var(EXTERNAL_SERVER_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return Self::connect_external(base_url.trim().to_string());
        }

        let executable = find_opencode_executable()?;
        let port = reserve_free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let password = generate_server_password();
        let auth = Some(format!(
            "Basic {}",
            base64_encode(format!("opencode:{password}").as_bytes())
        ));
        let mut command = executable.command(&["serve", "--port", &port.to_string()]);
        command
            .current_dir(cwd)
            .env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start opencode serve: {error}"))?;
        let client = OpencodeClient::new(base_url.clone(), auth.clone());

        match wait_for_health(&client, Some(&mut child)) {
            Ok(health) => Ok(Self {
                child: Some(child),
                base_url,
                auth,
                version: health_version(&health),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    fn connect_external(base_url: String) -> Result<Self, String> {
        let auth = external_server_auth();
        let client = OpencodeClient::new(base_url.clone(), auth.clone());
        let health = wait_for_health(&client, None)?;

        Ok(Self {
            child: None,
            base_url,
            auth,
            version: health_version(&health),
        })
    }

    pub fn client(&self) -> OpencodeClient {
        OpencodeClient::new(self.base_url.clone(), self.auth.clone())
    }
}

impl Drop for OpencodeServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;

                if matches!(child.try_wait(), Ok(None)) {
                    let pid = child.id().to_string();
                    let _ = Command::new("taskkill")
                        .args(["/PID", &pid, "/T", "/F"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .creation_flags(0x0800_0000)
                        .status();
                }
            }

            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn external_server_auth() -> Option<String> {
    let password = env::var("OPENCODE_SERVER_PASSWORD").ok()?;
    if password.is_empty() {
        return None;
    }

    let username = env::var("OPENCODE_SERVER_USERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "opencode".to_string());

    Some(format!(
        "Basic {}",
        base64_encode(format!("{username}:{password}").as_bytes())
    ))
}

fn generate_server_password() -> String {
    let mut first = std::collections::hash_map::RandomState::new().build_hasher();
    first.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
    );
    first.write_u32(std::process::id());

    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(first.finish());

    format!("{:016x}{:016x}", first.finish(), second.finish())
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        output.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }

    output
}

fn health_version(health: &Value) -> String {
    health
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn reserve_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("could not reserve a local port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("could not read the reserved port: {error}"))
}

fn wait_for_health(
    client: &OpencodeClient,
    mut child: Option<&mut Child>,
) -> Result<Value, String> {
    let deadline = Instant::now() + SERVER_START_TIMEOUT;

    loop {
        if let Ok(health) = client.health() {
            return Ok(health);
        }

        if let Some(Some(status)) = child.as_mut().and_then(|child| child.try_wait().ok()) {
            return Err(format!(
                "opencode serve exited before becoming ready ({status})"
            ));
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "opencode serve did not become ready within {} seconds",
                SERVER_START_TIMEOUT.as_secs()
            ));
        }

        thread::sleep(HEALTH_POLL_INTERVAL);
    }
}

enum OpencodeExecutable {
    Executable(PathBuf),
    CmdScript(PathBuf),
    PowerShellScript(PathBuf),
}

impl OpencodeExecutable {
    fn command(&self, args: &[&str]) -> Command {
        match self {
            Self::Executable(path) => {
                let mut command = Command::new(path);
                command.args(args);
                command
            }
            Self::CmdScript(path) => {
                let mut command = Command::new("cmd.exe");
                command.arg("/C").arg(path).args(args);
                command
            }
            Self::PowerShellScript(path) => {
                let shell = if cfg!(windows) { "pwsh.exe" } else { "pwsh" };
                let mut command = Command::new(shell);
                command
                    .arg("-NoLogo")
                    .arg("-NoProfile")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-File")
                    .arg(path)
                    .args(args);
                command
            }
        }
    }
}

fn find_opencode_executable() -> Result<OpencodeExecutable, String> {
    if let Some(configured) = env::var_os(OPENCODE_BIN_ENV) {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Ok(classify_executable(path));
        }
        return Err(format!(
            "{OPENCODE_BIN_ENV} points to {}, which is not a file",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            for name in [
                "opencode.exe",
                "opencode.cmd",
                "opencode.bat",
                "opencode.ps1",
                "opencode",
            ] {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    candidates.push(candidate);
                }
            }
        }
    }

    for root in known_install_roots() {
        for relative in [
            "node_modules/opencode-ai/bin/opencode.exe",
            "node_modules/.bin/opencode.exe",
            "bin/opencode.exe",
        ] {
            let candidate = root.join(relative);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| match path.extension().and_then(OsStr::to_str) {
        Some(extension) if extension.eq_ignore_ascii_case("exe") => 0,
        Some(extension) if extension.eq_ignore_ascii_case("cmd") => 1,
        Some(extension) if extension.eq_ignore_ascii_case("bat") => 1,
        Some(extension) if extension.eq_ignore_ascii_case("ps1") => 2,
        _ => 3,
    });

    candidates
        .into_iter()
        .next()
        .map(classify_executable)
        .ok_or_else(|| {
            "could not find the opencode executable. Install opencode or set OPENCODE_BIN."
                .to_string()
        })
}

fn classify_executable(path: PathBuf) -> OpencodeExecutable {
    match path.extension().and_then(OsStr::to_str) {
        Some(extension) if extension.eq_ignore_ascii_case("cmd") => {
            OpencodeExecutable::CmdScript(path)
        }
        Some(extension) if extension.eq_ignore_ascii_case("bat") => {
            OpencodeExecutable::CmdScript(path)
        }
        Some(extension) if extension.eq_ignore_ascii_case("ps1") => {
            OpencodeExecutable::PowerShellScript(path)
        }
        _ => OpencodeExecutable::Executable(path),
    }
}

fn known_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        if let Some(value) = env::var_os(variable) {
            roots.push(PathBuf::from(value));
        }
    }

    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let home = PathBuf::from(home);
        roots.push(home.join("AppData/Roaming/npm"));
        roots.push(home.join("AppData/Local/Programs"));
        roots.push(home.join(".bun/bin"));
        roots.push(home.join(".local/bin"));
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn encodes_basic_auth() {
        assert_eq!(base64_encode(b"opencode:pass"), "b3BlbmNvZGU6cGFzcw==");
    }

    #[test]
    fn picks_the_preferred_default_model() {
        let models = vec![
            ModelChoice {
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model_id: "deepseek-v4.1-flash".to_string(),
                model_name: "DeepSeek".to_string(),
            },
            ModelChoice {
                provider_id: DEFAULT_PROVIDER.to_string(),
                provider_name: "OpenCode Go".to_string(),
                model_id: DEFAULT_MODEL.to_string(),
                model_name: DEFAULT_MODEL.to_string(),
            },
        ];

        assert_eq!(pick_default_model(&models), Some(1));
        assert_eq!(pick_default_model(&[]), None);
    }

    #[test]
    #[ignore = "starts a local opencode server and makes a model request"]
    fn opencode_round_trip() {
        let cwd = env::current_dir().unwrap();
        eprintln!("[test] starting server");
        let server = OpencodeServer::connect_or_spawn(&cwd).expect("start server");
        eprintln!("[test] server ready at {}", server.base_url);
        let client = server.client();
        eprintln!("[test] fetching models");
        let models = client.fetch_models().expect("fetch models");
        eprintln!("[test] fetched {} models", models.len());
        let model = models
            .iter()
            .find(|model| model.provider_id == DEFAULT_PROVIDER && model.model_id == DEFAULT_MODEL)
            .cloned()
            .expect("default model is available");

        eprintln!("[test] creating session");
        let session = client
            .create_session("storage analytics integration test", Some(&model))
            .expect("create session");
        eprintln!("[test] session {session}");
        let reader = client.open_event_stream().expect("open event stream");
        eprintln!("[test] event stream open");

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader;
            let mut data = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    if !data.is_empty() {
                        let _ = tx.send(data.clone());
                        data.clear();
                    }
                } else if let Some(rest) = trimmed.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                }
            }
        });

        client
            .prompt_async(
                &session,
                Some(&model),
                &[
                    "Storage Analytics context: integration test".to_string(),
                    "Reply with exactly: OK".to_string(),
                ],
            )
            .expect("send prompt");
        eprintln!("[test] prompt sent");

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut saw_text = false;
        let mut saw_idle = false;
        while Instant::now() < deadline {
            let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) else {
                continue;
            };
            if event.contains("\"text\":\"OK\"") {
                saw_text = true;
            }
            if event.contains("session.idle") && event.contains(&session) {
                saw_idle = true;
                break;
            }
        }
        eprintln!("[test] saw_text={saw_text} saw_idle={saw_idle}");

        assert!(saw_text, "the assistant response was not observed");
        assert!(saw_idle, "the session never became idle");
    }
}
