//! codex-api-wrapper — proof of concept.
//!
//! Goal at this stage: run the official ChatGPT auth flow against the Codex
//! crates and then be able to issue requests, so the open questions from
//! KONTEXT-HARNESS.md 8.3/10 can be *measured* instead of researched.
//!
//! What this deliberately is NOT yet: a server, a process pool, an
//! OpenAI-compatible translation. That only comes once it is settled whether the
//! provider contract (own tools, client executes them) is satisfiable at all.

mod chatgpt;
mod session;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

/// Default model. Changeable via `--model`; `models` lists what the subscription offers.
///
/// Measured 2026-08-24: the `gpt-5.x-codex` slugs from the prompt files in the
/// upstream repo no longer exist. The backend serves gpt-5.6-{sol,terra,luna},
/// gpt-5.5, gpt-5.4{,-mini}. Run `models` before changing this.
const DEFAULT_MODEL: &str = "gpt-5.6-sol";

/// Appended to `/models` as the `client_version` query parameter. Taken from the
/// most recent upstream release tag, because the repo checkout itself carries
/// `0.0.0` (the real version is only substituted at release time).
const DEFAULT_CLIENT_VERSION: &str = "0.150.0";

#[derive(Parser)]
#[command(
    name = "codex-api-wrapper",
    about = "Proof of concept: reach the ChatGPT subscription via the official Codex crates"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Official ChatGPT OAuth flow (PKCE, local callback server).
    Login,
    /// Revoke tokens and delete local credentials.
    Logout,
    /// Shows the current auth state (plan, account, token length).
    Whoami,
    /// Lists the models the backend allows for this account.
    Models {
        #[arg(long, default_value = DEFAULT_CLIENT_VERSION)]
        client_version: String,
    },
    /// Issue a single Responses request.
    Ask {
        /// The user prompt.
        prompt: String,

        #[arg(long, default_value = DEFAULT_MODEL)]
        model: String,

        /// Reasoning effort (`low`, `medium`, `high`, ...). Without it no
        /// `reasoning` field is sent.
        #[arg(long)]
        effort: Option<String>,

        /// `instructions` as text. Without it the field is omitted — that is the
        /// test from KONTEXT-HARNESS.md 8.3.
        #[arg(long, conflicts_with = "instructions_file")]
        instructions: Option<String>,

        /// `instructions` from a file. The official prompts now ship from the
        /// backend itself (`models` -> `model_messages.instructions_template`);
        /// the files under `../codex/codex-rs/core/` are leftovers.
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
        Command::Login => session::login().await,
        Command::Logout => session::logout().await,
        Command::Whoami => session::whoami().await,
        Command::Models { client_version } => {
            let manager = session::auth_manager().await?;
            session::current_auth(&manager).await?;
            chatgpt::models(manager, &client_version).await
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
            let manager = session::auth_manager().await?;
            // Fail early when there is no login — otherwise the error only
            // surfaces as a 401 out of the stream.
            session::current_auth(&manager).await?;

            let instructions = match (instructions, instructions_file) {
                (Some(text), _) => Some(text),
                (None, Some(path)) => Some(chatgpt::load_instructions(&path)?),
                (None, None) => None,
            };
            let tools = match tools_file {
                Some(path) => Some(chatgpt::load_tools(&path)?),
                None => None,
            };

            let opts = chatgpt::AskOptions {
                prompt,
                model,
                effort,
                instructions,
                tools,
                store,
                session_id: session_id.unwrap_or_else(new_session_id),
                dump_dir,
            };

            if raw {
                chatgpt::ask_raw(manager, opts).await
            } else {
                chatgpt::ask_decoded(manager, opts).await
            }
        }
    }
}

/// A UUIDv4-shaped session id.
///
/// No `uuid` crate, because the value is just a correlation handle here. Should
/// it turn out that the backend checks the shape, this becomes a real
/// dependency.
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
