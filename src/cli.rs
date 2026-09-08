//! The one-binary command surface.
//!
//! Parsing and target selection live here; execution is delegated to the deep
//! product modules so CLI concerns do not become scheduler policy.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use thiserror::Error;

use crate::notification::{
    IngressError, Level, MAX_IPC_REQUEST_BYTES, NotificationDraft, NotificationKey,
    NotificationUpdate, Placement, Presentation, PresentationOverrides, Priority, SourceContext,
    Timeout,
};

#[derive(Debug, Parser, PartialEq)]
#[command(name = "tmnotify", version, about)]
pub struct Cli {
    /// Select a tmux server by socket name, like tmux -L.
    #[arg(
        short = 'L',
        long = "socket-name",
        global = true,
        conflicts_with = "socket_path"
    )]
    pub socket_name: Option<String>,

    /// Select a tmux server by socket path, like tmux -S.
    #[arg(
        short = 'S',
        long = "socket-path",
        global = true,
        conflicts_with = "socket_name"
    )]
    pub socket_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum Command {
    Send(SendArgs),
    Update(UpdateArgs),
    Dismiss(Selector),
    /// Jump to the Source Pane of a live keyed Notification.
    Jump(JumpArgs),
    History(HistoryArgs),
    Hook(HookArgs),
    Doctor(DoctorArgs),
    #[command(name = "__daemon", hide = true)]
    Daemon,
    #[command(name = "__render-toast", hide = true)]
    RenderToast(RendererArgs),
    #[command(name = "__render-attention", hide = true)]
    RenderAttention(RendererArgs),
    #[command(name = "__history-ui", hide = true)]
    HistoryUi(HistoryUiArgs),
    #[command(name = "__hook-event", hide = true)]
    HookEvent(HookEventArgs),
}

#[derive(Debug, Args, PartialEq)]
pub struct SendArgs {
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long, value_enum, default_value_t = LevelArg::Info)]
    pub level: LevelArg,
    #[arg(long, value_enum, default_value_t = PriorityArg::Normal)]
    pub priority: PriorityArg,
    #[arg(long)]
    pub attention: bool,
    #[arg(long)]
    pub timeout: Option<String>,
    #[arg(long, value_enum)]
    pub position: Option<PlacementArg>,
    #[arg(long)]
    pub key: Option<String>,
    #[arg(long)]
    pub no_source: bool,
    #[arg(long)]
    pub json: bool,
    /// Notification body, or - to read bounded multiline stdin.
    pub message: String,
}

#[derive(Debug, Args, PartialEq)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub selector: Selector,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long, value_enum)]
    pub level: Option<LevelArg>,
    #[arg(long, value_enum)]
    pub priority: Option<PriorityArg>,
    #[arg(long)]
    pub timeout: Option<String>,
    #[arg(long, value_enum)]
    pub position: Option<PlacementArg>,
    #[arg(long)]
    pub json: bool,
    /// Replacement body, or - to read bounded multiline stdin.
    pub message: Option<String>,
}

#[derive(Debug, Args, PartialEq)]
#[group(required = true, multiple = false)]
pub struct Selector {
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long)]
    pub key: Option<String>,
}

