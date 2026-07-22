use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{
    CodeLanguage, CodeRunner, HttpClient, LlmProvider, StateStore, ToolInvoker, WorkflowResolver,
};
use tinyflows::companion::{CompanionServer, CompanionServerConfig, RelayPolicy, SecretStore};
use tinyflows::error::{EngineError, Result as EngineResult};
use tinyflows::model::WorkflowGraph;

#[tokio::main]
async fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() {
        println!("{}", tinyflows::product_name());
        return;
    }
    if let Err(error) = dispatch(&arguments).await {
        eprintln!("tinyflows: {error}");
        std::process::exit(2);
    }
}

async fn dispatch(arguments: &[String]) -> Result<(), String> {
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["extension", "path"] => {
            println!("{}", extension_path().display());
            Ok(())
        }
        ["pair", rest @ ..] => pair(rest),
        ["companion", "start", rest @ ..] => start_companion(rest).await,
        ["tabs", rest @ ..] => native_get("tabs", rest).await,
        ["workflows", rest @ ..] => native_get("workflows", rest).await,
        ["run", workflow_id, rest @ ..] => native_run(workflow_id, rest).await,
        ["help"] | ["--help"] | ["-h"] => {
            print_help();
            Ok(())
        }
        _ => Err("unknown command; run `tinyflows help`".into()),
    }
}

fn pair(arguments: &[&str]) -> Result<(), String> {
    let state_dir = option_path(arguments, "--state-dir")?.unwrap_or_else(default_state_dir);
    let port = option_u16(arguments, "--port")?.unwrap_or(32189);
    let store = SecretStore::new(secret_path(&state_dir));
    let secret = if arguments.contains(&"--rotate") {
        store.rotate()
    } else {
        store.load_or_create()
    }
    .map_err(|error| error.to_string())?;
    println!("relay_url=ws://127.0.0.1:{port}/v1/extension");
    println!("pairing_token={}", secret.expose());
    Ok(())
}

async fn start_companion(arguments: &[&str]) -> Result<(), String> {
    let extension_id = required_option(arguments, "--extension-id")?;
    let state_dir = option_path(arguments, "--state-dir")?.unwrap_or_else(default_state_dir);
    let workflows_dir =
        option_path(arguments, "--workflows-dir")?.unwrap_or_else(|| state_dir.join("workflows"));
    let port = option_u16(arguments, "--port")?.unwrap_or(32189);
    std::fs::create_dir_all(&workflows_dir).map_err(|error| error.to_string())?;
    let secret = SecretStore::new(secret_path(&state_dir))
        .load_or_create()
        .map_err(|error| error.to_string())?;
    let server = CompanionServer::new(CompanionServerConfig {
        policy: RelayPolicy::loopback(port),
        extension_id: extension_id.to_owned(),
        pairing_secret: secret,
        workflows_dir,
        capabilities: standalone_capabilities(),
    })
    .map_err(|error| error.to_string())?;
    eprintln!("TinyFlows companion listening on {}", server.bind_addr());
    server.serve().await.map_err(|error| error.to_string())
}

async fn native_get(resource: &str, arguments: &[&str]) -> Result<(), String> {
    let (url, secret) = native_connection(arguments)?;
    let response = reqwest::Client::new()
        .get(format!("{url}/v1/native/{resource}"))
        .bearer_auth(secret.expose())
        .send()
        .await
        .map_err(|error| error.to_string())?;
    print_response(response).await
}

