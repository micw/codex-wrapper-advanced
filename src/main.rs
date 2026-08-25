//! CLI shell around [`codex_api_wrapper`].
//!
//! Argument handling and presentation only. Anything the daemon needs as well
//! lives in the library.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use codex_api_wrapper::DEFAULT_CLIENT_VERSION;
use codex_api_wrapper::DEFAULT_MODEL;
use codex_api_wrapper::auth;
use codex_api_wrapper::client::Client;
use codex_api_wrapper::client::build_body;
use codex_api_wrapper::client::raw_stream;
use codex_api_wrapper::serve;
use codex_api_wrapper::user_input;
use codex_api_wrapper::wire::Event;
use codex_api_wrapper::wire::StreamRequest;
use futures::StreamExt;
use serde_json::Value;

#[derive(Parser)]
#[command(
    name = "codex-api-wrapper",
    about = "The ChatGPT subscription via the official Codex crates"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Official ChatGPT OAuth flow (PKCE, local callback server).
    Login {
        /// Device code flow instead of a callback server — for containers and
        /// remote machines where `localhost:1455` is not reachable.
        #[arg(long)]
        device: bool,

        /// Only check whether the device flow is enabled. Signs nothing in.
        #[arg(long, requires = "device")]
        probe: bool,
    },
    /// Revoke tokens and delete local credentials.
    Logout,
    /// Shows the current auth state (plan, account, token length).
    Whoami,
    /// Lists the models the backend allows for this account.
    Models {
        #[arg(long, default_value = DEFAULT_CLIENT_VERSION)]
        client_version: String,
    },
    /// Start the local REST API. One process, arbitrarily many requests.
    Serve {
        /// Also write to this file (mode 0600).
        #[arg(long)]
        server_info: Option<PathBuf>,

        /// Bind address. Loopback by default. `0.0.0.0` makes the daemon
        /// reachable from other network namespaces — needed in a container when it
        /// is NOT a sidecar. See DEPLOY.md.
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,

        /// A fixed port instead of an ephemeral one. Useful in a container,
        /// where a port gets mapped anyway.
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Issue a single Responses request.
    Ask {
        /// The user prompt.
        prompt: String,

        #[arg(long, default_value = DEFAULT_MODEL)]
        model: String,

        /// Reasoning effort (`low`, `medium`, `high`, `xhigh`, `max`, `ultra`).
        /// Without it no `reasoning` field is sent.
        #[arg(long)]
        effort: Option<String>,

        /// `instructions` as text. Without it the field is omitted — the
        /// backend does not require it (MESSUNGEN.md §1).
        #[arg(long, conflicts_with = "instructions_file")]
        instructions: Option<String>,

        /// `instructions` from a file. The official prompts now ship from the
        /// backend itself (`models` -> `model_messages.instructions_template`).
        #[arg(long)]
        instructions_file: Option<String>,

        /// JSON file holding an array of tool definitions. Without it
        /// `"tools": []` is sent.
        #[arg(long)]
        tools_file: Option<String>,

        /// Send `store: true`. The default is `false`, as the CLI does in
        /// subscription mode.
        #[arg(long)]
        store: bool,

        /// Print the unparsed SSE response instead of decoded events.
        #[arg(long)]
        raw: bool,

        /// Write request body, headers and response into this directory.
        #[arg(long)]
        dump_dir: Option<String>,

        /// `session-id` header. One is generated when omitted.
        #[arg(long)]
        session_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Login { device, probe } => {
            if device {
                auth::login_device(probe).await
            } else {
                auth::login().await
            }
        }
        Command::Logout => auth::logout().await,
        Command::Whoami => auth::whoami().await,
        Command::Serve {
            server_info,
            bind,
            port,
        } => {
            serve::run(
                server_info,
                serve::BindConfig {
                    address: bind,
                    port,
                },
            )
            .await
        }
        Command::Models { client_version } => {
            let manager = auth::auth_manager().await?;
            auth::current_auth(&manager).await?;
            let models = Client::new(manager).models(&client_version).await?;
            println!("{}", serde_json::to_string_pretty(&models)?);
            Ok(())
        }
        Command::Ask {
            prompt,
            model,
            effort,
            instructions,
            instructions_file,
            tools_file,
            store,
            raw,
            dump_dir,
            session_id,
        } => {
            let manager = auth::auth_manager().await?;
            // Fail early when there is no login — otherwise the error only
            // surfaces as a 401 out of the stream.
            auth::current_auth(&manager).await?;

            let instructions = match (instructions, instructions_file) {
                (Some(text), _) => Some(text),
                (None, Some(path)) => Some(load_instructions(&path)?),
                (None, None) => None,
            };
            let tools = match tools_file {
                Some(path) => Some(load_tools(&path)?),
                None => None,
            };

            let request = StreamRequest {
                model,
                input: user_input(&prompt),
                instructions,
                tools,
                effort,
                tool_choice: None,
                parallel_tool_calls: None,
                store: Some(store),
                session_id: Some(session_id.unwrap_or_else(new_session_id)),
            };

            if raw {
                ask_raw(manager, request, dump_dir.as_deref()).await
            } else {
                ask_decoded(manager, request, dump_dir.as_deref()).await
            }
        }
    }
}