#[derive(Debug, Args, PartialEq)]
pub struct JumpArgs {
    /// Notification Key of a live Notification in the selected server.
    #[arg(long, required = true)]
    pub key: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub action: Option<HistoryAction>,
    #[arg(long, conflicts_with = "json")]
    pub plain: bool,
    #[arg(long, conflicts_with = "plain")]
    pub json: bool,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub all_servers: bool,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum HistoryAction {
    Clear(ClearArgs),
}

#[derive(Debug, Args, PartialEq)]
pub struct ClearArgs {
    #[arg(
        long,
        required_unless_present_any = ["before", "all"],
        conflicts_with_all = ["before", "all"]
    )]
    pub hidden: bool,
    #[arg(
        long,
        required_unless_present_any = ["hidden", "all"],
        conflicts_with_all = ["hidden", "all"]
    )]
    pub before: Option<String>,
    #[arg(
        long,
        required_unless_present_any = ["hidden", "before"],
        conflicts_with_all = ["hidden", "before"]
    )]
    pub all: bool,
    #[arg(long)]
    pub all_servers: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct HookArgs {
    #[command(subcommand)]
    pub action: HookAction,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum HookAction {
    Install(HookMutationArgs),
    Remove(HookMutationArgs),
    Sync(HookSyncArgs),
    Status(HookSelectionArgs),
}

#[derive(Debug, Args, PartialEq)]
pub struct HookMutationArgs {
    #[arg(value_enum)]
    pub provider: ProviderArg,
    #[arg(value_enum)]
    pub scope: Option<HookScopeArg>,
    #[arg(long)]
    pub allow_mixed: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct HookSelectionArgs {
    #[arg(value_enum)]
    pub provider: Option<ProviderArg>,
}

#[derive(Debug, Args, PartialEq)]
pub struct HookSyncArgs {
    #[arg(value_enum)]
    pub provider: Option<ProviderArg>,
    /// Keep Codex inline TOML hooks unchanged while syncing hooks.json.
    #[arg(long)]
    pub allow_mixed: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct RendererArgs {
    #[arg(long)]
    pub window_display: String,
    #[arg(long)]
    pub token: String,
}

#[derive(Debug, Args, PartialEq)]
pub struct HistoryUiArgs {
    #[arg(long)]
    pub all_servers: bool,
    #[arg(long)]
    pub include_hidden: bool,
}

#[derive(Debug, Args, PartialEq)]
pub struct HookEventArgs {
    #[arg(value_enum)]
    pub provider: ProviderArg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LevelArg {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PriorityArg {
    Low,
    Normal,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PlacementArg {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HookScopeArg {
    User,
    Project,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxTarget {
    SocketName(String),
    SocketPath(PathBuf),
}

impl Cli {
    /// Resolves only an explicit selector or the socket embedded in TMUX.
    /// It never guesses the default tmux server outside tmux.
    pub fn tmux_target(
        &self,
        tmux_environment: Option<&str>,
    ) -> Result<Option<TmuxTarget>, CliError> {
        if let Some(name) = &self.socket_name {
            return Ok(Some(TmuxTarget::SocketName(name.clone())));
        }
        if let Some(path) = &self.socket_path {
            return Ok(Some(TmuxTarget::SocketPath(path.clone())));
        }
        match tmux_environment {
            Some(value) => parse_tmux_socket(value).map(|path| path.map(TmuxTarget::SocketPath)),
            None => Ok(None),
        }
    }

    #[must_use]
    pub fn requires_tmux_target(&self) -> bool {
        !matches!(
            self.command,
            Command::Hook(_) | Command::Doctor(_) | Command::HookEvent(_)
        )
    }
}

impl SendArgs {
    pub fn into_draft(
        self,
        source: Option<SourceContext>,
        stdin: impl Read,
    ) -> Result<NotificationDraft, CliBuildError> {
        let body = message_or_stdin(&self.message, stdin)?;
        let presentation = if self.attention {
            Presentation::Attention
        } else {
            Presentation::Toast
        };
        let source = if self.no_source { None } else { source };
        let mut draft = NotificationDraft::new(
            presentation,
            self.title.as_deref().unwrap_or_default(),
            &body,
            source,
        )?
        .with_level(self.level.into())
        .with_priority(self.priority.into());
        if let Some(timeout) = self.timeout {
            draft = draft.with_timeout(parse_timeout(&timeout)?);
        }
        if let Some(position) = self.position {
            draft = draft
                .with_overrides(PresentationOverrides::default().with_position(position.into()));
        }
        if let Some(key) = self.key {
            draft = draft.with_key(NotificationKey::new(&key)?);
        }
        Ok(draft)
    }
}

impl UpdateArgs {
    pub fn into_update(self, stdin: impl Read) -> Result<NotificationUpdate, CliBuildError> {
        let mut update = NotificationUpdate::new();
        if let Some(title) = self.title {
            update = update.with_title(&title)?;
        }
        if let Some(message) = self.message {
            update = update.with_body(&message_or_stdin(&message, stdin)?)?;
        }
        if let Some(level) = self.level {
            update = update.with_level(level.into());
        }
        if let Some(priority) = self.priority {
            update = update.with_priority(priority.into());
        }
        if let Some(timeout) = self.timeout {
            update = update.with_timeout(parse_timeout(&timeout)?);
        }
        if let Some(position) = self.position {
            update = update
                .with_overrides(PresentationOverrides::default().with_position(position.into()));
        }
        if update.is_empty() {
            return Err(CliBuildError::EmptyUpdate);
        }
        Ok(update)
    }
}

fn parse_timeout(value: &str) -> Result<Timeout, CliBuildError> {
    if value == "never" {
        return Ok(Timeout::Never);
    }
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        return Err(CliBuildError::InvalidTimeout);
    };
    let number: u64 = number.parse().map_err(|_| CliBuildError::InvalidTimeout)?;
    let milliseconds = number
        .checked_mul(multiplier)
        .filter(|value| *value <= 86_400_000)
        .ok_or(CliBuildError::InvalidTimeout)?;
    Ok(Timeout::After(Duration::from_millis(milliseconds)))
}

impl From<LevelArg> for Level {
    fn from(value: LevelArg) -> Self {
        match value {
            LevelArg::Info => Self::Info,
            LevelArg::Success => Self::Success,
            LevelArg::Warning => Self::Warning,
            LevelArg::Error => Self::Error,
        }
    }
}

impl From<PriorityArg> for Priority {
    fn from(value: PriorityArg) -> Self {
        match value {
            PriorityArg::Low => Self::Low,
            PriorityArg::Normal => Self::Normal,
            PriorityArg::High => Self::High,
            PriorityArg::Critical => Self::Critical,
        }
    }
}

impl From<PlacementArg> for Placement {
    fn from(value: PlacementArg) -> Self {
        match value {
            PlacementArg::TopLeft => Self::TopLeft,
            PlacementArg::TopCenter => Self::TopCenter,
            PlacementArg::TopRight => Self::TopRight,
            PlacementArg::BottomLeft => Self::BottomLeft,
            PlacementArg::BottomCenter => Self::BottomCenter,
            PlacementArg::BottomRight => Self::BottomRight,
        }
    }
}

fn parse_tmux_socket(value: &str) -> Result<Option<PathBuf>, CliError> {
    if value.is_empty() {
        return Ok(None);
    }
    let (path, rest) = value
        .split_once(',')
        .ok_or(CliError::InvalidTmuxEnvironment)?;
    if path.is_empty() || !rest.contains(',') {
        return Err(CliError::InvalidTmuxEnvironment);
    }
    Ok(Some(PathBuf::from(path)))
}

pub fn read_bounded_stdin(reader: impl Read) -> Result<String, CliError> {
    read_bounded(reader, MAX_IPC_REQUEST_BYTES)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<String, CliError> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    reader
        .by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(CliError::ReadStdin)?;
    if bytes.len() > limit {
        return Err(CliError::StdinTooLarge { limit });
    }
    String::from_utf8(bytes).map_err(|_| CliError::StdinNotUtf8)
}

pub fn message_or_stdin(message: &str, stdin: impl Read) -> Result<String, CliError> {
    if message == "-" {
        read_bounded_stdin(stdin)
    } else {
        Ok(message.to_owned())
    }
}

pub fn parse_from<I, T>(arguments: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(arguments)
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("TMUX does not contain a valid tmux socket path")]
    InvalidTmuxEnvironment,
    #[error("stdin exceeds the {limit}-byte limit")]
    StdinTooLarge { limit: usize },
    #[error("stdin is not valid UTF-8")]
    StdinNotUtf8,
    #[error("failed to read stdin: {0}")]
    ReadStdin(io::Error),
}

#[derive(Debug, Error)]
pub enum CliBuildError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Ingress(#[from] IngressError),
    #[error("timeout must be never or an integer with ms, s, m, or h suffix up to 24h")]
    InvalidTimeout,
    #[error("update requires at least one changed field")]
    EmptyUpdate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::notification::{Notification, TmuxServerId};

    fn source() -> SourceContext {
        SourceContext::new(TmuxServerId::new("server").unwrap(), "$1", "@2", "%3").unwrap()
    }

    #[test]
    fn parses_documented_send_defaults_and_key() {
        let cli = parse_from(["tmnotify", "send", "--key", "build", "done"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Send(SendArgs {
                title: None,
                level: LevelArg::Info,
                priority: PriorityArg::Normal,
                attention: false,
                timeout: None,
                position: None,
                key: Some("build".into()),
                no_source: false,
                json: false,
                message: "done".into(),
            })
        );
    }

    #[test]
    fn direct_jump_requires_a_key() {
        assert!(parse_from(["tmnotify", "jump"]).is_err());
        let cli = parse_from(["tmnotify", "jump", "--key", "build"]).unwrap();
        assert!(matches!(cli.command, Command::Jump(JumpArgs { key, .. }) if key == "build"));
    }

    #[test]
    fn update_and_dismiss_require_one_explicit_selector() {
        assert!(parse_from(["tmnotify", "update", "body"]).is_err());
        assert!(parse_from(["tmnotify", "dismiss"]).is_err());
        assert!(parse_from(["tmnotify", "dismiss", "--id", "id", "--key", "key"]).is_err());
        assert!(parse_from(["tmnotify", "update", "--key", "build", "done"]).is_ok());
    }

    #[test]
    fn socket_selectors_are_mutually_exclusive_and_global() {
        assert!(parse_from(["tmnotify", "-L", "one", "-S", "/tmp/tmux", "send", "done"]).is_err());
        let cli = parse_from(["tmnotify", "send", "-L", "work", "done"]).unwrap();
        assert_eq!(
            cli.tmux_target(None).unwrap(),
            Some(TmuxTarget::SocketName("work".into()))
        );
    }

    #[test]
    fn target_resolution_uses_tmux_and_never_guesses() {
        let cli = parse_from(["tmnotify", "send", "done"]).unwrap();
        assert_eq!(cli.tmux_target(None).unwrap(), None);
        assert_eq!(
            cli.tmux_target(Some("/tmp/tmux-1000/default,123,0"))
                .unwrap(),
            Some(TmuxTarget::SocketPath("/tmp/tmux-1000/default".into()))
        );
        assert!(cli.tmux_target(Some("broken")).is_err());
    }

    #[test]
    fn clear_requires_a_real_selector() {
        assert!(parse_from(["tmnotify", "history", "clear", "--all-servers"]).is_err());
        assert!(parse_from(["tmnotify", "history", "clear", "--hidden", "--all-servers"]).is_ok());
    }

    #[test]
    fn stdin_is_bounded_and_utf8() {
        assert_eq!(message_or_stdin("inline", io::empty()).unwrap(), "inline");
        assert_eq!(message_or_stdin("-", &b"one\ntwo"[..]).unwrap(), "one\ntwo");
        assert!(matches!(
            read_bounded(&b"12345"[..], 4),
            Err(CliError::StdinTooLarge { limit: 4 })
        ));
        assert!(matches!(
            read_bounded(&[0xff][..], 4),
            Err(CliError::StdinNotUtf8)
        ));
    }

    #[test]
    fn plain_and_json_history_modes_conflict() {
        assert!(parse_from(["tmnotify", "history", "--plain", "--json"]).is_err());
    }

    #[test]
    fn hook_sync_accepts_the_explicit_mixed_codex_override() {
        let cli = parse_from(["tmnotify", "hook", "sync", "codex", "--allow-mixed"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Hook(HookArgs {
                action: HookAction::Sync(HookSyncArgs {
                    provider: Some(ProviderArg::Codex),
                    allow_mixed: true,
                }),
            })
        );
    }

    #[test]
    fn send_builds_a_normalized_keyed_draft_and_honors_no_source() {
        let cli = parse_from([
            "tmnotify",
            "send",
            "--key",
            "build",
            "--level",
            "success",
            "--priority",
            "high",
            "--timeout",
            "2s",
            "--position",
            "bottom-right",
            "done\u{1b}[31m",
        ])
        .unwrap();
        let Command::Send(arguments) = cli.command else {
            panic!("expected send");
        };
        let notification = Notification::from_draft(
            arguments.into_draft(Some(source()), io::empty()).unwrap(),
            Utc::now(),
        );
        assert_eq!(notification.body(), "done");
        assert_eq!(notification.key().unwrap().as_str(), "build");
        assert_eq!(notification.level(), Level::Success);
        assert_eq!(notification.priority(), Priority::High);
        assert_eq!(
            notification.timeout(),
            Timeout::After(Duration::from_secs(2))
        );
        assert_eq!(
            notification.overrides().position(),
            Some(Placement::BottomRight)
        );

        let cli = parse_from(["tmnotify", "send", "--no-source", "done"]).unwrap();
        let Command::Send(arguments) = cli.command else {
            panic!("expected send");
        };
        let notification = Notification::from_draft(
            arguments.into_draft(Some(source()), io::empty()).unwrap(),
            Utc::now(),
        );
        assert!(notification.source().is_none());
    }

    #[test]
    fn attention_requires_captured_source_and_forces_never_timeout() {
        let parse_attention = || {
            parse_from([
                "tmnotify",
                "send",
                "--attention",
                "--timeout",
                "2s",
                "input",
            ])
            .unwrap()
        };
        let Command::Send(arguments) = parse_attention().command else {
            panic!("expected send");
        };
        assert!(matches!(
            arguments.into_draft(None, io::empty()),
            Err(CliBuildError::Ingress(
                IngressError::AttentionRequiresSource
            ))
        ));

        let Command::Send(arguments) = parse_attention().command else {
            panic!("expected send");
        };
        let notification = Notification::from_draft(
            arguments.into_draft(Some(source()), io::empty()).unwrap(),
            Utc::now(),
        );
        assert_eq!(notification.timeout(), Timeout::Never);
    }

    #[test]
    fn update_builds_partial_domain_changes_and_rejects_bad_timeout_or_empty() {
        let cli = parse_from([
            "tmnotify",
            "update",
            "--key",
            "build",
            "--timeout",
            "never",
            "-",
        ])
        .unwrap();
        let Command::Update(arguments) = cli.command else {
            panic!("expected update");
        };
        let update = arguments.into_update(&b"done\nnow"[..]).unwrap();
        assert_eq!(update.body(), Some("done\nnow"));
        assert_eq!(update.timeout(), Some(Timeout::Never));

        let cli = parse_from(["tmnotify", "update", "--key", "build", "--timeout", "2d"]).unwrap();
        let Command::Update(arguments) = cli.command else {
            panic!("expected update");
        };
        assert!(matches!(
            arguments.into_update(io::empty()),
            Err(CliBuildError::InvalidTimeout)
        ));

        let cli = parse_from(["tmnotify", "update", "--key", "build"]).unwrap();
        let Command::Update(arguments) = cli.command else {
            panic!("expected update");
        };
        assert!(matches!(
            arguments.into_update(io::empty()),
            Err(CliBuildError::EmptyUpdate)
        ));
    }
}
