use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};
use hermes_config::config::{AppConfig, hermes_home};
use hermes_managed::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedAgentYaml,
    ManagedAgentYamlDiff, ManagedAgentYamlFieldDiff, ManagedApprovalPolicy, ManagedModelPreflight,
    ManagedStore, build_filtered_skill_manager, extract_sync_metadata_sha256,
    preflight_managed_model, resolve_managed_version_defaults, validate_managed_agent_name,
    validate_managed_beta_tools,
};
use hermes_skills::SkillManager;

#[derive(Subcommand, Debug)]
pub enum AgentsAction {
    /// List managed agents
    List {
        /// Maximum number of agents to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Include archived agents
        #[arg(long)]
        all: bool,
    },
    /// Create a managed agent
    Create {
        /// Agent name
        name: String,
    },
    /// Show one managed agent by id or name
    Get {
        /// Agent id or name
        agent: String,
    },
    /// Archive a managed agent by id or name
    Archive {
        /// Agent id or name
        agent: String,
    },
    /// Show YAML drift for one managed agent file
    Diff {
        /// YAML file path or agent name resolved via ~/.hermes/agents/<name>.yaml
        file: PathBuf,
    },
    /// Sync managed agents from YAML files
    Sync {
        /// YAML files or directories. Defaults to ~/.hermes/agents when omitted.
        paths: Vec<PathBuf>,
        /// Show planned changes without mutating the DB
        #[arg(long)]
        dry_run: bool,
        /// Apply without interactive confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Managed agent version commands
    #[command(subcommand)]
    Versions(AgentVersionsAction),
}

#[derive(Subcommand, Debug)]
pub enum AgentVersionsAction {
    /// List versions for one managed agent
    List {
        /// Agent id or name
        agent: String,
    },
    /// Show one managed agent version
    Get {
        /// Agent id or name
        agent: String,
        /// Version number
        version: u32,
    },
    /// Publish the next managed agent version
    Create {
        /// Agent id or name
        agent: String,
        /// Provider/model string like openai/gpt-4o. Falls back to ~/.hermes/.env when omitted.
        #[arg(long)]
        model: Option<String>,
        /// System prompt text
        #[arg(long = "system-prompt")]
        system_prompt: String,
        /// Optional provider base URL override. Falls back to ~/.hermes/.env when omitted.
        #[arg(long)]
        base_url: Option<String>,
        /// Allowed managed beta tool, repeat to add more
        #[arg(long = "tool")]
        allowed_tools: Vec<String>,
        /// Allowed skill name, repeat to add more
        #[arg(long = "skill")]
        allowed_skills: Vec<String>,
        /// Max iterations for this version
        #[arg(long, default_value_t = 90)]
        max_iterations: u32,
        /// Temperature for this version
        #[arg(long, default_value_t = 0.0)]
        temperature: f64,
        /// Approval policy for this version
        #[arg(long, value_enum, default_value_t = ApprovalArg::Ask)]
        approval: ApprovalArg,
        /// Run timeout in seconds
        #[arg(long, default_value_t = 300)]
        timeout_secs: u32,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ApprovalArg {
    Ask,
    Yolo,
    Deny,
}

impl From<ApprovalArg> for ManagedApprovalPolicy {
    fn from(value: ApprovalArg) -> Self {
        match value {
            ApprovalArg::Ask => Self::Ask,
            ApprovalArg::Yolo => Self::Yolo,
            ApprovalArg::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncAction {
    CreateAgentAndVersion,
    PublishVersion,
    WriteMetadata,
    Noop,
}

impl SyncAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::CreateAgentAndVersion => "create_agent_and_version",
            Self::PublishVersion => "publish_version",
            Self::WriteMetadata => "write_metadata",
            Self::Noop => "noop",
        }
    }
}

#[derive(Debug, Clone)]
struct AgentSyncPlan {
    source_path: PathBuf,
    spec: ManagedAgentYaml,
    existing_agent: Option<ManagedAgent>,
    latest_version: Option<ManagedAgentVersion>,
    desired_hash: String,
    current_hash: Option<String>,
    metadata_hash: Option<String>,
    diff: ManagedAgentYamlDiff,
    action: SyncAction,
}

pub async fn run_agents(action: AgentsAction) -> anyhow::Result<()> {
    let store = ManagedStore::open().await?;
    let app_config = AppConfig::load();

    match action {
        AgentsAction::List { limit, all } => list_agents(&store, limit, all).await,
        AgentsAction::Create { name } => create_agent(&store, &name).await,
        AgentsAction::Get { agent } => get_agent(&store, &agent).await,
        AgentsAction::Archive { agent } => archive_agent(&store, &agent).await,
        AgentsAction::Diff { file } => diff_agent_yaml(&store, &file, &app_config).await,
        AgentsAction::Sync {
            paths,
            dry_run,
            yes,
        } => sync_agent_yamls(&store, &paths, dry_run, yes, &app_config).await,
        AgentsAction::Versions(action) => run_agent_versions(&store, action, &app_config).await,
    }
}

async fn run_agent_versions(
    store: &ManagedStore,
    action: AgentVersionsAction,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    match action {
        AgentVersionsAction::List { agent } => list_versions(store, &agent).await,
        AgentVersionsAction::Get { agent, version } => get_version(store, &agent, version).await,
        AgentVersionsAction::Create {
            agent,
            model,
            system_prompt,
            base_url,
            allowed_tools,
            allowed_skills,
            max_iterations,
            temperature,
            approval,
            timeout_secs,
        } => {
            let args = CreateVersionArgs {
                agent,
                model,
                system_prompt,
                base_url,
                allowed_tools,
                allowed_skills,
                max_iterations,
                temperature,
                approval: approval.into(),
                timeout_secs,
            };
            create_version(store, args, app_config).await
        }
    }
}

async fn list_agents(store: &ManagedStore, limit: usize, all: bool) -> anyhow::Result<()> {
    let mut agents = store.list_agents(limit.clamp(1, 1000)).await?;
    if !all {
        agents.retain(|agent| !agent.archived);
    }

    if agents.is_empty() {
        println!("No managed agents found.");
        return Ok(());
    }

    println!(
        "{:<24} {:<20} {:<8} {:<9} Updated",
        "ID", "Name", "Latest", "Archived"
    );
    println!("{}", "-".repeat(88));
    for agent in &agents {
        println!(
            "{:<24} {:<20} {:<8} {:<9} {}",
            truncate(&agent.id, 24),
            truncate(&agent.name, 20),
            agent.latest_version,
            if agent.archived { "yes" } else { "no" },
            format_ts(agent.updated_at),
        );
    }
    Ok(())
}

async fn create_agent(store: &ManagedStore, name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    validate_managed_agent_name(name).map_err(|e| anyhow::anyhow!("{e}"))?;

    let agent = ManagedAgent::new(name.to_string());
    match store.create_agent(&agent).await {
        Ok(()) => {
            println!("Created managed agent:");
            print_agent_summary(&agent);
            Ok(())
        }
        Err(e) if e.to_string().contains("UNIQUE constraint failed") => {
            bail!("Managed agent already exists: {name}");
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

async fn get_agent(store: &ManagedStore, agent_ref: &str) -> anyhow::Result<()> {
    let agent = resolve_agent_ref(store, agent_ref).await?;
    let latest_version = load_latest_version(store, &agent).await?;

    print_agent_summary(&agent);
    match latest_version {
        Some(version) => {
            println!();
            println!("Latest version:");
            print_version_summary(&version);
        }
        None => println!("No versions published yet."),
    }

    Ok(())
}

async fn archive_agent(store: &ManagedStore, agent_ref: &str) -> anyhow::Result<()> {
    let agent = resolve_agent_ref(store, agent_ref).await?;
    if agent.archived {
        println!("Managed agent already archived: {}", agent.name);
        return Ok(());
    }

    store.archive_agent(&agent.id).await?;
    let archived = store
        .get_agent(&agent.id)
        .await?
        .context("managed agent disappeared after archive")?;

    println!("Archived managed agent:");
    print_agent_summary(&archived);
    Ok(())
}

async fn diff_agent_yaml(
    store: &ManagedStore,
    input: &Path,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    let path = resolve_yaml_input(input)?;
    let plan = build_sync_plan(store, &path, app_config).await?;
    print_sync_plan(&plan);
    Ok(())
}

async fn sync_agent_yamls(
    store: &ManagedStore,
    inputs: &[PathBuf],
    dry_run: bool,
    yes: bool,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    let paths = collect_sync_paths(inputs)?;
    let mut plans = Vec::new();

    for path in &paths {
        plans.push(build_sync_plan(store, path, app_config).await?);
    }

    validate_unique_sync_targets(&plans)?;

    for plan in &plans {
        print_sync_plan(plan);
        println!();
    }

    let create_count = plans
        .iter()
        .filter(|plan| plan.action == SyncAction::CreateAgentAndVersion)
        .count();
    let publish_count = plans
        .iter()
        .filter(|plan| plan.action == SyncAction::PublishVersion)
        .count();
    let metadata_count = plans
        .iter()
        .filter(|plan| plan.action == SyncAction::WriteMetadata)
        .count();
    let noop_count = plans
        .iter()
        .filter(|plan| plan.action == SyncAction::Noop)
        .count();

    println!(
        "Plan summary: {} create, {} publish, {} metadata, {} noop",
        create_count, publish_count, metadata_count, noop_count
    );

    if dry_run {
        println!("Dry run only; no changes applied.");
        return Ok(());
    }

    let apply_count = create_count + publish_count + metadata_count;
    if apply_count == 0 {
        println!("No changes to apply.");
        return Ok(());
    }

    if !yes && !confirm_sync_apply(apply_count)? {
        println!("Aborted.");
        return Ok(());
    }

    for plan in &plans {
        apply_sync_plan(store, plan, app_config).await?;
    }

    Ok(())
}

async fn list_versions(store: &ManagedStore, agent_ref: &str) -> anyhow::Result<()> {
    let agent = resolve_agent_ref(store, agent_ref).await?;
    let versions = store.list_agent_versions(&agent.id).await?;

    if versions.is_empty() {
        println!("No versions published for {}.", agent.name);
        return Ok(());
    }

    println!("Managed agent: {} ({})", agent.name, agent.id);
    println!(
        "{:<8} {:<28} {:<6} {:<8} {:<10} Created",
        "Version", "Model", "Tools", "Skills", "Approval"
    );
    println!("{}", "-".repeat(104));
    for version in &versions {
        println!(
            "{:<8} {:<28} {:<6} {:<8} {:<10} {}",
            version.version,
            truncate(&version.model, 28),
            version.allowed_tools.len(),
            version.allowed_skills.len(),
            version.approval_policy.as_str(),
            format_ts(version.created_at),
        );
    }

    Ok(())
}

async fn get_version(
    store: &ManagedStore,
    agent_ref: &str,
    version_num: u32,
) -> anyhow::Result<()> {
    let agent = resolve_agent_ref(store, agent_ref).await?;
    let version = store
        .get_agent_version(&agent.id, version_num)
        .await?
        .with_context(|| {
            format!(
                "Managed agent version not found: {}@{}",
                agent.name, version_num
            )
        })?;

    println!("Managed agent: {} ({})", agent.name, agent.id);
    print_version_summary(&version);
    Ok(())
}

struct CreateVersionArgs {
    agent: String,
    model: Option<String>,
    system_prompt: String,
    base_url: Option<String>,
    allowed_tools: Vec<String>,
    allowed_skills: Vec<String>,
    max_iterations: u32,
    temperature: f64,
    approval: ManagedApprovalPolicy,
    timeout_secs: u32,
}

async fn create_version(
    store: &ManagedStore,
    args: CreateVersionArgs,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    let agent = resolve_agent_ref(store, &args.agent).await?;
    if agent.archived {
        bail!("Managed agent is archived: {}", agent.name);
    }

    let system_prompt = args.system_prompt.trim();
    if system_prompt.is_empty() {
        bail!("Managed agent version system_prompt is required");
    }
    if args.max_iterations == 0 {
        bail!("Managed agent version max_iterations must be greater than 0");
    }
    if args.timeout_secs == 0 {
        bail!("Managed agent version timeout_secs must be greater than 0");
    }
    if !args.temperature.is_finite() {
        bail!("Managed agent version temperature must be finite");
    }

    validate_managed_beta_tools(&args.allowed_tools).map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_allowed_skills(&args.allowed_skills)?;

    let resolved = resolve_managed_version_defaults(
        args.model.as_deref(),
        args.base_url.as_deref(),
        app_config,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut draft = ManagedAgentVersionDraft::new(&resolved.model, system_prompt);
    draft.base_url = resolved.base_url.clone();
    draft.allowed_tools = args.allowed_tools;
    draft.allowed_skills = args.allowed_skills;
    draft.max_iterations = args.max_iterations;
    draft.temperature = args.temperature;
    draft.approval_policy = args.approval;
    draft.timeout_secs = args.timeout_secs;

    report_preflight_outcome(
        preflight_managed_model(app_config, &draft.model, draft.base_url.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        &draft.model,
    );

    let version = store.create_next_agent_version(&agent.id, &draft).await?;

    println!("Published managed agent version:");
    println!("Agent: {} ({})", agent.name, agent.id);
    print_version_summary(&version);
    Ok(())
}

async fn build_sync_plan(
    store: &ManagedStore,
    path: &Path,
    app_config: &AppConfig,
) -> anyhow::Result<AgentSyncPlan> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read managed agent YAML {}", path.display()))?;
    let metadata_hash = extract_sync_metadata_sha256(&contents);
    let spec = ManagedAgentYaml::parse_str_with_defaults(&contents, app_config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_allowed_skills(&spec.allowed_skills)?;

    let existing_agent = store.get_agent_by_name(&spec.name).await?;
    if existing_agent.as_ref().is_some_and(|agent| agent.archived) {
        bail!("Managed agent is archived: {}", spec.name);
    }

    let latest_version = match existing_agent.as_ref() {
        Some(agent) => load_latest_version(store, agent).await?,
        None => None,
    };

    let desired_hash = spec
        .canonical_sha256()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let current_hash = latest_version
        .as_ref()
        .map(|version| ManagedAgentYaml::from_agent_version(&spec.name, version))
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .map(|yaml| yaml.canonical_sha256())
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let diff = spec
        .diff_against_version(latest_version.as_ref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let action = if existing_agent.is_none() {
        SyncAction::CreateAgentAndVersion
    } else if latest_version.is_none() {
        SyncAction::PublishVersion
    } else if current_hash.as_deref() == Some(desired_hash.as_str())
        && metadata_hash.as_deref() != Some(desired_hash.as_str())
    {
        SyncAction::WriteMetadata
    } else if current_hash.as_deref() == Some(desired_hash.as_str()) {
        SyncAction::Noop
    } else {
        SyncAction::PublishVersion
    };

    Ok(AgentSyncPlan {
        source_path: path.to_path_buf(),
        spec,
        existing_agent,
        latest_version,
        desired_hash,
        current_hash,
        metadata_hash,
        diff,
        action,
    })
}

async fn apply_sync_plan(
    store: &ManagedStore,
    plan: &AgentSyncPlan,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    match plan.action {
        SyncAction::Noop => {
            println!("No changes for {}.", plan.spec.name);
            Ok(())
        }
        SyncAction::CreateAgentAndVersion => {
            let agent = ManagedAgent::new(plan.spec.name.clone());
            store.create_agent(&agent).await?;
            let draft = plan.spec.to_draft();
            report_preflight_outcome(
                preflight_managed_model(app_config, &draft.model, draft.base_url.as_deref())
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
                &draft.model,
            );
            let version = store.create_next_agent_version(&agent.id, &draft).await?;
            write_sync_metadata(&plan.source_path, &plan.spec)?;
            println!(
                "Created managed agent {} and published version {}.",
                agent.name, version.version
            );
            Ok(())
        }
        SyncAction::PublishVersion => {
            let agent = plan
                .existing_agent
                .as_ref()
                .context("sync plan missing existing agent")?;
            let draft = plan.spec.to_draft();
            report_preflight_outcome(
                preflight_managed_model(app_config, &draft.model, draft.base_url.as_deref())
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
                &draft.model,
            );
            let version = store.create_next_agent_version(&agent.id, &draft).await?;
            write_sync_metadata(&plan.source_path, &plan.spec)?;
            println!(
                "Published managed agent {} version {}.",
                agent.name, version.version
            );
            Ok(())
        }
        SyncAction::WriteMetadata => {
            write_sync_metadata(&plan.source_path, &plan.spec)?;
            println!(
                "Updated managed agent YAML metadata for {}.",
                plan.spec.name
            );
            Ok(())
        }
    }
}

fn write_sync_metadata(path: &Path, spec: &ManagedAgentYaml) -> anyhow::Result<()> {
    let rendered = spec
        .render_with_sync_metadata()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(path, rendered)
        .with_context(|| format!("failed to write managed agent YAML {}", path.display()))?;
    Ok(())
}

fn print_sync_plan(plan: &AgentSyncPlan) {
    println!("Source: {}", plan.source_path.display());
    println!("Agent:  {}", plan.spec.name);
    println!("Action: {}", plan.action.as_str());
    println!("Desired sha256: {}", plan.desired_hash);
    println!(
        "Stored sha256:  {}",
        plan.current_hash.as_deref().unwrap_or("-")
    );
    println!(
        "YAML metadata:  {}",
        plan.metadata_hash.as_deref().unwrap_or("-")
    );
    if let Some(agent) = &plan.existing_agent {
        println!("Current agent id: {}", agent.id);
    }
    if let Some(version) = &plan.latest_version {
        println!("Current latest version: {}", version.version);
    }

    if plan.diff.is_empty() {
        println!("Config drift: none");
        if plan.action == SyncAction::WriteMetadata {
            println!("Metadata drift: missing or stale hermes-synced sha256");
        }
        return;
    }

    let changed_fields = plan
        .diff
        .changes
        .iter()
        .map(|change| change.field)
        .collect::<Vec<_>>()
        .join(", ");
    println!("Config drift: {changed_fields}");
    println!("Changes:");
    for change in &plan.diff.changes {
        print_yaml_field_diff(change);
    }
}

fn print_yaml_field_diff(change: &ManagedAgentYamlFieldDiff) {
    let current = change.current.as_deref().unwrap_or("(missing)");
    let desired = change.desired.as_deref().unwrap_or("(missing)");
    if is_inline_diff_value(current) && is_inline_diff_value(desired) {
        println!("  {}: {} -> {}", change.field, current, desired);
        return;
    }

    println!("  {}:", change.field);
    println!("    current:");
    println!("{}", indent_block(current, "      "));
    println!("    desired:");
    println!("{}", indent_block(desired, "      "));
}

fn confirm_sync_apply(change_count: usize) -> anyhow::Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("sync requires a terminal for confirmation; rerun with --yes or --dry-run");
    }

    print!("Apply {change_count} managed agent change(s)? [y/N] ");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

async fn resolve_agent_ref(store: &ManagedStore, agent_ref: &str) -> anyhow::Result<ManagedAgent> {
    if let Some(agent) = store.get_agent(agent_ref).await? {
        return Ok(agent);
    }
    if let Some(agent) = store.get_agent_by_name(agent_ref).await? {
        return Ok(agent);
    }

    bail!("Managed agent not found: {agent_ref}")
}

async fn load_latest_version(
    store: &ManagedStore,
    agent: &ManagedAgent,
) -> anyhow::Result<Option<ManagedAgentVersion>> {
    if agent.latest_version == 0 {
        return Ok(None);
    }

    store
        .get_agent_version(&agent.id, agent.latest_version)
        .await?
        .with_context(|| {
            format!(
                "latest managed agent version missing: {}@{}",
                agent.name, agent.latest_version
            )
        })
        .map(Some)
}

fn collect_sync_paths(inputs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    if inputs.is_empty() {
        let default_dir = default_agents_dir();
        let paths = discover_yaml_files(&default_dir)?;
        if paths.is_empty() {
            bail!(
                "No managed agent YAML files found in {}",
                default_dir.display()
            );
        }
        return Ok(paths);
    }

    let mut paths = BTreeSet::new();
    for input in inputs {
        for path in resolve_yaml_paths(input)? {
            paths.insert(path);
        }
    }

    Ok(paths.into_iter().collect())
}

fn validate_unique_sync_targets(plans: &[AgentSyncPlan]) -> anyhow::Result<()> {
    let mut by_name = BTreeMap::<&str, Vec<&Path>>::new();
    for plan in plans {
        by_name
            .entry(&plan.spec.name)
            .or_default()
            .push(plan.source_path.as_path());
    }

    let duplicates = by_name
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(name, paths)| {
            format!(
                "{name}: {}",
                paths
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>();

    if duplicates.is_empty() {
        return Ok(());
    }

    bail!(
        "Duplicate managed agent names across sync inputs:\n{}",
        duplicates.join("\n")
    )
}

fn resolve_yaml_input(input: &Path) -> anyhow::Result<PathBuf> {
    let paths = resolve_yaml_paths(input)?;
    if paths.len() != 1 {
        bail!(
            "expected one managed agent YAML file, found {} entries for {}",
            paths.len(),
            input.display()
        );
    }
    Ok(paths.into_iter().next().unwrap_or_default())
}

fn resolve_yaml_paths(input: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if input.exists() {
        if input.is_dir() {
            let paths = discover_yaml_files(input)?;
            if paths.is_empty() {
                bail!("No managed agent YAML files found in {}", input.display());
            }
            return Ok(paths);
        }
        if !is_yaml_file(input) {
            bail!(
                "Managed agent YAML file must end in .yaml or .yml: {}",
                input.display()
            );
        }
        return Ok(vec![input.to_path_buf()]);
    }

    if let Some(path) = resolve_default_named_yaml(input) {
        return Ok(vec![path]);
    }

    bail!("Managed agent YAML not found: {}", input.display())
}

fn discover_yaml_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read managed agent directory {}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_yaml_file(path))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn resolve_default_named_yaml(input: &Path) -> Option<PathBuf> {
    let name = input.to_str()?;
    if name.contains(std::path::MAIN_SEPARATOR) || name.is_empty() {
        return None;
    }

    let dir = default_agents_dir();
    let yaml = dir.join(format!("{name}.yaml"));
    if yaml.exists() {
        return Some(yaml);
    }

    let yml = dir.join(format!("{name}.yml"));
    if yml.exists() {
        return Some(yml);
    }

    None
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "yaml" | "yml"))
}

fn default_agents_dir() -> PathBuf {
    hermes_home().join("agents")
}

fn validate_allowed_skills(allowed_skills: &[String]) -> anyhow::Result<()> {
    if allowed_skills.is_empty() {
        return Ok(());
    }

    let skills_dir = hermes_home().join("skills");
    let manager = SkillManager::new(vec![skills_dir])
        .context("failed to initialize skills for validation")?;
    build_filtered_skill_manager(&manager, allowed_skills).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn report_preflight_outcome(outcome: ManagedModelPreflight, model: &str) {
    if let ManagedModelPreflight::Skipped(reason) = outcome {
        eprintln!("Managed model preflight skipped for {model}: {reason}");
    }
}

fn print_agent_summary(agent: &ManagedAgent) {
    println!("ID:             {}", agent.id);
    println!("Name:           {}", agent.name);
    println!("Latest version: {}", agent.latest_version);
    println!(
        "Archived:       {}",
        if agent.archived { "yes" } else { "no" }
    );
    println!("Created:        {}", format_ts(agent.created_at));
    println!("Updated:        {}", format_ts(agent.updated_at));
}

fn print_version_summary(version: &ManagedAgentVersion) {
    println!("Version:        {}", version.version);
    println!("Model:          {}", version.model);
    println!(
        "Base URL:       {}",
        version.base_url.as_deref().unwrap_or("-")
    );
    println!("Approval:       {}", version.approval_policy.as_str());
    println!("Max iterations: {}", version.max_iterations);
    println!("Temperature:    {}", version.temperature);
    println!("Timeout:        {}s", version.timeout_secs);
    println!(
        "Allowed tools:  {}",
        if version.allowed_tools.is_empty() {
            "-".to_string()
        } else {
            version.allowed_tools.join(", ")
        }
    );
    println!(
        "Allowed skills: {}",
        if version.allowed_skills.is_empty() {
            "-".to_string()
        } else {
            version.allowed_skills.join(", ")
        }
    );
    println!("Created:        {}", format_ts(version.created_at));
    println!("System prompt:");
    println!("{}", indent_block(&version.system_prompt, "  "));
}

fn format_ts(value: chrono::DateTime<chrono::Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn indent_block(value: &str, prefix: &str) -> String {
    value
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_inline_diff_value(value: &str) -> bool {
    !value.contains('\n') && value.chars().count() <= 80
}

fn truncate(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }

    let visible = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(visible).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    async fn temp_store() -> (TempDir, ManagedStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    fn write_agent_yaml(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn resolve_agent_ref_supports_id_and_name() {
        let (_dir, store) = temp_store().await;
        let agent = ManagedAgent::new("reviewer");
        store.create_agent(&agent).await.unwrap();

        let by_id = resolve_agent_ref(&store, &agent.id).await.unwrap();
        let by_name = resolve_agent_ref(&store, "reviewer").await.unwrap();

        assert_eq!(by_id.id, agent.id);
        assert_eq!(by_name.id, agent.id);
    }

    #[tokio::test]
    async fn build_sync_plan_is_noop_when_yaml_matches_latest_version() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig::default();
        let agent = ManagedAgent::new("reviewer");
        store.create_agent(&agent).await.unwrap();

        let mut draft = ManagedAgentVersionDraft::new("openai/gpt-4o-mini", "review carefully");
        draft.allowed_tools = vec!["read_file".to_string(), "search_files".to_string()];
        store
            .create_next_agent_version(&agent.id, &draft)
            .await
            .unwrap();

        let path = write_agent_yaml(
            dir.path(),
            "reviewer",
            r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: review carefully
allowed_tools: [search_files, read_file]
"#,
        );

        let plan = build_sync_plan(&store, &path, &app_config).await.unwrap();
        assert_eq!(plan.action, SyncAction::WriteMetadata);
        assert!(plan.diff.is_empty());
    }

    #[tokio::test]
    async fn build_sync_plan_creates_agent_when_missing() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig::default();
        let path = write_agent_yaml(
            dir.path(),
            "new-agent",
            r#"
name: new-agent
model: openai/gpt-4o-mini
system_prompt: review carefully
"#,
        );

        let plan = build_sync_plan(&store, &path, &app_config).await.unwrap();
        assert_eq!(plan.action, SyncAction::CreateAgentAndVersion);
        assert!(plan.existing_agent.is_none());
        assert!(plan.latest_version.is_none());
        assert!(!plan.diff.is_empty());
    }

    #[tokio::test]
    async fn build_sync_plan_is_noop_when_yaml_metadata_matches_latest_version() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig::default();
        let agent = ManagedAgent::new("reviewer");
        store.create_agent(&agent).await.unwrap();

        let mut draft = ManagedAgentVersionDraft::new("openai/gpt-4o-mini", "review carefully");
        draft.allowed_tools = vec!["read_file".to_string(), "search_files".to_string()];
        let version = store
            .create_next_agent_version(&agent.id, &draft)
            .await
            .unwrap();

        let spec = ManagedAgentYaml::from_agent_version("reviewer", &version).unwrap();
        let path = dir.path().join("reviewer.yaml");
        std::fs::write(&path, spec.render_with_sync_metadata().unwrap()).unwrap();

        let plan = build_sync_plan(&store, &path, &app_config).await.unwrap();
        assert_eq!(plan.action, SyncAction::Noop);
        assert_eq!(
            plan.metadata_hash.as_deref(),
            Some(plan.desired_hash.as_str())
        );
        assert!(plan.diff.is_empty());
    }

    #[tokio::test]
    async fn apply_sync_plan_metadata_write_rewrites_yaml_without_publishing() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig::default();
        let agent = ManagedAgent::new("reviewer");
        store.create_agent(&agent).await.unwrap();

        let mut draft = ManagedAgentVersionDraft::new("openai/gpt-4o-mini", "review carefully");
        draft.allowed_tools = vec!["read_file".to_string()];
        store
            .create_next_agent_version(&agent.id, &draft)
            .await
            .unwrap();

        let path = write_agent_yaml(
            dir.path(),
            "reviewer",
            r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: review carefully
allowed_tools: [read_file]
"#,
        );

        let plan = build_sync_plan(&store, &path, &app_config).await.unwrap();
        assert_eq!(plan.action, SyncAction::WriteMetadata);

        apply_sync_plan(&store, &plan, &app_config).await.unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let metadata_hash = extract_sync_metadata_sha256(&contents).unwrap();
        let spec = ManagedAgentYaml::parse_str(&contents).unwrap();
        assert_eq!(metadata_hash, spec.canonical_sha256().unwrap());

        let agent = resolve_agent_ref(&store, "reviewer").await.unwrap();
        assert_eq!(agent.latest_version, 1);
    }

    #[tokio::test]
    async fn sync_rejects_duplicate_agent_names_across_files() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig::default();
        write_agent_yaml(
            dir.path(),
            "reviewer-a",
            r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: first
"#,
        );
        write_agent_yaml(
            dir.path(),
            "reviewer-b",
            r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: second
"#,
        );

        let err = sync_agent_yamls(
            &store,
            &[dir.path().to_path_buf()],
            true,
            false,
            &app_config,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Duplicate managed agent names across sync inputs")
        );
    }

    #[tokio::test]
    async fn build_sync_plan_resolves_missing_model_from_app_config() {
        let (dir, store) = temp_store().await;
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some("https://models.example/v1".to_string()),
            ..AppConfig::default()
        };
        let path = write_agent_yaml(
            dir.path(),
            "new-agent",
            r#"
name: new-agent
system_prompt: review carefully
"#,
        );

        let plan = build_sync_plan(&store, &path, &app_config).await.unwrap();
        assert_eq!(plan.spec.model, "openai/gpt-4o-mini");
        assert_eq!(
            plan.spec.base_url.as_deref(),
            Some("https://models.example/v1")
        );
    }

    #[test]
    fn approval_arg_maps_to_managed_policy() {
        assert_eq!(
            ManagedApprovalPolicy::from(ApprovalArg::Ask),
            ManagedApprovalPolicy::Ask
        );
        assert_eq!(
            ManagedApprovalPolicy::from(ApprovalArg::Yolo),
            ManagedApprovalPolicy::Yolo
        );
        assert_eq!(
            ManagedApprovalPolicy::from(ApprovalArg::Deny),
            ManagedApprovalPolicy::Deny
        );
    }
}
