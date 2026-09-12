//! Top-level command orchestration that is independent of daemon transport.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use thiserror::Error;

use crate::cli::{
    ClearArgs, Cli, Command, ConfigAction, DoctorArgs, HistoryAction, HistoryArgs, HookAction,
    HookArgs, HookEventArgs, HookMutationArgs, HookScopeArg, HookSelectionArgs, ProviderArg,
    RendererArgs, Selector, TmuxTarget,
};
use crate::config::{
    Config, ConfigOverrides, FeatureMode, HooksConfig, TimeoutValue, load, to_stable_toml,
};
use crate::daemon::application::{HistoryActionClient, request_config_reload};
use crate::daemon::runtime::{RuntimeError, ServerIdentity, submit_lazy};
use crate::doctor::{DoctorOutputError, HookObservation, SystemProbe};
use crate::history::{ClearFilter, History, HistoryError, HistoryQuery, write_ndjson, write_plain};
use crate::hooks::{HookError, HookManager, HookSubmitter, Scope, receive_hook_event};
use crate::notification::{Provider, SourceContext, Timeout, TmuxServerId};
use crate::platform::{Environment, PathError, PlatformPaths, PrivateLogger};
use crate::protocol::{
    ClientCommand, ClientRequest, ClientResponse, ProtocolError, WireNotificationDraft,
    WireNotificationUpdate, WireSelector, decode_client_response,
};
use crate::providers::HookPolicy;
use crate::renderer_runtime::{RendererKind, RendererRuntimeError, run_hidden_renderer};
use crate::tmux::Server;
use crate::ui::history::{
    HistoryActions, HistoryLaunch, HistoryOutcome, OutsideTmuxScope,
    run_with_options as run_history_terminal,
};

/// App-facing dispatch target for both hidden renderer subcommands. Keeping it
/// here lets the one-binary entry point route modes without learning socket or
/// terminal mechanics.
pub fn run_renderer_command(kind: RendererKind, arguments: RendererArgs) -> Result<(), AppError> {
    run_hidden_renderer(kind, &arguments.window_display, &arguments.token)?;
    Ok(())
}

/// Hook mutations are silent on success. Status is a command result and is
/// written to stdout by the caller-provided writer.
pub fn run_hook_command(
    manager: &HookManager,
    config: &HooksConfig,
    arguments: HookArgs,
    mut output: impl Write,
) -> Result<(), AppError> {
    match arguments.action {
        HookAction::Install(arguments) => {
            let (provider, scope, allow_mixed) = mutation_parts(arguments);
            manager.install(
                provider,
                scope,
                provider_config(config, provider),
                allow_mixed,
            )?;
        }
        HookAction::Remove(arguments) => {
            let (provider, scope, _) = mutation_parts(arguments);
            manager.remove(provider, scope)?;
        }
        HookAction::Sync(arguments) => {
            for provider in selected_providers(arguments.provider) {
                manager.sync(
                    provider,
                    provider_config(config, provider),
                    arguments.allow_mixed,
                )?;
            }
        }
        HookAction::Status(arguments) => {
            write_hook_status(manager, config, arguments, &mut output)?;
        }
    }
    Ok(())
}

fn mutation_parts(arguments: HookMutationArgs) -> (Provider, Scope, bool) {
    (
        arguments.provider.into(),
        arguments.scope.unwrap_or(HookScopeArg::User).into(),
        arguments.allow_mixed,
    )
}

fn write_hook_status(
    manager: &HookManager,
    config: &HooksConfig,
    arguments: HookSelectionArgs,
    mut output: impl Write,
) -> Result<(), AppError> {
    for provider in selected_providers(arguments.provider) {
        for status in manager.status_with_config(provider, provider_config(config, provider))? {
            let state = if !status.installed {
                "not-installed"
            } else if status.in_sync {
                "in-sync"
            } else {
                "out-of-sync"
            };
            writeln!(
                output,
                "{}\t{}\t{}\ttrust: {}\t{}",
                provider_name(provider),
                scope_name(status.scope),
                state,
                status.trust,
                status.path.display()
            )?;
        }
    }
    Ok(())
}