// --- Execution -------------------------------------------------------------

async fn ask_decoded(
    manager: Arc<codex_login::AuthManager>,
    request: StreamRequest,
    dump_dir: Option<&str>,
) -> Result<()> {
    if let Some(dir) = dump_dir {
        dump(dir, "request.json", &to_pretty(&build_body(&request))?)?;
    }

    let mut stream = Client::new(manager).stream(request).await?;
    let mut transcript = String::new();

    while let Some(event) = stream.next().await {
        transcript.push_str(&format!("{event:?}\n"));
        print_event(&event);
    }
    println!();

    if let Some(dir) = dump_dir {
        dump(dir, "events.log", &transcript)?;
    }
    Ok(())
}

/// Deltas run on, everything else gets a marked line — so it stays visible what
/// the backend sends besides text.
fn print_event(event: &Event) {
    match event {
        Event::TextDelta { text } => print!("{text}"),
        Event::ThinkingDelta { text } => eprint!("\x1b[2m{text}\x1b[0m"),
        Event::Started { model } => eprintln!("[started] model={model:?}"),
        Event::ToolCall {
            call_id,
            name,
            arguments,
        } => eprintln!("\n[tool-call] {name}({arguments}) call_id={call_id}"),
        Event::Reasoning { summary, item } => {
            let bytes = item
                .get("encrypted_content")
                .and_then(|v| v.as_str())
                .map(str::len)
                .unwrap_or(0);
            eprintln!("\n[reasoning] {summary:?} (encrypted_content: {bytes} characters)")
        }
        Event::RateLimits { plan, primary, .. } => {
            eprintln!("[rate-limits] plan={plan:?} primary={primary:?}")
        }
        Event::Done {
            stop_reason, usage, ..
        } => eprintln!("\n[done] {stop_reason} usage={usage:?}"),
        Event::Failed { message, retryable } => {
            eprintln!("\n[failed] retryable={retryable} {message}")
        }
    }
}

async fn ask_raw(
    manager: Arc<codex_login::AuthManager>,
    request: StreamRequest,
    dump_dir: Option<&str>,
) -> Result<()> {
    let body = build_body(&request);
    if let Some(dir) = dump_dir {
        dump(dir, "request.json", &to_pretty(&body)?)?;
    }

    // Shared rather than borrowed: the callback lives inside the future that
    // yields the stream, and we read the buffer once more afterwards for the
    // dump.
    let meta = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = meta.clone();
    let stream = raw_stream(manager, &request, body, move |line| {
        eprintln!("{line}");
        if let Ok(mut buffer) = sink.lock() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    })
    .await?;

    let mut raw = String::new();
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| anyhow::anyhow!("stream error: {err}"))?;
        let text = String::from_utf8_lossy(&chunk);
        print!("{text}");
        raw.push_str(&text);
    }
    println!();

    if let Some(dir) = dump_dir {
        let meta = meta.lock().map(|m| m.clone()).unwrap_or_default();
        dump(dir, "meta.txt", &meta)?;
        dump(dir, "response-raw.sse", &raw)?;
    }
    Ok(())
}

// --- Helpers ---------------------------------------------------------------

fn to_pretty(value: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

fn dump(dir: &str, name: &str, content: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating dump directory: {dir}"))?;
    let path = Path::new(dir).join(name);
    std::fs::write(&path, content).with_context(|| format!("writing: {}", path.display()))?;
    eprintln!("  -> {}", path.display());
    Ok(())
}

/// Reads tool definitions from a JSON file.
fn load_tools(path: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading: {path}"))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing JSON: {path}"))?;
    if !value.is_array() {
        bail!("{path}: expected a JSON array of tool definitions");
    }
    Ok(value)
}

fn load_instructions(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading: {path}"))
}

/// A UUIDv4-shaped session id.
///
/// No `uuid` crate, because the value is just a correlation handle. The backend
/// derives its `prompt_cache_key` from it but does not check the shape.
fn new_session_id() -> String {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mix = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(pid);
    let hex = format!("{mix:032x}");

    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}