async fn native_run(workflow_id: &str, arguments: &[&str]) -> Result<(), String> {
    let tab_id = required_option(arguments, "--tab")?
        .parse::<u64>()
        .map_err(|_| "--tab must be a non-negative integer".to_string())?;
    let input = option(arguments, "--input")?
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| format!("invalid --input JSON: {error}"))?
        .unwrap_or(Value::Null);
    let (url, secret) = native_connection(arguments)?;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/native/runs"))
        .bearer_auth(secret.expose())
        .json(&json!({"workflow_id":workflow_id,"tab_id":tab_id,"input":input}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    print_response(response).await
}

fn native_connection(
    arguments: &[&str],
) -> Result<(String, tinyflows::companion::PairingSecret), String> {
    let state_dir = option_path(arguments, "--state-dir")?.unwrap_or_else(default_state_dir);
    let port = option_u16(arguments, "--port")?.unwrap_or(32189);
    let secret = SecretStore::new(secret_path(&state_dir))
        .load()
        .map_err(|error| error.to_string())?;
    Ok((format!("http://127.0.0.1:{port}"), secret))
}

async fn print_response(response: reqwest::Response) -> Result<(), String> {
    let status = response.status();
    let text = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("companion returned {status}: {text}"));
    }
    let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn option<'a>(arguments: &'a [&str], name: &str) -> Result<Option<&'a str>, String> {
    let Some(index) = arguments.iter().position(|argument| *argument == name) else {
        return Ok(None);
    };
    arguments
        .get(index + 1)
        .copied()
        .map(Some)
        .ok_or_else(|| format!("{name} requires a value"))
}

fn required_option<'a>(arguments: &'a [&str], name: &str) -> Result<&'a str, String> {
    option(arguments, name)?.ok_or_else(|| format!("missing required {name}"))
}

fn option_path(arguments: &[&str], name: &str) -> Result<Option<PathBuf>, String> {
    option(arguments, name).map(|value| value.map(PathBuf::from))
}

fn option_u16(arguments: &[&str], name: &str) -> Result<Option<u16>, String> {
    option(arguments, name)?
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| format!("{name} must be a valid port"))
        })
        .transpose()
}

fn extension_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("extension/dist")
}

fn default_state_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("TINYFLOWS_HOME") {
        return PathBuf::from(value);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tinyflows")
}

fn secret_path(state_dir: &Path) -> PathBuf {
    state_dir.join("credentials/chrome-extension-relay.secret")
}

fn print_help() {
    println!(
        "tinyflows commands:\n  extension path\n  pair [--rotate] [--port N] [--state-dir PATH]\n  companion start --extension-id ID [--workflows-dir PATH] [--port N]\n  tabs [--port N]\n  workflows [--port N]\n  run WORKFLOW_ID --tab TAB_ID [--input JSON] [--port N]"
    );
}

fn unavailable(capability: &str) -> EngineError {
    EngineError::Capability(format!(
        "{capability} is not configured in the standalone companion; embed CompanionServer with host capabilities"
    ))
}

struct NoLlm;
#[async_trait]
impl LlmProvider for NoLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        Err(unavailable("llm"))
    }
}

struct NoTools;
#[async_trait]
impl ToolInvoker for NoTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> EngineResult<Value> {
        Err(unavailable(&format!("integration tool `{slug}`")))
    }
}

struct NoHttp;
#[async_trait]
impl HttpClient for NoHttp {
    async fn request(&self, _request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        Err(unavailable("http client"))
    }
}

struct NoCode;
#[async_trait]
impl CodeRunner for NoCode {
    async fn run(
        &self,
        _language: CodeLanguage,
        _source: &str,
        _input: Value,
    ) -> EngineResult<Value> {
        Err(unavailable("code runner"))
    }
}

#[derive(Default)]
struct MemoryState(Mutex<HashMap<String, Value>>);
#[async_trait]
impl StateStore for MemoryState {
    async fn load(&self, key: &str) -> EngineResult<Option<Value>> {
        Ok(self.0.lock().ok().and_then(|state| state.get(key).cloned()))
    }
    async fn store(&self, key: &str, value: Value) -> EngineResult<()> {
        self.0
            .lock()
            .map_err(|_| unavailable("state store"))?
            .insert(key.to_owned(), value);
        Ok(())
    }
}

struct NoResolver;
#[async_trait]
impl WorkflowResolver for NoResolver {
    async fn resolve(&self, _workflow_id: &str) -> EngineResult<WorkflowGraph> {
        Err(unavailable("workflow resolver"))
    }
}

fn standalone_capabilities() -> tinyflows::caps::Capabilities {
    tinyflows::caps::Capabilities {
        llm: Arc::new(NoLlm),
        tools: Arc::new(NoTools),
        http: Arc::new(NoHttp),
        code: Arc::new(NoCode),
        state: Arc::new(MemoryState::default()),
        resolver: Arc::new(NoResolver),
        agent: None,
    }
}
