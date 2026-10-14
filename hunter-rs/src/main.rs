//! CLI entry point: `hunter [--root <path>] [--port <port>]` runs the
//! daemon; `hunter [--root <path>] user <add|passwd|disable> <name>`
//! manages accounts.
//!
//! Hand-parsed args -- a handful of words do not justify a CLI dependency.
//! The daemon takes exclusive locks on <`work_root>/hunter.lock` and
//! <`db_path>-daemon.lock` (`recovery::DaemonLock`) and owns the database
//! schema: it runs the embedded migrations on startup. `user` commands do
//! not take the locks -- they are meant to run next to a live daemon --
//! but they open the database read-write and run the same migrations.
//! Their store has no startup token, so they cannot reconcile or create
//! jobs under the daemon.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use hunter::config::Config;

const USAGE: &str = "usage: hunter [--root <path>] [--port <port>]
       hunter [--root <path>] user add <name>
       hunter [--root <path>] user passwd <name>
       hunter [--root <path>] user disable <name>

  --root <path>  hunter/ project root to serve from (default: ../hunter)
  --port <port>  listen port (default: serve.port from config.json)

  user add       create an account; the password is read from the terminal
                 (asked twice) or, when stdin is not a terminal, its first line
  user passwd    set a new password; logs the account out everywhere
  user disable   disable the account; logs it out everywhere";

enum UserCommand {
    Add(String),
    Passwd(String),
    Disable(String),
}

struct Cli {
    root: PathBuf,
    port: Option<u16>,
    user: Option<UserCommand>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut root = PathBuf::from("../hunter");
    let mut port = None;
    let mut user = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                root = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--root requires a value".to_owned())?;
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_owned())?;
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port {value:?}"))?,
                );
            }
            "user" => {
                let action = args.next().ok_or("user requires add, passwd or disable")?;
                let name = args
                    .next()
                    .ok_or_else(|| format!("user {action} requires a name"))?;
                user = Some(match action.as_str() {
                    "add" => UserCommand::Add(name),
                    "passwd" => UserCommand::Passwd(name),
                    "disable" => UserCommand::Disable(name),
                    other => return Err(format!("unknown user command {other:?}")),
                });
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Cli { root, port, user })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("error: {msg}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("fatal: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let mut cfg = Config::load(&cli.root)?;
    if let Some(command) = cli.user {
        return run_user(&cfg, command).await;
    }
    if let Some(port) = cli.port {
        cfg.serve_port = port;
    }
    hunter::daemon::run_daemon(cfg).await
}

async fn run_user(cfg: &Config, command: UserCommand) -> anyhow::Result<()> {
    let store = hunter::store::Store::connect(&cfg.db_path).await?;
    match command {
        UserCommand::Add(name) => {
            let password = read_password()?;
            hunter::auth::add_user(&store, &name, password).await?;
            println!("created user {name}");
        }
        UserCommand::Passwd(name) => {
            let password = read_password()?;
            hunter::auth::change_password(&store, &name, password).await?;
            println!("password changed for {name}; its sessions are logged out");
        }
        UserCommand::Disable(name) => {
            hunter::auth::disable_user(&store, &name).await?;
            println!("disabled {name}; its sessions are logged out");
        }
    }
    Ok(())
}

/// A password from the terminal, typed twice without echo, or the first
/// line of a non-terminal stdin (`hunter user add me < file`).
fn read_password() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
    }
    let first = prompt_hidden("password: ")?;
    let second = prompt_hidden("again: ")?;
    anyhow::ensure!(first == second, "the passwords differ");
    Ok(first)
}

/// Read one line from the terminal with echo switched off.
fn prompt_hidden(prompt: &str) -> anyhow::Result<String> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin)?;
    let mut silent = saved.clone();
    silent.local_flags.remove(LocalFlags::ECHO);
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    tcsetattr(&stdin, SetArg::TCSANOW, &silent)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    tcsetattr(&stdin, SetArg::TCSANOW, &saved)?;
    eprintln!();
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}
