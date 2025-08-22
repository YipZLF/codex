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

    /// Show a session timeline and discovered response ids
    Show(ShowArgs),

    /// Create a branch pointer at a specific response id
    Branch(BranchArgs),

    /// Set the active branch pointer
    Checkout(CheckoutArgs),
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

    /// Resume from a specific response id (server-side chaining)
    #[arg(long = "at")]
    at: Option<String>,

    /// Resume from the n-th recorded step (0-based). Internally resolves to a response id.
    #[arg(long = "step")]
    step: Option<usize>,

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
        SessionsCommand::Show(args) => show_session(cli.config_overrides, args).await,
        SessionsCommand::Branch(args) => branch_session(cli.config_overrides, args).await,
        SessionsCommand::Checkout(args) => checkout_session(cli.config_overrides, args).await,
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

    // Optionally resolve --step to a response id by scanning the rollout file's state lines.
    let mut at = args.at.clone();
    if at.is_none()
        && let Some(step) = args.step
    {
        if let Some(id) = resolve_step_to_response_id(&rollout_path, step)? {
            at = Some(id);
        } else {
            anyhow::bail!(format!("No response id found at step {step}"));
        }
    }

    // Build an ExecCli with the resume override baked into -c.
    let mut raw_overrides = overrides.raw_overrides.clone();
    raw_overrides.push(format!(
        "experimental_resume=\"{}\"",
        rollout_path.to_string_lossy().replace('\\', "/")
    ));
    if let Some(at) = &at {
        raw_overrides.push(format!("experimental_previous_response_id=\"{}\"", at));
    }

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

fn resolve_step_to_response_id(path: &Path, step: usize) -> anyhow::Result<Option<String>> {
    let text = std::fs::read_to_string(path)?;
    let mut ids = Vec::new();
    for line in text.lines() {
        if !line.contains("\"record_type\":\"state\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(id) = v
                .get("last_response_id")
                .or_else(|| v.get("state").and_then(|s| s.get("last_response_id")))
                .and_then(|x| x.as_str())
        {
            if ids.last().map(|s: &String| s == id).unwrap_or(false) {
                continue;
            }
            ids.push(id.to_string());
        }
    }
    Ok(ids.get(step).cloned())
}

#[derive(Debug, Parser)]
pub struct ShowArgs {
    /// Session ID (UUID suffix) or path to a rollout .jsonl file
    #[arg(value_name = "SESSION_ID_OR_PATH")]
    target: String,
}

async fn show_session(overrides: CliConfigOverrides, args: ShowArgs) -> anyhow::Result<()> {
    let cfg = load_config_min(&overrides)?;
    let path = resolve_target_to_path(&cfg.codex_home, &args.target)?;
    let text = std::fs::read_to_string(&path)?;
    println!("Session: {}", path.display());
    let mut step = 0usize;
    let mut last_id: Option<String> = None;
    for line in text.lines() {
        if !line.contains("\"record_type\":\"state\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let id = v
                .get("last_response_id")
                .or_else(|| v.get("state").and_then(|s| s.get("last_response_id")))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if id.is_some() && id != last_id {
                let disp = id
                    .as_ref()
                    .map(|s| if s.len() > 12 { &s[..12] } else { s })
                    .unwrap();
                let ts = v
                    .get("created_at")
                    .or_else(|| v.get("state").and_then(|s| s.get("created_at")))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let summary = v
                    .get("summary")
                    .or_else(|| v.get("state").and_then(|s| s.get("summary")))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if ts.is_empty() && summary.is_empty() {
                    println!("  [{}] resp: {}", step, disp);
                } else {
                    println!("  [{}] resp: {}  {}  {}", step, disp, ts, summary);
                }
                step += 1;
                last_id = id;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Parser)]
pub struct BranchArgs {
    /// Session ID (UUID suffix) or path to a rollout .jsonl file
    #[arg(value_name = "SESSION_ID_OR_PATH")]
    target: String,

    /// Base response id to branch from
    #[arg(long = "from")]
    from: String,

    /// Branch name
    #[arg(long = "name")]
    name: String,
}

async fn branch_session(overrides: CliConfigOverrides, args: BranchArgs) -> anyhow::Result<()> {
    let cfg = load_config_min(&overrides)?;
    let path = resolve_target_to_path(&cfg.codex_home, &args.target)?;
    let session_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let meta_path = session_dir.join("resume-index.json");
    let mut meta: serde_json::Value = if meta_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&meta_path)?)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if meta.get("sessionId").is_none() {
        meta["sessionId"] = serde_json::json!(session_id_from_filename(&path).unwrap_or_default());
    }
    if meta.get("branches").is_none() {
        meta["branches"] = serde_json::json!([]);
    }
    let branches = meta["branches"].as_array_mut().unwrap();
    if branches
        .iter()
        .any(|b| b.get("name").and_then(|n| n.as_str()) == Some(args.name.as_str()))
    {
        anyhow::bail!(format!("Branch already exists: {}", args.name));
    }
    let branch = serde_json::json!({
        "branchId": format!("b_{}", rand::random::<u32>()),
        "name": args.name,
        "baseResponseId": args.from,
        "tipResponseId": args.from,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    });
    branches.push(branch);
    meta["head"] = serde_json::json!(args.name);
    meta["updatedAt"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    std::fs::write(meta_path, serde_json::to_string_pretty(&meta)?)?;
    println!("Created branch '{}' at {}", args.name, args.from);
    Ok(())
}

#[derive(Debug, Parser)]
pub struct CheckoutArgs {
    /// Session ID (UUID suffix) or path to a rollout .jsonl file
    #[arg(value_name = "SESSION_ID_OR_PATH")]
    target: String,

    /// Branch name to make active
    #[arg(long = "branch")]
    branch: String,
}

async fn checkout_session(overrides: CliConfigOverrides, args: CheckoutArgs) -> anyhow::Result<()> {
    let cfg = load_config_min(&overrides)?;
    let path = resolve_target_to_path(&cfg.codex_home, &args.target)?;
    let session_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let meta_path = session_dir.join("resume-index.json");
    let mut meta: serde_json::Value = if meta_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&meta_path)?)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        anyhow::bail!("No branches found for this session");
    };
    let branches = meta["branches"].as_array().cloned().unwrap_or_default();
    if !branches
        .iter()
        .any(|b| b.get("name").and_then(|n| n.as_str()) == Some(args.branch.as_str()))
    {
        anyhow::bail!(format!("Branch not found: {}", args.branch));
    }
    meta["head"] = serde_json::json!(args.branch);
    meta["updatedAt"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    std::fs::write(meta_path, serde_json::to_string_pretty(&meta)?)?;
    println!(
        "Checked out branch '{}'.",
        meta["head"].as_str().unwrap_or("")
    );
    Ok(())
}