fn selected_providers(provider: Option<ProviderArg>) -> Vec<Provider> {
    provider.map_or_else(
        || vec![Provider::Claude, Provider::Codex],
        |provider| vec![provider.into()],
    )
}

fn provider_config(config: &HooksConfig, provider: Provider) -> &crate::config::HookProviderConfig {
    match provider {
        Provider::Claude => &config.claude,
        Provider::Codex => &config.codex,
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
        Scope::Local => "local",
    }
}

pub fn run_doctor_command(
    probe: &SystemProbe<'_>,
    arguments: DoctorArgs,
    mut output: impl Write,
    unicode: bool,
) -> Result<bool, AppError> {
    let report = probe.run();
    if arguments.json {
        report.write_json(&mut output)?;
    } else {
        report.write_human(&mut output, unicode)?;
    }
    Ok(report.is_healthy())
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Hook(#[from] HookError),
    #[error(transparent)]
    DoctorOutput(#[from] DoctorOutputError),
    #[error("failed to write command output: {0}")]
    Output(#[from] io::Error),
    #[error(transparent)]
    Renderer(#[from] RendererRuntimeError),
}

/// Runs the complete public/hidden command dispatcher. Successful direct
/// mutations remain silent unless their command requested JSON.
pub async fn run(cli: Cli) -> Result<(), RuntimeAppError> {
    if matches!(cli.command, Command::HookEvent(_)) {
        run_hook_event_silent(cli).await;
        return Ok(());
    }

    let environment = Environment::current();
    let paths = PlatformPaths::resolve(&environment)?;
    if matches!(
        &cli.command,
        Command::Config(crate::cli::ConfigArgs {
            action: ConfigAction::Show
        })
    ) {
        let config = load(&paths.config_file, &environment, ConfigOverrides::default())?;
        write!(io::stdout().lock(), "{}", to_stable_toml(&config)?)?;
        return Ok(());
    }
    let tmux_environment = environment.get("TMUX").and_then(|value| value.to_str());
    let inside_tmux = tmux_environment.is_some();
    let target = cli.tmux_target(tmux_environment)?;
    let server = target.map(resolve_server).transpose()?;
    let selected = server
        .map(|server| SelectedServer::new(server, &paths))
        .transpose()?;
    if let Command::Config(crate::cli::ConfigArgs {
        action: ConfigAction::Reload(arguments),
    }) = &cli.command
    {
        let selected = require_server(selected)?;
        let result = request_config_reload(&selected.daemon_socket).await?;
        if arguments.json {
            serde_json::to_writer(io::stdout().lock(), &result)?;
            println!();
        } else {
            let changed = result
                .get("changed")
                .and_then(Value::as_bool)
                .ok_or_else(|| RuntimeAppError::Remote("invalid reload acknowledgement".into()))?;
            println!("{}", if changed { "changed" } else { "unchanged" });
        }
        return Ok(());
    }
    let config = load(&paths.config_file, &environment, ConfigOverrides::default())?;

    match cli.command {
        Command::Send(arguments) => {
            let selected = require_server(selected)?;
            let source = if arguments.no_source {
                None
            } else {
                capture_source(&selected, &environment)?
            };
            let json = arguments.json;
            let use_config_timeout = arguments.timeout.is_none();
            let mut draft = arguments.into_draft(source, io::stdin().lock())?;
            if use_config_timeout
                && draft.presentation() == crate::notification::Presentation::Toast
            {
                draft = draft.with_timeout(config_timeout(config.toast.timeout));
            }
            submit_and_present(
                &selected,
                ClientCommand::Send {
                    notification: Box::new(WireNotificationDraft::from_domain(&draft)),
                },
                json,
            )
            .await
        }
        Command::Update(arguments) => {
            let selected = require_server(selected)?;
            let json = arguments.json;
            let selector = wire_selector(&arguments.selector);
            let update = arguments.into_update(io::stdin().lock())?;
            submit_and_present(
                &selected,
                ClientCommand::Update {
                    selector,
                    update: WireNotificationUpdate::from_domain(&update),
                },
                json,
            )
            .await
        }
        Command::Dismiss(selector) => {
            let selected = require_server(selected)?;
            submit_and_present(
                &selected,
                ClientCommand::Dismiss {
                    selector: wire_selector(&selector),
                },
                false,
            )
            .await
        }
        Command::Jump(arguments) => {
            let selected = require_server(selected)?;
            submit_and_present(
                &selected,
                ClientCommand::Jump { key: arguments.key },
                arguments.json,
            )
            .await
        }
        Command::History(arguments) => {
            match history_mode(&arguments, io::stdout().is_terminal(), inside_tmux) {
                HistoryMode::Floating => {
                    launch_history_pane(arguments, require_server(selected)?, &paths, &environment)
                        .await?;
                    Ok(())
                }
                HistoryMode::Terminal => {
                    run_interactive_history(arguments, selected, &paths, &config)
                }
                HistoryMode::Output => {
                    run_history(arguments, selected.as_ref(), &paths, &config).await
                }
            }
        }
        Command::Config(_) => unreachable!("configuration commands returned before dispatch"),
        Command::Hook(arguments) => {
            let manager = production_hook_manager(&environment)?;
            run_hook_command(&manager, &config.hooks, arguments, io::stdout().lock())?;
            Ok(())
        }
        Command::Doctor(arguments) => {
            run_production_doctor(arguments, selected.as_ref(), &paths, &environment, &config)
        }
        Command::Daemon => {
            let selected = require_server(selected)?;
            crate::daemon::application::run(selected.server, paths, config).await?;
            Ok(())
        }
        Command::HistoryUi(arguments) => run_interactive_history(
            HistoryArgs {
                action: None,
                plain: false,
                json: false,
                all: arguments.include_hidden,
                all_servers: arguments.all_servers,
            },
            selected,
            &paths,
            &config,
        ),
        Command::RenderToast(arguments) => {
            run_renderer_command(RendererKind::Toast, arguments)?;
            Ok(())
        }
        Command::RenderAttention(arguments) => {
            run_renderer_command(RendererKind::Attention, arguments)?;
            Ok(())
        }
        Command::HookEvent(_) => unreachable!("hook mode returned before regular dispatch"),
    }
}

#[derive(Clone)]
struct SelectedServer {
    server: Server,
    identity: ServerIdentity,
    daemon_socket: PathBuf,
    domain_id: TmuxServerId,
}

impl SelectedServer {
    fn new(server: Server, paths: &PlatformPaths) -> Result<Self, RuntimeAppError> {
        let identity = ServerIdentity::resolve(server.socket_path())?;
        let daemon_socket = paths.socket_path(identity.server_id())?;
        let domain_id = TmuxServerId::new(identity.server_id())?;
        Ok(Self {
            server: Server::new(identity.tmux_socket()),
            identity,
            daemon_socket,
            domain_id,
        })
    }
}

fn resolve_server(target: TmuxTarget) -> Result<Server, RuntimeAppError> {
    Ok(match target {
        TmuxTarget::SocketName(name) => Server::from_socket_name(&name)?,
        TmuxTarget::SocketPath(path) => Server::new(path),
    })
}

fn require_server(server: Option<SelectedServer>) -> Result<SelectedServer, RuntimeAppError> {
    server.ok_or(RuntimeAppError::MissingTmuxTarget)
}

fn capture_source(
    selected: &SelectedServer,
    environment: &Environment,
) -> Result<Option<SourceContext>, RuntimeAppError> {
    let Some(tmux) = environment.get("TMUX").and_then(|value| value.to_str()) else {
        return Ok(None);
    };
    let current_socket = tmux
        .split_once(',')
        .map(|(socket, _)| Path::new(socket))
        .filter(|socket| !socket.as_os_str().is_empty());
    let same_server = current_socket
        .and_then(|socket| std::fs::canonicalize(socket).ok())
        .is_some_and(|socket| socket == selected.identity.tmux_socket());
    if !same_server {
        return Ok(None);
    }
    let Some(pane) = environment
        .get("TMUX_PANE")
        .and_then(|value| value.to_str())
    else {
        return Ok(None);
    };
    selected
        .server
        .source_context(selected.domain_id.clone(), pane)
        .map(Some)
        .map_err(|error| RuntimeAppError::Execution(error.to_string()))
}

fn config_timeout(value: TimeoutValue) -> Timeout {
    match value {
        TimeoutValue::Never => Timeout::Never,
        TimeoutValue::After(duration) => Timeout::After(duration.0),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryMode {
    Output,
    Floating,
    Terminal,
}

fn history_mode(arguments: &HistoryArgs, stdout_terminal: bool, inside_tmux: bool) -> HistoryMode {
    if arguments.action.is_some() || arguments.plain || arguments.json || !stdout_terminal {
        HistoryMode::Output
    } else if inside_tmux {
        HistoryMode::Floating
    } else {
        HistoryMode::Terminal
    }
}

async fn launch_history_pane(
    arguments: HistoryArgs,
    selected: SelectedServer,
    paths: &PlatformPaths,
    environment: &Environment,
) -> Result<(), RuntimeAppError> {
    HistoryActionClient::new(
        paths.clone(),
        Some((selected.domain_id.clone(), selected.server.clone())),
    )?
    .ensure_open_allowed()
    .await?;
    let source =
        capture_source(&selected, environment)?.ok_or(RuntimeAppError::HistorySourceUnavailable)?;
    let executable = std::env::current_exe()?;
    selected.server.launch_history_ui(
        source.window_id(),
        &executable,
        arguments.all_servers,
        arguments.all,
    )?;
    Ok(())
}

fn run_interactive_history(
    arguments: HistoryArgs,
    selected: Option<SelectedServer>,
    paths: &PlatformPaths,
    config: &Config,
) -> Result<(), RuntimeAppError> {
    let current_server = selected.as_ref().map(|server| server.domain_id.clone());
    let history_server = current_server
        .clone()
        .unwrap_or_else(|| TmuxServerId::new("all-servers").expect("fixed ID is valid"));
    let history = Arc::new(History::open_reader(
        &paths.history_file,
        history_server,
        &config.history,
    )?);
    let launch = match current_server {
        Some(current_server) if std::env::var_os("TMUX").is_some() => {
            HistoryLaunch::InsideTmux { current_server }
        }
        Some(current_server) => {
            HistoryLaunch::OutsideTmux(OutsideTmuxScope::ExplicitServer(current_server))
        }
        None => HistoryLaunch::OutsideTmux(OutsideTmuxScope::AllServers),
    };

    let daemon_actions = HistoryActionClient::new(
        paths.clone(),
        selected
            .as_ref()
            .map(|server| (server.domain_id.clone(), server.server.clone())),
    )?;
    let guard_actions = daemon_actions.clone();
    let runtime = tokio::runtime::Handle::current();
    let guard_runtime = runtime.clone();
    let actions = HistoryActions::new(
        history,
        move || {
            guard_runtime
                .block_on(guard_actions.ensure_open_allowed())
                .map_err(|error| error.to_string())
        },
        |_source: &SourceContext| Ok(()),
        move |source: &SourceContext| {
            runtime
                .block_on(daemon_actions.jump(source))
                .map_err(|error| error.to_string())
        },
    );
    match run_history_terminal(launch, arguments.all, actions)? {
        HistoryOutcome::Jumped {
            warning: Some(warning),
        } => eprintln!("warning: {warning}"),
        HistoryOutcome::Closed | HistoryOutcome::Interrupted | HistoryOutcome::Jumped { .. } => {}
    }
    Ok(())
}

fn wire_selector(selector: &Selector) -> WireSelector {
    WireSelector {
        id: selector.id.clone(),
        key: selector.key.clone(),
    }
}

async fn submit_and_present(
    selected: &SelectedServer,
    command: ClientCommand,
    json: bool,
) -> Result<(), RuntimeAppError> {
    let request = ClientRequest::new(command);
    // submit_lazy supplies exactly one delimiter. Encoding a line here would
    // create an empty second frame on the daemon connection.
    let payload = serde_json::to_vec(&request)?;
    let frame = submit_lazy(
        &selected.daemon_socket,
        selected.identity.tmux_socket(),
        &payload,
    )
    .await?;
    let result = match decode_client_response(&frame, request.request_id)? {
        ClientResponse::Success(value) => value,
        ClientResponse::Error(message) => return Err(RuntimeAppError::Remote(message)),
    };
    if result
        .get("history_persisted")
        .is_some_and(|value| value == &Value::Bool(false))
    {
        eprintln!("warning: Notification was accepted but History could not be persisted");
    }
    if json {
        serde_json::to_writer(io::stdout().lock(), &result)?;
        println!();
    }
    Ok(())
}

async fn run_history(
    arguments: HistoryArgs,
    selected: Option<&SelectedServer>,
    paths: &PlatformPaths,
    config: &Config,
) -> Result<(), RuntimeAppError> {
    let clear_all_servers = matches!(
        arguments.action,
        Some(HistoryAction::Clear(ClearArgs {
            all_servers: true,
            ..
        }))
    );
    if selected.is_none() && !arguments.all_servers && !clear_all_servers {
        return Err(RuntimeAppError::MissingHistoryTarget);
    }
    let server_id = selected
        .map(|server| server.domain_id.clone())
        .unwrap_or_else(|| TmuxServerId::new("all-servers").expect("fixed ID is valid"));
    let history = History::open_reader(&paths.history_file, server_id, &config.history)?;
    if let Some(HistoryAction::Clear(mut clear)) = arguments.action {
        clear.all_servers |= arguments.all_servers;
        return clear_history(&history, clear).await;
    }
    let entries = history
        .list(HistoryQuery {
            include_hidden: arguments.all,
            all_servers: arguments.all_servers,
            limit: config.history.max_entries,
        })?
        .wait()
        .await?;
    if arguments.json {
        write_ndjson(&entries, io::stdout().lock())?;
    } else {
        let width = if io::stdout().is_terminal() {
            crossterm::terminal::size().map_or(120, |(width, _)| usize::from(width))
        } else {
            120
        };
        write_plain(&entries, width, io::stdout().lock())?;
    }
    Ok(())
}

async fn clear_history(history: &History, arguments: ClearArgs) -> Result<(), RuntimeAppError> {
    let filter = clear_filter(&arguments)?;
    let count = history
        .count_clear(filter, arguments.all_servers)?
        .wait()
        .await?;
    if !arguments.yes {
        if !io::stdin().is_terminal() {
            return Err(RuntimeAppError::ConfirmationRequired);
        }
        eprint!(
            "Delete {count} History row(s) from {}? [y/N] ",
            if arguments.all_servers {
                "all servers"
            } else {
                "the selected server"
            }
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Ok(());
        }
    }
    let deleted = history.clear(filter, arguments.all_servers)?.wait().await?;
    println!("deleted {deleted}");
    Ok(())
}

fn clear_filter(arguments: &ClearArgs) -> Result<ClearFilter, RuntimeAppError> {
    if arguments.hidden {
        return Ok(ClearFilter::Hidden);
    }
    if arguments.all {
        return Ok(ClearFilter::All);
    }
    let value = arguments
        .before
        .as_deref()
        .ok_or_else(|| RuntimeAppError::Execution("History Clear requires a selector".into()))?;
    let duration = parse_age(value)?;
    let before = Utc::now()
        - chrono::Duration::from_std(duration)
            .map_err(|_| RuntimeAppError::InvalidAge(value.to_owned()))?;
    Ok(ClearFilter::Before(before))
}

fn parse_age(value: &str) -> Result<Duration, RuntimeAppError> {
    let (number, seconds) = if let Some(number) = value.strip_suffix('d') {
        (number, 86_400_u64)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60)
    } else {
        return Err(RuntimeAppError::InvalidAge(value.to_owned()));
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| RuntimeAppError::InvalidAge(value.to_owned()))?;
    number
        .checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| RuntimeAppError::InvalidAge(value.to_owned()))
}

fn production_hook_manager(environment: &Environment) -> Result<HookManager, RuntimeAppError> {
    HookManager::new(
        std::env::current_exe()?,
        std::env::current_dir()?,
        environment,
    )
    .map_err(RuntimeAppError::Hook)
}

fn run_production_doctor(
    arguments: DoctorArgs,
    selected: Option<&SelectedServer>,
    paths: &PlatformPaths,
    environment: &Environment,
    config: &Config,
) -> Result<(), RuntimeAppError> {
    let manager = production_hook_manager(environment)?;
    let mut observations = Vec::new();
    for (provider, name) in [(Provider::Claude, "claude"), (Provider::Codex, "codex")] {
        let provider_config = provider_config(&config.hooks, provider);
        let statuses = manager.status_with_config(provider, provider_config)?;
        let installed = statuses.iter().any(|status| status.installed);
        observations.push(HookObservation {
            provider: name,
            installed,
            synchronized: installed
                && statuses
                    .iter()
                    .filter(|status| status.installed)
                    .all(|status| status.in_sync),
        });
    }
    let probe = SystemProbe {
        paths,
        environment,
        tmux: selected.map(|selected| &selected.server),
        daemon_socket: selected.map(|selected| selected.daemon_socket.as_path()),
        hooks: &observations,
    };
    let unicode = config.display.unicode != FeatureMode::Never;
    let healthy = run_doctor_command(&probe, arguments, io::stdout().lock(), unicode)?;
    if healthy {
        Ok(())
    } else {
        Err(RuntimeAppError::DoctorUnhealthy)
    }
}

struct RuntimeHookSubmitter {
    selected: SelectedServer,
    handle: tokio::runtime::Handle,
}

impl HookSubmitter for RuntimeHookSubmitter {
    type Error = String;

    fn submit_and_wait(
        &self,
        notification: crate::notification::NotificationDraft,
        ack_timeout: Duration,
    ) -> Result<(), Self::Error> {
        let request = ClientRequest::new(ClientCommand::Send {
            notification: Box::new(WireNotificationDraft::from_domain(&notification)),
        });
        let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        self.handle.block_on(async {
            tokio::time::timeout(
                ack_timeout,
                submit_lazy(
                    &self.selected.daemon_socket,
                    self.selected.identity.tmux_socket(),
                    &payload,
                ),
            )
            .await
            .map_err(|_| "hook acknowledgement timed out".to_owned())?
            .map_err(|error| error.to_string())
            .and_then(|frame| {
                match decode_client_response(&frame, request.request_id)
                    .map_err(|error| error.to_string())?
                {
                    ClientResponse::Success(_) => Ok(()),
                    ClientResponse::Error(error) => Err(error),
                }
            })
        })
    }
}

async fn run_hook_event_silent(cli: Cli) {
    let Command::HookEvent(HookEventArgs { provider }) = cli.command else {
        return;
    };
    let provider: Provider = provider.into();
    let environment = Environment::current();
    let operation = || -> Result<_, RuntimeAppError> {
        let paths = PlatformPaths::resolve(&environment)?;
        let config = load(&paths.config_file, &environment, ConfigOverrides::default())?;
        let tmux_environment = environment.get("TMUX").and_then(|value| value.to_str());
        let target = cli.tmux_target(tmux_environment)?;
        let selected = SelectedServer::new(
            resolve_server(target.ok_or(RuntimeAppError::MissingTmuxTarget)?)?,
            &paths,
        )?;
        let source = capture_source(&selected, &environment)?;
        let logger = PrivateLogger::new(paths.log_file).ok();
        Ok((selected, config, source, logger))
    }();
    let Ok((selected, config, source, logger)) = operation else {
        return;
    };
    let policy = HookPolicy::from(provider_config(&config.hooks, provider));
    let submitter = RuntimeHookSubmitter {
        selected,
        handle: tokio::runtime::Handle::current(),
    };
    let _ = tokio::task::spawn_blocking(move || {
        receive_hook_event(
            provider,
            io::stdin().lock(),
            &policy,
            source,
            &submitter,
            logger.as_ref(),
        );
    })
    .await;
}

#[derive(Debug, Error)]
pub enum RuntimeAppError {
    #[error("a tmux target is required; run inside tmux or pass -L/-S")]
    MissingTmuxTarget,
    #[error("History outside tmux requires -L, -S, or --all-servers")]
    MissingHistoryTarget,
    #[error("History inside tmux requires a valid current Source Pane")]
    HistorySourceUnavailable,
    #[error("non-interactive History Clear requires --yes")]
    ConfirmationRequired,
    #[error("invalid --before age {0:?}; use a positive m, h, or d duration")]
    InvalidAge(String),
    #[error("doctor found failing diagnostics")]
    DoctorUnhealthy,
    #[error("daemon rejected the request: {0}")]
    Remote(String),
    #[error("{0}")]
    Execution(String),
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    HistoryAction(#[from] crate::daemon::application::HistoryActionError),
    #[error(transparent)]
    ConfigReload(#[from] crate::daemon::application::ConfigReloadError),
    #[error(transparent)]
    Cli(#[from] crate::cli::CliError),
    #[error(transparent)]
    CliBuild(#[from] crate::cli::CliBuildError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    HistoryOutput(#[from] crate::history::HistoryOutputError),
    #[error(transparent)]
    HistoryUi(#[from] crate::ui::history::HistoryUiError),
    #[error(transparent)]
    Hook(#[from] HookError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Daemon(#[from] crate::daemon::application::Error),
    #[error(transparent)]
    Scheduler(#[from] crate::daemon::SchedulerError),
    #[error(transparent)]
    Tmux(#[from] crate::tmux::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ingress(#[from] crate::notification::IngressError),
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::cli::{HookMutationArgs, HookScopeArg, HookSyncArgs};
    use crate::config::HooksConfig;
    use crate::platform::Environment;

    fn environment(root: &Path) -> Environment {
        Environment::from_pairs([(OsString::from("HOME"), root.as_os_str().to_owned())])
    }

    fn manager(temporary: &TempDir) -> HookManager {
        HookManager::new(
            temporary.path().join("bin/tmnotify"),
            temporary.path().join("project"),
            &environment(temporary.path()),
        )
        .unwrap()
    }

    #[test]
    fn hook_mutations_are_silent_and_status_is_a_stdout_result() {
        let temporary = TempDir::new().unwrap();
        let manager = manager(&temporary);
        let config = HooksConfig::default();
        let mut output = Vec::new();
        run_hook_command(
            &manager,
            &config,
            HookArgs {
                action: HookAction::Install(HookMutationArgs {
                    provider: ProviderArg::Claude,
                    scope: Some(HookScopeArg::Project),
                    allow_mixed: false,
                }),
            },
            &mut output,
        )
        .unwrap();
        assert!(output.is_empty());

        run_hook_command(
            &manager,
            &config,
            HookArgs {
                action: HookAction::Status(HookSelectionArgs {
                    provider: Some(ProviderArg::Claude),
                }),
            },
            &mut output,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("claude\tproject\tin-sync"));
        assert!(output.contains("trust: unknown — verify in /hooks"));
    }

    #[test]
    fn sync_without_provider_processes_both_providers_without_creating_scopes() {
        let temporary = TempDir::new().unwrap();
        let manager = manager(&temporary);
        run_hook_command(
            &manager,
            &HooksConfig::default(),
            HookArgs {
                action: HookAction::Sync(HookSyncArgs {
                    provider: None,
                    allow_mixed: false,
                }),
            },
            Vec::new(),
        )
        .unwrap();
        assert!(!temporary.path().join(".claude/settings.json").exists());
        assert!(!temporary.path().join(".codex/hooks.json").exists());
    }

    #[test]
    fn mutation_default_scope_is_user() {
        let (_, scope, allow_mixed) = mutation_parts(HookMutationArgs {
            provider: ProviderArg::Codex,
            scope: None,
            allow_mixed: true,
        });
        assert_eq!(scope, Scope::User);
        assert!(allow_mixed);
    }

    #[test]
    fn clear_age_is_bounded_and_requires_an_explicit_unit() {
        assert_eq!(parse_age("30d").unwrap(), Duration::from_secs(30 * 86_400));
        assert_eq!(parse_age("2h").unwrap(), Duration::from_secs(7_200));
        assert!(parse_age("30").is_err());
        assert!(parse_age("forever").is_err());
        assert!(parse_age("18446744073709551615d").is_err());
    }

    #[test]
    fn history_defaults_to_tui_but_preserves_scriptable_output_modes() {
        let arguments = HistoryArgs {
            action: None,
            plain: false,
            json: false,
            all: false,
            all_servers: false,
        };
        assert_eq!(history_mode(&arguments, true, true), HistoryMode::Floating);
        assert_eq!(history_mode(&arguments, true, false), HistoryMode::Terminal);
        assert_eq!(history_mode(&arguments, false, true), HistoryMode::Output);

        let plain = HistoryArgs {
            plain: true,
            ..arguments
        };
        assert_eq!(history_mode(&plain, true, true), HistoryMode::Output);
    }
}
