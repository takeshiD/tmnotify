//! Conservative provider hook installation and synchronous hook ingress.
//!
//! The module deliberately owns the whole provider-file mutation boundary. It
//! recognizes only tmnotify's fixed handlers, leaves ambiguous commands alone,
//! and never reads or changes provider trust state.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};
use thiserror::Error;

use crate::cli::{HookScopeArg, ProviderArg};
use crate::config::{HookEvent, HookProviderConfig};
use crate::notification::{AgentEventKind, NotificationDraft, Provider, SourceContext};
use crate::platform::{Environment, LogEvent, LogLevel, PrivateLogger};
use crate::providers::{self, HookPolicy};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const MAX_PROVIDER_CONFIG_BYTES: u64 = 1024 * 1024;
const PRIVATE_FILE_MODE: u32 = 0o600;
pub const HOOK_ACK_TIMEOUT: Duration = Duration::from_secs(2);
pub const TRUST_GUIDANCE: &str = "unknown — verify in /hooks";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl From<HookScopeArg> for Scope {
    fn from(value: HookScopeArg) -> Self {
        match value {
            HookScopeArg::User => Self::User,
            HookScopeArg::Project => Self::Project,
            HookScopeArg::Local => Self::Local,
        }
    }
}

impl From<ProviderArg> for Provider {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Claude => Self::Claude,
            ProviderArg::Codex => Self::Codex,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mutation {
    Changed,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeStatus {
    pub provider: Provider,
    pub scope: Scope,
    pub path: PathBuf,
    pub installed: bool,
    pub in_sync: bool,
    /// Provider trust has no stable machine-readable interface.
    pub trust: &'static str,
}

#[derive(Clone, Debug)]
pub struct HookManager {
    executable: PathBuf,
    home: Option<PathBuf>,
    project: PathBuf,
    claude_config_directory: Option<PathBuf>,
}

impl HookManager {
    pub fn new(
        executable: PathBuf,
        project: PathBuf,
        environment: &Environment,
    ) -> Result<Self, HookError> {
        if !executable.is_absolute() || executable.file_name() != Some(OsStr::new("tmnotify")) {
            return Err(HookError::InvalidExecutable(executable));
        }
        let home = environment
            .get("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let claude_config_directory = environment
            .get("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            executable,
            home,
            project,
            claude_config_directory,
        })
    }

    #[must_use]
    pub fn supported_scopes(provider: Provider) -> &'static [Scope] {
        match provider {
            Provider::Claude => &[Scope::User, Scope::Project, Scope::Local],
            Provider::Codex => &[Scope::User, Scope::Project],
        }
    }

    pub fn path(&self, provider: Provider, scope: Scope) -> Result<PathBuf, HookError> {
        match (provider, scope) {
            (Provider::Claude, Scope::User) => Ok(self
                .claude_config_directory
                .clone()
                .or_else(|| self.home.as_ref().map(|home| home.join(".claude")))
                .ok_or(HookError::HomeUnavailable)?
                .join("settings.json")),
            (Provider::Claude, Scope::Project) => Ok(self.project.join(".claude/settings.json")),
            (Provider::Claude, Scope::Local) => {
                Ok(self.project.join(".claude/settings.local.json"))
            }
            (Provider::Codex, Scope::User) => Ok(self
                .home
                .as_ref()
                .ok_or(HookError::HomeUnavailable)?
                .join(".codex/hooks.json")),
            (Provider::Codex, Scope::Project) => Ok(self.project.join(".codex/hooks.json")),
            (Provider::Codex, Scope::Local) => Err(HookError::UnsupportedScope { provider, scope }),
        }
    }

    pub fn install(
        &self,
        provider: Provider,
        scope: Scope,
        config: &HookProviderConfig,
        allow_mixed: bool,
    ) -> Result<Mutation, HookError> {
        let path = self.path(provider, scope)?;
        self.reject_inline_codex(provider, scope, allow_mixed)?;
        self.reconcile_file(&path, provider, config, ReconcileMode::Install)
    }

    pub fn remove(&self, provider: Provider, scope: Scope) -> Result<Mutation, HookError> {
        let path = self.path(provider, scope)?;
        self.reconcile_file(
            &path,
            provider,
            &HookProviderConfig::default(),
            ReconcileMode::Remove,
        )
    }

    /// Reconciles only scopes containing a structurally owned handler.
    pub fn sync(
        &self,
        provider: Provider,
        config: &HookProviderConfig,
        allow_mixed: bool,
    ) -> Result<Vec<(Scope, Mutation)>, HookError> {
        let mut outcomes = Vec::new();
        for &scope in Self::supported_scopes(provider) {
            let path = self.path(provider, scope)?;
            let document = match read_document(&path)? {
                Some(document) => document,
                None => continue,
            };
            if !contains_owned_handler(&document.value, provider) {
                continue;
            }
            self.reject_inline_codex(provider, scope, allow_mixed)?;
            outcomes.push((
                scope,
                self.reconcile_document(&path, provider, config, ReconcileMode::Sync, document)?,
            ));
        }
        Ok(outcomes)
    }

    pub fn status(&self, provider: Provider) -> Result<Vec<ScopeStatus>, HookError> {
        let desired = desired_events(provider, &HookProviderConfig::default());
        Self::supported_scopes(provider)
            .iter()
            .map(|&scope| {
                let path = self.path(provider, scope)?;
                let Some(document) = read_document(&path)? else {
                    return Ok(ScopeStatus {
                        provider,
                        scope,
                        path,
                        installed: false,
                        in_sync: false,
                        trust: TRUST_GUIDANCE,
                    });
                };
                let installed = contains_owned_handler(&document.value, provider);
                let in_sync = installed
                    && installation_is_in_sync(
                        &document.value,
                        provider,
                        &self.executable,
                        &desired,
                    );
                Ok(ScopeStatus {
                    provider,
                    scope,
                    path,
                    installed,
                    in_sync,
                    trust: TRUST_GUIDANCE,
                })
            })
            .collect()
    }

    pub fn status_with_config(
        &self,
        provider: Provider,
        config: &HookProviderConfig,
    ) -> Result<Vec<ScopeStatus>, HookError> {
        let desired = desired_events(provider, config);
        let mut statuses = self.status(provider)?;
        for status in &mut statuses {
            let Some(document) = read_document(&status.path)? else {
                continue;
            };
            status.in_sync = status.installed
                && installation_is_in_sync(&document.value, provider, &self.executable, &desired);
        }
        Ok(statuses)
    }

    fn reconcile_file(
        &self,
        path: &Path,
        provider: Provider,
        config: &HookProviderConfig,
        mode: ReconcileMode,
    ) -> Result<Mutation, HookError> {
        let document = read_document(path)?.unwrap_or_else(Document::empty);
        if mode == ReconcileMode::Remove && document.original.is_none() {
            return Ok(Mutation::Unchanged);
        }
        self.reconcile_document(path, provider, config, mode, document)
    }

    fn reconcile_document(
        &self,
        path: &Path,
        provider: Provider,
        config: &HookProviderConfig,
        mode: ReconcileMode,
        mut document: Document,
    ) -> Result<Mutation, HookError> {
        let desired = desired_events(provider, config);
        if (mode == ReconcileMode::Remove && !contains_owned_handler(&document.value, provider))
            || (mode != ReconcileMode::Remove
                && installation_is_in_sync(&document.value, provider, &self.executable, &desired))
        {
            return Ok(Mutation::Unchanged);
        }
        reconcile_value(
            &mut document.value,
            provider,
            &self.executable,
            config,
            mode,
        )?;
        let rendered = document.render()?;
        if document.original.as_deref() == Some(rendered.as_slice()) {
            return Ok(Mutation::Unchanged);
        }
        atomic_replace(path, &rendered, document.original.as_deref())?;
        Ok(Mutation::Changed)
    }

    fn reject_inline_codex(
        &self,
        provider: Provider,
        scope: Scope,
        allow_mixed: bool,
    ) -> Result<(), HookError> {
        if provider != Provider::Codex || allow_mixed {
            return Ok(());
        }
        let config = match scope {
            Scope::User => self
                .home
                .as_ref()
                .ok_or(HookError::HomeUnavailable)?
                .join(".codex/config.toml"),
            Scope::Project => self.project.join(".codex/config.toml"),
            Scope::Local => return Ok(()),
        };
        let Some(bytes) = read_bounded_file(&config)? else {
            return Ok(());
        };
        let contents =
            String::from_utf8(bytes).map_err(|_| HookError::InvalidUtf8(config.clone()))?;
        let value: toml::Value = toml::from_str(&contents).map_err(|source| HookError::Toml {
            path: config.clone(),
            source,
        })?;
        if value.get("hooks").is_some_and(toml_value_is_nonempty) {
            return Err(HookError::MixedCodexHooks(config));
        }
        Ok(())
    }
}

fn toml_value_is_nonempty(value: &toml::Value) -> bool {
    match value {
        toml::Value::Array(values) => !values.is_empty(),
        toml::Value::Table(values) => !values.is_empty(),
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileMode {
    Install,
    Sync,
    Remove,
}

#[derive(Debug)]
struct Document {
    value: Value,
    original: Option<Vec<u8>>,
    indent: Vec<u8>,
    newline: &'static str,
    final_newline: bool,
}

impl Document {
    fn empty() -> Self {
        Self {
            value: Value::Object(Map::new()),
            original: None,
            indent: b"  ".to_vec(),
            newline: "\n",
            final_newline: true,
        }
    }

    fn render(&self) -> Result<Vec<u8>, HookError> {
        let formatter = serde_json::ser::PrettyFormatter::with_indent(&self.indent);
        let mut output = Vec::new();
        let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
        self.value
            .serialize(&mut serializer)
            .map_err(HookError::Serialize)?;
        if self.newline == "\r\n" {
            let text = String::from_utf8(output).expect("JSON serialization is UTF-8");
            output = text.replace('\n', "\r\n").into_bytes();
        }
        if self.final_newline {
            output.extend_from_slice(self.newline.as_bytes());
        }
        Ok(output)
    }
}

use serde::Serialize;

fn read_document(path: &Path) -> Result<Option<Document>, HookError> {
    let Some(bytes) = read_bounded_file(path)? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| HookError::Json {
        path: path.to_owned(),
        source,
    })?;
    if !value.is_object() {
        return Err(HookError::RootNotObject(path.to_owned()));
    }
    let newline = if bytes.windows(2).any(|pair| pair == b"\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let final_newline = bytes.ends_with(newline.as_bytes());
    let indent = detect_indent(&bytes).unwrap_or_else(|| b"  ".to_vec());
    Ok(Some(Document {
        value,
        original: Some(bytes),
        indent,
        newline,
        final_newline,
    }))
}

fn read_bounded_file(path: &Path) -> Result<Option<Vec<u8>>, HookError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(HookError::SymbolicLink(path.to_owned()));
        }
        Err(source) => {
            return Err(HookError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| HookError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(HookError::InvalidPath(path.to_owned()));
    }
    if metadata.len() > MAX_PROVIDER_CONFIG_BYTES {
        return Err(HookError::TooLarge(path.to_owned()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_PROVIDER_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| HookError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_PROVIDER_CONFIG_BYTES {
        return Err(HookError::TooLarge(path.to_owned()));
    }
    Ok(Some(bytes))
}

fn detect_indent(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines().skip(1).find_map(|line| {
        let whitespace = line
            .bytes()
            .take_while(|byte| *byte == b' ' || *byte == b'\t')
            .count();
        (whitespace > 0).then(|| line.as_bytes()[..whitespace].to_vec())
    })
}

fn reconcile_value(
    root: &mut Value,
    provider: Provider,
    executable: &Path,
    config: &HookProviderConfig,
    mode: ReconcileMode,
) -> Result<(), HookError> {
    remove_owned_handlers(root, provider);
    if mode == ReconcileMode::Remove {
        return Ok(());
    }
    let events = desired_events(provider, config);
    for event in events {
        add_handler(root, provider, &event, executable)?;
    }
    Ok(())
}

fn hooks_object_mut(root: &mut Value) -> Result<&mut Map<String, Value>, HookError> {
    let object = root.as_object_mut().ok_or(HookError::InvalidHooksShape)?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    hooks.as_object_mut().ok_or(HookError::InvalidHooksShape)
}

fn add_handler(
    root: &mut Value,
    provider: Provider,
    event: &str,
    executable: &Path,
) -> Result<(), HookError> {
    let hooks = hooks_object_mut(root)?;
    let matchers = hooks
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or(HookError::InvalidHooksShape)?;
    let handler = handler_value(provider, executable)?;
    let desired_matcher = required_matcher(provider, event);
    // Reuse a semantically matching group without changing other handlers.
    if let Some(group) = matchers.iter_mut().find(|group| {
        group
            .as_object()
            .is_some_and(|object| object.get("matcher").and_then(Value::as_str) == desired_matcher)
    }) {
        let handlers = group
            .as_object_mut()
            .and_then(|object| object.get_mut("hooks"))
            .and_then(Value::as_array_mut)
            .ok_or(HookError::InvalidHooksShape)?;
        handlers.push(handler);
    } else {
        let group = match desired_matcher {
            Some(matcher) => serde_json::json!({ "matcher": matcher, "hooks": [handler] }),
            None => serde_json::json!({ "hooks": [handler] }),
        };
        matchers.push(group);
    }
    Ok(())
}

fn handler_value(provider: Provider, executable: &Path) -> Result<Value, HookError> {
    let executable = executable
        .to_str()
        .ok_or_else(|| HookError::NonUtf8Executable(executable.to_owned()))?;
    Ok(match provider {
        Provider::Claude => serde_json::json!({
            "type": "command",
            "command": executable,
            "args": ["__hook-event", "claude"]
        }),
        Provider::Codex => serde_json::json!({
            "type": "command",
            "command": format!("{} __hook-event codex", shell_quote(executable))
        }),
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn remove_owned_handlers(root: &mut Value, provider: Provider) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let event_names: Vec<String> = hooks.keys().cloned().collect();
    for event in event_names {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for group in groups.iter_mut() {
            let Some(handlers) = group
                .as_object_mut()
                .and_then(|object| object.get_mut("hooks"))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            handlers.retain(|handler| !owned_executable(handler, provider).is_some());
        }
        groups.retain(|group| {
            group
                .as_object()
                .and_then(|object| object.get("hooks"))
                .and_then(Value::as_array)
                .is_none_or(|handlers| !handlers.is_empty())
        });
        if groups.is_empty() {
            hooks.remove(&event);
        }
    }
}

fn contains_owned_handler(root: &Value, provider: Provider) -> bool {
    visit_handlers(root).any(|(_, handler)| owned_executable(handler, provider).is_some())
}

fn installation_is_in_sync(
    root: &Value,
    provider: Provider,
    executable: &Path,
    desired: &BTreeSet<String>,
) -> bool {
    let owned: Vec<(&str, Option<&str>, PathBuf)> = visit_handler_records(root)
        .filter_map(|(event, matcher, handler)| {
            owned_executable(handler, provider).map(|path| (event, matcher, path))
        })
        .collect();
    owned.len() == desired.len()
        && owned.iter().all(|(event, matcher, path)| {
            path == executable
                && desired.contains(*event)
                && *matcher == required_matcher(provider, event)
        })
}

fn visit_handlers(root: &Value) -> impl Iterator<Item = (&str, &Value)> {
    visit_handler_records(root).map(|(event, _, handler)| (event, handler))
}

fn visit_handler_records(root: &Value) -> impl Iterator<Item = (&str, Option<&str>, &Value)> {
    root.get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|hooks| hooks.iter())
        .flat_map(|(event, groups)| {
            groups
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(move |group| {
                    let matcher = group.get("matcher").and_then(Value::as_str);
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(move |handler| (event.as_str(), matcher, handler))
                })
        })
}

fn required_matcher(provider: Provider, event: &str) -> Option<&'static str> {
    (provider == Provider::Claude && event == "Notification").then_some("agent_needs_input")
}

fn owned_executable(handler: &Value, provider: Provider) -> Option<PathBuf> {
    let object = handler.as_object()?;
    if object.get("type")?.as_str()? != "command" {
        return None;
    }
    let command = object.get("command")?.as_str()?;
    let executable = match provider {
        Provider::Claude => {
            let args = object.get("args")?.as_array()?;
            if args.len() != 2
                || args[0].as_str() != Some("__hook-event")
                || args[1].as_str() != Some("claude")
            {
                return None;
            }
            PathBuf::from(command)
        }
        Provider::Codex => {
            if object.contains_key("args") {
                return None;
            }
            parse_codex_command(command)?
        }
    };
    (executable.is_absolute() && executable.file_name() == Some(OsStr::new("tmnotify")))
        .then_some(executable)
}

fn parse_codex_command(command: &str) -> Option<PathBuf> {
    let prefix = command.strip_suffix(" __hook-event codex")?;
    let decoded = if prefix.starts_with('\'') {
        decode_single_quoted_word(prefix)?
    } else if !prefix.is_empty()
        && prefix.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
        })
    {
        prefix.to_owned()
    } else {
        return None;
    };
    Some(PathBuf::from(decoded))
}

fn decode_single_quoted_word(value: &str) -> Option<String> {
    if !value.starts_with('\'') || !value.ends_with('\'') {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut output = String::new();
    let mut remaining = inner;
    while let Some(index) = remaining.find("'\"'\"'") {
        output.push_str(&remaining[..index]);
        output.push('\'');
        remaining = &remaining[index + 5..];
    }
    if remaining.contains('\'') {
        return None;
    }
    output.push_str(remaining);
    Some(output)
}

fn desired_events(provider: Provider, config: &HookProviderConfig) -> BTreeSet<String> {
    const EVENTS: &[(HookEvent, AgentEventKind)] = &[
        (HookEvent::NeedsAttention, AgentEventKind::NeedsAttention),
        (HookEvent::Completed, AgentEventKind::Completed),
        (HookEvent::Failed, AgentEventKind::Failed),
        (HookEvent::Started, AgentEventKind::Started),
        (HookEvent::Interrupted, AgentEventKind::Interrupted),
        (
            HookEvent::SubagentCompleted,
            AgentEventKind::SubagentCompleted,
        ),
        (HookEvent::ToolStarted, AgentEventKind::ToolStarted),
        (HookEvent::ToolCompleted, AgentEventKind::ToolCompleted),
    ];
    let policy = HookPolicy::from(config);
    EVENTS
        .iter()
        .filter(|(_, kind)| policy.is_enabled(*kind))
        .flat_map(|(event, _)| provider_event_names(provider, *event))
        .map(|event| (*event).to_owned())
        .collect()
}

fn provider_event_names(provider: Provider, event: HookEvent) -> &'static [&'static str] {
    match (provider, event) {
        (Provider::Claude, HookEvent::NeedsAttention) => &["PermissionRequest", "Notification"],
        (Provider::Claude, HookEvent::Completed) => &["Stop"],
        (Provider::Claude, HookEvent::Failed) => &["StopFailure"],
        (Provider::Claude, HookEvent::Started) => &["SessionStart"],
        // SessionEnd includes clear/resume/logout/normal exits and cannot
        // identify an interruption without inventing provider semantics.
        (Provider::Claude, HookEvent::Interrupted) => &[],
        (Provider::Claude, HookEvent::SubagentCompleted) => &["SubagentStop"],
        (Provider::Claude, HookEvent::ToolStarted) => &["PreToolUse"],
        (Provider::Claude, HookEvent::ToolCompleted) => &["PostToolUse"],
        (Provider::Codex, HookEvent::NeedsAttention) => &["PermissionRequest"],
        (Provider::Codex, HookEvent::Completed) => &["Stop"],
        (Provider::Codex, HookEvent::Failed) => &[],
        (Provider::Codex, HookEvent::Started) => &["SessionStart"],
        (Provider::Codex, HookEvent::Interrupted) => &["Interrupt"],
        (Provider::Codex, HookEvent::SubagentCompleted) => &["SubagentStop"],
        (Provider::Codex, HookEvent::ToolStarted) => &["PreToolUse"],
        (Provider::Codex, HookEvent::ToolCompleted) => &["PostToolUse"],
    }
}

fn atomic_replace(path: &Path, contents: &[u8], prior: Option<&[u8]>) -> Result<(), HookError> {
    let directory = path
        .parent()
        .ok_or_else(|| HookError::InvalidPath(path.to_owned()))?;
    fs::create_dir_all(directory).map_err(|source| HookError::Io {
        path: directory.to_owned(),
        source,
    })?;
    if let Some(prior) = prior {
        let backup = PathBuf::from(format!("{}.tmnotify.bak", path.display()));
        write_atomic(&backup, prior)?;
        set_private_mode(&backup)?;
    }
    write_atomic(path, contents)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), HookError> {
    let directory = path
        .parent()
        .ok_or_else(|| HookError::InvalidPath(path.to_owned()))?;
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| HookError::InvalidPath(path.to_owned()))?;
    let mut last_error = None;
    for attempt in 0..32_u32 {
        let temporary = directory.join(format!(
            ".{name}.tmnotify.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(PRIVATE_FILE_MODE);
        match options.open(&temporary) {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(contents)?;
                    file.flush()?;
                    file.sync_all()?;
                    #[cfg(unix)]
                    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
                    fs::rename(&temporary, path)?;
                    File::open(directory)?.sync_all()?;
                    Ok::<(), io::Error>(())
                })();
                if let Err(source) = result {
                    let _ = fs::remove_file(&temporary);
                    return Err(HookError::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = Some(error),
            Err(source) => {
                return Err(HookError::Io {
                    path: temporary,
                    source,
                });
            }
        }
    }
    Err(HookError::Io {
        path: path.to_owned(),
        source: last_error.unwrap_or_else(|| io::Error::other("temporary file collision")),
    })
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> Result<(), HookError> {
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE)).map_err(|source| {
        HookError::Io {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> Result<(), HookError> {
    Ok(())
}

/// Synchronous submit seam used by the hidden `__hook-event` mode.
pub trait HookSubmitter {
    type Error;

    /// Submits one normalized Notification and waits no longer than `ack_timeout`.
    fn submit_and_wait(
        &self,
        notification: NotificationDraft,
        ack_timeout: Duration,
    ) -> Result<(), Self::Error>;
}

/// Receives one provider event. All failures are swallowed after a content-free
/// private log event so provider behavior and output remain unchanged.
pub fn receive_hook_event(
    provider: Provider,
    mut input: impl Read,
    policy: &HookPolicy,
    source: Option<SourceContext>,
    submitter: &impl HookSubmitter,
    logger: Option<&PrivateLogger>,
) {
    let mut bytes = Vec::with_capacity(8 * 1024);
    let read = input
        .by_ref()
        .take((providers::MAX_PROVIDER_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > providers::MAX_PROVIDER_INPUT_BYTES {
        log_hook_failure(logger, LogEvent::HookInputRejected);
        return;
    }
    match providers::normalize(provider, &bytes, policy, source) {
        Ok(Some(notification)) => {
            if submitter
                .submit_and_wait(notification, HOOK_ACK_TIMEOUT)
                .is_err()
            {
                log_hook_failure(logger, LogEvent::HookSubmissionFailed);
            }
        }
        Ok(None) => {}
        Err(_) => log_hook_failure(logger, LogEvent::HookInputRejected),
    }
}

fn log_hook_failure(logger: Option<&PrivateLogger>, event: LogEvent) {
    if let Some(logger) = logger {
        let _ = logger.write(LogLevel::Warning, event);
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("HOME is unavailable")]
    HomeUnavailable,
    #[error("tmnotify executable must be an absolute path ending in tmnotify: {0}")]
    InvalidExecutable(PathBuf),
    #[error("provider executable path is not valid UTF-8: {0}")]
    NonUtf8Executable(PathBuf),
    #[error("{provider:?} does not support {scope:?} hook scope")]
    UnsupportedScope { provider: Provider, scope: Scope },
    #[error("provider configuration exceeds the 1 MiB limit: {0}")]
    TooLarge(PathBuf),
    #[error("provider configuration root must be a JSON object: {0}")]
    RootNotObject(PathBuf),
    #[error("provider hooks have an unsupported JSON shape")]
    InvalidHooksShape,
    #[error("invalid path: {0}")]
    InvalidPath(PathBuf),
    #[error("provider configuration is not valid UTF-8: {0}")]
    InvalidUtf8(PathBuf),
    #[error("provider configuration is a symbolic link: {0}")]
    SymbolicLink(PathBuf),
    #[error("invalid provider JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize provider JSON: {0}")]
    Serialize(serde_json::Error),
    #[error("invalid Codex TOML at {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("Codex inline hooks exist in {0}; pass --allow-mixed to keep them unchanged")]
    MixedCodexHooks(PathBuf),
    #[error("provider configuration IO failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::ffi::OsString;

    fn manager(root: &Path, executable: &Path) -> HookManager {
        let environment =
            Environment::from_pairs([(OsString::from("HOME"), root.join("home").into_os_string())]);
        HookManager::new(executable.to_owned(), root.join("project"), &environment)
            .expect("manager")
    }

    fn minimal() -> HookProviderConfig {
        HookProviderConfig::default()
    }

    #[test]
    fn claude_config_directory_does_not_require_home_discovery() {
        let environment = Environment::from_pairs([(
            OsString::from("CLAUDE_CONFIG_DIR"),
            OsString::from("/custom/claude"),
        )]);
        let manager = HookManager::new(
            PathBuf::from("/opt/bin/tmnotify"),
            PathBuf::from("/project"),
            &environment,
        )
        .expect("manager");
        assert_eq!(
            manager
                .path(Provider::Claude, Scope::User)
                .expect("Claude user path"),
            Path::new("/custom/claude/settings.json")
        );
        assert!(matches!(
            manager.path(Provider::Codex, Scope::User),
            Err(HookError::HomeUnavailable)
        ));
    }

    fn owned_handlers(value: &Value, provider: Provider) -> usize {
        visit_handlers(value)
            .filter(|(_, handler)| owned_executable(handler, provider).is_some())
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn claude_install_is_idempotent_and_preserves_unrelated_configuration() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let executable = temp.path().join("bin/tmnotify");
        let manager = manager(temp.path(), &executable);
        let path = manager
            .path(Provider::Claude, Scope::Project)
            .expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(
            &path,
            "{\r\n    \"permissions\": {\"allow\": [\"Read\"]}\r\n}\r\n",
        )
        .expect("fixture");

        assert_eq!(
            manager
                .install(Provider::Claude, Scope::Project, &minimal(), false)
                .expect("install"),
            Mutation::Changed
        );
        let installed = fs::read(&path).expect("installed file");
        assert!(installed.windows(2).any(|pair| pair == b"\r\n"));
        assert!(installed.ends_with(b"\r\n"));
        let value: Value = serde_json::from_slice(&installed).expect("JSON");
        assert_eq!(value["permissions"]["allow"][0], "Read");
        assert_eq!(owned_handlers(&value, Provider::Claude), 4);
        let status = manager
            .status_with_config(Provider::Claude, &minimal())
            .expect("status");
        let project = status
            .iter()
            .find(|status| status.scope == Scope::Project)
            .expect("project status");
        assert!(project.installed);
        assert!(project.in_sync);
        assert_eq!(project.trust, "unknown — verify in /hooks");

        assert_eq!(
            manager
                .install(Provider::Claude, Scope::Project, &minimal(), false)
                .expect("reinstall"),
            Mutation::Unchanged
        );
        assert_eq!(fs::read(&path).expect("same file"), installed);
        let backup = PathBuf::from(format!("{}.tmnotify.bak", path.display()));
        assert_eq!(
            fs::metadata(backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn sync_updates_relocated_handlers_without_creating_other_scopes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let old_executable = temp.path().join("old/tmnotify");
        let old_manager = manager(temp.path(), &old_executable);
        old_manager
            .install(Provider::Codex, Scope::Project, &minimal(), false)
            .expect("initial install");

        let executable = temp.path().join("new path/it's/tmnotify");
        let manager = manager(temp.path(), &executable);
        let outcomes = manager
            .sync(Provider::Codex, &minimal(), false)
            .expect("sync");
        assert_eq!(outcomes, vec![(Scope::Project, Mutation::Changed)]);
        let project = manager
            .path(Provider::Codex, Scope::Project)
            .expect("project path");
        let value: Value =
            serde_json::from_slice(&fs::read(project).expect("hooks")).expect("valid hooks JSON");
        assert!(installation_is_in_sync(
            &value,
            Provider::Codex,
            &executable,
            &desired_events(Provider::Codex, &minimal())
        ));
        assert!(
            !manager
                .path(Provider::Codex, Scope::User)
                .expect("user path")
                .exists()
        );
    }

    #[test]
    fn remove_deletes_only_owned_handlers() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let manager = manager(temp.path(), &temp.path().join("bin/tmnotify"));
        let path = manager.path(Provider::Codex, Scope::Project).expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(
            &path,
            format!(
                r#"{{
  "hooks": {{
    "Stop": [{{"hooks": [
      {{"type":"command","command":"'{}' __hook-event codex"}},
      {{"type":"command","command":"'{}' __hook-event codex | tee /tmp/x"}},
      {{"type":"command","command":"notify-me"}}
    ]}}]
  }}
}}
"#,
                temp.path().join("bin/tmnotify").display(),
                temp.path().join("bin/tmnotify").display()
            ),
        )
        .expect("fixture");

        assert_eq!(
            manager
                .remove(Provider::Codex, Scope::Project)
                .expect("remove"),
            Mutation::Changed
        );
        let output = fs::read_to_string(&path).expect("result");
        assert!(output.contains("| tee /tmp/x"));
        assert!(output.contains("notify-me"));
        let value: Value = serde_json::from_str(&output).expect("JSON");
        assert_eq!(owned_handlers(&value, Provider::Codex), 0);
        assert_eq!(
            manager
                .remove(Provider::Codex, Scope::Project)
                .expect("repeated remove"),
            Mutation::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&path).expect("byte-identical result"),
            output
        );
    }

    #[test]
    fn codex_inline_hooks_require_explicit_mixed_permission() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let manager = manager(temp.path(), &temp.path().join("bin/tmnotify"));
        let config = temp.path().join("project/.codex/config.toml");
        fs::create_dir_all(config.parent().expect("parent")).expect("directory");
        fs::write(&config, "[hooks]\nStop = [{ command = \"other\" }]\n").expect("TOML");
        assert!(matches!(
            manager.install(Provider::Codex, Scope::Project, &minimal(), false),
            Err(HookError::MixedCodexHooks(path)) if path == config
        ));
        assert!(!temp.path().join("project/.codex/hooks.json").exists());
        assert_eq!(
            manager
                .install(Provider::Codex, Scope::Project, &minimal(), true)
                .expect("allowed mixed install"),
            Mutation::Changed
        );
        assert_eq!(
            fs::read_to_string(config).expect("unchanged TOML"),
            "[hooks]\nStop = [{ command = \"other\" }]\n"
        );
    }

    #[test]
    fn codex_inline_toml_read_is_bounded_and_does_not_follow_symlinks() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let manager = manager(temp.path(), &temp.path().join("bin/tmnotify"));
        let config = temp.path().join("project/.codex/config.toml");
        fs::create_dir_all(config.parent().expect("parent")).expect("directory");
        fs::write(
            &config,
            vec![b'x'; usize::try_from(MAX_PROVIDER_CONFIG_BYTES).unwrap() + 1],
        )
        .expect("oversized TOML");
        assert!(matches!(
            manager.install(Provider::Codex, Scope::Project, &minimal(), false),
            Err(HookError::TooLarge(path)) if path == config
        ));
        assert!(!temp.path().join("project/.codex/hooks.json").exists());

        #[cfg(unix)]
        {
            fs::remove_file(&config).expect("remove oversized fixture");
            let target = temp.path().join("inline.toml");
            fs::write(&target, "[hooks]\n").expect("target");
            std::os::unix::fs::symlink(&target, &config).expect("symlink");
            assert!(matches!(
                manager.install(Provider::Codex, Scope::Project, &minimal(), false),
                Err(HookError::SymbolicLink(path)) if path == config
            ));
            assert_eq!(fs::read_to_string(target).unwrap(), "[hooks]\n");
        }
    }

    #[test]
    fn malformed_json_and_symlinks_are_never_rewritten() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let manager = manager(temp.path(), &temp.path().join("bin/tmnotify"));
        let path = manager
            .path(Provider::Claude, Scope::Project)
            .expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(&path, b"{broken").expect("malformed JSON");
        assert!(matches!(
            manager.install(Provider::Claude, Scope::Project, &minimal(), false),
            Err(HookError::Json { .. })
        ));
        assert_eq!(fs::read(&path).expect("unchanged"), b"{broken");
        assert!(!PathBuf::from(format!("{}.tmnotify.bak", path.display())).exists());

        #[cfg(unix)]
        {
            fs::remove_file(&path).expect("remove fixture");
            let target = temp.path().join("target.json");
            fs::write(&target, "{}").expect("target");
            std::os::unix::fs::symlink(&target, &path).expect("symlink");
            assert!(matches!(
                manager.install(Provider::Claude, Scope::Project, &minimal(), false),
                Err(HookError::SymbolicLink(candidate)) if candidate == path
            ));
            assert_eq!(fs::read_to_string(target).expect("target"), "{}");
        }
    }

    struct RecordingSubmitter {
        calls: Cell<usize>,
        timeout: Cell<Option<Duration>>,
        fail: bool,
    }

    impl HookSubmitter for RecordingSubmitter {
        type Error = ();

        fn submit_and_wait(
            &self,
            _notification: NotificationDraft,
            ack_timeout: Duration,
        ) -> Result<(), Self::Error> {
            self.calls.set(self.calls.get() + 1);
            self.timeout.set(Some(ack_timeout));
            if self.fail { Err(()) } else { Ok(()) }
        }
    }

    #[test]
    fn receiver_submits_synchronously_with_a_two_second_ack_bound() {
        let submitter = RecordingSubmitter {
            calls: Cell::new(0),
            timeout: Cell::new(None),
            fail: false,
        };
        receive_hook_event(
            Provider::Claude,
            br#"{"session_id":"s1","hook_event_name":"Stop","last_assistant_message":"done"}"#
                .as_slice(),
            &HookPolicy::new(crate::config::HookPreset::Minimal),
            None,
            &submitter,
            None,
        );
        assert_eq!(submitter.calls.get(), 1);
        assert_eq!(submitter.timeout.get(), Some(Duration::from_secs(2)));
    }

    #[test]
    fn receiver_swallows_unknown_malformed_oversized_and_submission_failures() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let log_path = temp.path().join("state/tmnotify.log");
        let logger = PrivateLogger::new(log_path.clone()).expect("logger");
        let submitter = RecordingSubmitter {
            calls: Cell::new(0),
            timeout: Cell::new(None),
            fail: true,
        };
        let policy = HookPolicy::new(crate::config::HookPreset::Minimal);
        receive_hook_event(
            Provider::Claude,
            br#"{"session_id":"s1","hook_event_name":"Future"}"#.as_slice(),
            &policy,
            None,
            &submitter,
            Some(&logger),
        );
        receive_hook_event(
            Provider::Claude,
            b"not json".as_slice(),
            &policy,
            None,
            &submitter,
            Some(&logger),
        );
        receive_hook_event(
            Provider::Claude,
            vec![b'x'; providers::MAX_PROVIDER_INPUT_BYTES + 1].as_slice(),
            &policy,
            None,
            &submitter,
            Some(&logger),
        );
        receive_hook_event(
            Provider::Claude,
            br#"{"session_id":"s1","hook_event_name":"Stop"}"#.as_slice(),
            &policy,
            None,
            &submitter,
            Some(&logger),
        );
        assert_eq!(submitter.calls.get(), 1);
        let log = fs::read_to_string(log_path).expect("private log");
        assert_eq!(log.matches("event=hook_input_rejected").count(), 2);
        assert_eq!(log.matches("event=hook_submission_failed").count(), 1);
        assert!(!log.contains("session_id"));
    }
}
