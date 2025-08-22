use std::fs;
use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;
use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_exec::Cli as ExecCli;

#[derive(Debug, Parser)]
pub struct SessionsCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    cmd: SessionsCommand,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List recorded session rollout files
    List,

    /// Resume from an existing session rollout file (continues the same session)
    Resume(ResumeArgs),
}

#[derive(Debug, Parser)]
pub struct ResumeArgs {
    /// Session ID (UUID suffix in the rollout filename) or path to a rollout .jsonl
    #[arg(value_name = "SESSION_ID_OR_PATH")]
    target: String,

    /// Next user prompt to continue with
    #[arg(long = "prompt")]
    prompt: String,

    /// Optional model override for the resumed run
    #[arg(long = "model", short = 'm')]
    model: Option<String>,

    /// Optional profile to apply
    #[arg(long = "profile", short = 'p')]
    profile: Option<String>,

    /// Optional working directory override
    #[arg(long = "cd", short = 'C')]
    cwd: Option<PathBuf>,

    /// Run in full-auto (workspace-write sandbox + no confirmations)
    #[arg(long = "full-auto", default_value_t = false)]
    full_auto: bool,

    /// Danger: run without sandbox/approvals
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false
    )]
    yolo: bool,
}

pub async fn run_main(
    cli: SessionsCli,
    codex_linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    match cli.cmd {
        SessionsCommand::List => list_sessions(cli.config_overrides).await,
        SessionsCommand::Resume(args) => {
            resume_session(cli.config_overrides, args, codex_linux_sandbox_exe).await
        }
    }
}

async fn list_sessions(overrides: CliConfigOverrides) -> anyhow::Result<()> {
    let cfg = load_config_min(&overrides)?;
    let base = cfg.codex_home.join("sessions");
    let mut files = Vec::new();
    if base.exists() {
        collect_rollouts(&base, &mut files)?;
    }
    // Sort by path for stability
    files.sort();
    for f in files {
        let sid = session_id_from_filename(&f).unwrap_or_else(|| "?".to_string());
        println!("{}  {}", sid, f.display());
    }
    Ok(())
}

async fn resume_session(
    overrides: CliConfigOverrides,
    args: ResumeArgs,
    codex_linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    let cfg = load_config_min(&overrides)?;
    let rollout_path = resolve_target_to_path(&cfg.codex_home, &args.target)?;
    if !rollout_path.exists() {
        anyhow::bail!(format!(
            "rollout file not found: {}",
            rollout_path.display()
        ));
    }

    // Build an ExecCli with the resume override baked into -c.
    let mut raw_overrides = overrides.raw_overrides.clone();
    raw_overrides.push(format!(
        "experimental_resume=\"{}\"",
        rollout_path.to_string_lossy().replace('\\', "/")
    ));

    let exec_cli = ExecCli {
        images: Vec::new(),
        model: args.model,
        oss: false,
        sandbox_mode: None,
        config_profile: args.profile,
        full_auto: args.full_auto,
        dangerously_bypass_approvals_and_sandbox: args.yolo,
        cwd: args.cwd,
        skip_git_repo_check: false,
        config_overrides: CliConfigOverrides { raw_overrides },
        color: codex_exec::Color::Auto,
        json: false,
        last_message_file: None,
        prompt: Some(args.prompt),
    };

    codex_exec::run_main(exec_cli, codex_linux_sandbox_exe).await
}

fn load_config_min(overrides: &CliConfigOverrides) -> anyhow::Result<Config> {
    // We only need codex_home resolved; load a minimal Config without changing defaults.
    let parsed = overrides
        .clone()
        .parse_overrides()
        .map_err(|e| anyhow::anyhow!(e))?;
    let cfg = Config::load_with_cli_overrides(parsed, ConfigOverrides::default())?;
    Ok(cfg)
}

fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, out)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

fn session_id_from_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    // rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl
    let stem = name.trim_end_matches(".jsonl");
    let idx = stem.rfind('-')?;
    Some(stem[idx + 1..].to_string())
}

fn resolve_target_to_path(codex_home: &Path, target: &str) -> anyhow::Result<PathBuf> {
    let as_path = PathBuf::from(target);
    if as_path.exists() {
        return Ok(as_path);
    }
    // Treat as session id; search under codex_home/sessions
    let base = codex_home.join("sessions");
    let mut matches = Vec::new();
    if base.exists() {
        let mut files = Vec::new();
        collect_rollouts(&base, &mut files)?;
        for f in files {
            if session_id_from_filename(&f).as_deref() == Some(target) {
                matches.push(f);
            }
        }
    }
    matches.sort();
    matches
        .pop()
        .ok_or_else(|| anyhow::anyhow!(format!("No rollout found for session id: {target}")))
}
