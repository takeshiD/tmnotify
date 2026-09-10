//! Strict user-level configuration and atomic last-known-good reloads.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use thiserror::Error;

use crate::platform::{Environment, PathError, open_private_read};

const MAX_CONFIG_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub queue: QueueConfig,
    pub toast: ToastConfig,
    pub attention: AttentionConfig,
    pub history: HistoryConfig,
    pub display: DisplayConfig,
    pub hooks: HooksConfig,
}

impl Config {
    fn validate(&self) -> Result<(), ConfigError> {
        bounded("queue.max_pending", self.queue.max_pending, 1, 10_000)?;
        bounded("toast.width", self.toast.width, 24, 500)?;
        bounded("toast.height", self.toast.height, 1, 100)?;
        bounded("toast.max_visible", self.toast.max_visible, 1, 100)?;
        bounded("toast.gap", self.toast.gap, 0, 20)?;
        validate_timeout(
            "toast.timeout",
            self.toast.timeout,
            Duration::from_secs(86_400),
        )?;
        bounded("toast.animation.fps", self.toast.animation.fps, 1, 120)?;
        validate_duration(
            "toast.animation.enter_duration",
            self.toast.animation.enter_duration,
            Duration::from_secs(10),
        )?;
        validate_duration(
            "toast.animation.exit_duration",
            self.toast.animation.exit_duration,
            Duration::from_secs(10),
        )?;
        bounded("attention.width", self.attention.width.0, 1, 100)?;
        bounded(
            "attention.minimum_width",
            self.attention.minimum_width,
            1,
            500,
        )?;
        bounded(
            "attention.minimum_height",
            self.attention.minimum_height,
            1,
            200,
        )?;
        bounded(
            "history.max_entries",
            self.history.max_entries,
            1,
            1_000_000,
        )?;

        for (provider, hooks) in [("claude", &self.hooks.claude), ("codex", &self.hooks.codex)] {
            for event in &hooks.enable {
                if hooks.disable.contains(event) {
                    return Err(ConfigError::ConflictingHookEvent {
                        provider,
                        event: *event,
                    });
                }
            }
        }
        Ok(())
    }
}

fn bounded(field: &'static str, value: u32, minimum: u32, maximum: u32) -> Result<(), ConfigError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::OutOfRange {
            field,
            minimum,
            maximum,
        })
    }
}

fn validate_duration(
    field: &'static str,
    value: DurationValue,
    maximum: Duration,
) -> Result<(), ConfigError> {
    if value.0 <= maximum {
        Ok(())
    } else {
        Err(ConfigError::DurationOutOfRange { field, maximum })
    }
}

fn validate_timeout(
    field: &'static str,
    value: TimeoutValue,
    maximum: Duration,
) -> Result<(), ConfigError> {
    match value {
        TimeoutValue::Never => Ok(()),
        TimeoutValue::After(duration) => validate_duration(field, duration, maximum),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub idle_timeout: IdleTimeout,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout: IdleTimeout::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdleTimeout {
    Never,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub max_pending: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self { max_pending: 1_000 }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToastConfig {
    pub position: Placement,
    pub width: u32,
    pub height: u32,
    pub timeout: TimeoutValue,
    pub max_visible: u32,
    pub gap: u32,
    pub stack_order: StackOrder,
    pub body: BodyPresentation,
    pub animation: AnimationConfig,
}

impl Default for ToastConfig {
    fn default() -> Self {
        Self {
            position: Placement::TopRight,
            width: 42,
            height: 3,
            timeout: TimeoutValue::After(DurationValue(Duration::from_secs(3))),
            max_visible: 4,
            gap: 1,
            stack_order: StackOrder::OldestFirst,
            body: BodyPresentation::FirstLine,
            animation: AnimationConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StackOrder {
    OldestFirst,
    NewestFirst,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BodyPresentation {
    FirstLine,
    JoinLines,
    Wrap,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnimationConfig {
    pub enabled: bool,
    pub fps: u32,
    pub enter_duration: DurationValue,
    pub exit_duration: DurationValue,
    pub enter_easing: Easing,
    pub exit_easing: Easing,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fps: 20,
            enter_duration: DurationValue(Duration::from_millis(180)),
            exit_duration: DurationValue(Duration::from_millis(150)),
            enter_easing: Easing::EaseOut,
            exit_easing: Easing::EaseIn,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Easing {
    EaseIn,
    EaseOut,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttentionConfig {
    pub width: Percentage,
    pub minimum_width: u32,
    pub minimum_height: u32,
    pub capture_all_keys: bool,
    pub close_on_outside_click: bool,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        Self {
            width: Percentage(60),
            minimum_width: 32,
            minimum_height: 7,
            capture_all_keys: true,
            close_on_outside_click: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_entries: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 10_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    pub color: FeatureMode,
    pub unicode: FeatureMode,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            color: FeatureMode::Auto,
            unicode: FeatureMode::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub claude: HookProviderConfig,
    pub codex: HookProviderConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookProviderConfig {
    pub preset: HookPreset,
    pub enable: Vec<HookEvent>,
    pub disable: Vec<HookEvent>,
}

impl Default for HookProviderConfig {
    fn default() -> Self {
        Self {
            preset: HookPreset::Minimal,
            enable: Vec::new(),
            disable: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookPreset {
    Minimal,
    Normal,
    Verbose,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    NeedsAttention,
    Completed,
    Failed,
    Started,
    Interrupted,
    SubagentCompleted,
    ToolStarted,
    ToolCompleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationValue(pub Duration);

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_duration(&value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for DurationValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format_duration(self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutValue {
    After(DurationValue),
    Never,
}

impl<'de> Deserialize<'de> for TimeoutValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "never" {
            Ok(Self::Never)
        } else {
            parse_duration(&value)
                .map(DurationValue)
                .map(Self::After)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl Serialize for TimeoutValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::After(duration) => duration.serialize(serializer),
            Self::Never => serializer.serialize_str("never"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Percentage(pub u32);

impl<'de> Deserialize<'de> for Percentage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let number = value
            .strip_suffix('%')
            .ok_or_else(|| serde::de::Error::custom("percentage must end in %"))?
            .parse()
            .map_err(serde::de::Error::custom)?;
        Ok(Self(number))
    }
}

impl Serialize for Percentage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("{}%", self.0))
    }
}

fn format_duration(duration: Duration) -> String {
    let milliseconds = duration.as_millis();
    for (unit, divisor) in [("h", 3_600_000_u128), ("m", 60_000), ("s", 1_000)] {
        if milliseconds != 0 && milliseconds.is_multiple_of(divisor) {
            return format!("{}{unit}", milliseconds / divisor);
        }
    }
    format!("{milliseconds}ms")
}

fn parse_duration(value: &str) -> Result<Duration, &'static str> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        return Err("duration must use ms, s, m, or h");
    };
    let number: u64 = number.parse().map_err(|_| "duration must be an integer")?;
    let milliseconds = number
        .checked_mul(multiplier)
        .ok_or("duration is too large")?;
    Ok(Duration::from_millis(milliseconds))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigOverrides {
    pub color: Option<FeatureMode>,
    pub unicode: Option<FeatureMode>,
    pub animation_enabled: Option<bool>,
}

fn apply_precedence(config: &mut Config, environment: &Environment, cli: ConfigOverrides) {
    if environment.contains("NO_COLOR") || environment.get("TERM") == Some("dumb".as_ref()) {
        config.display.color = FeatureMode::Never;
    }
    if environment.get("TERM") == Some("dumb".as_ref()) {
        config.display.unicode = FeatureMode::Never;
        config.toast.animation.enabled = false;
    }
    if let Some(color) = cli.color {
        config.display.color = color;
    }
    if let Some(unicode) = cli.unicode {
        config.display.unicode = unicode;
    }
    if let Some(enabled) = cli.animation_enabled {
        config.toast.animation.enabled = enabled;
    }
}

pub fn load(
    path: &Path,
    environment: &Environment,
    cli: ConfigOverrides,
) -> Result<Config, ConfigError> {
    let mut config = match open_private_read(path) {
        Ok(file) => {
            let metadata = file.metadata().map_err(|source| ConfigError::Io {
                path: path.to_owned(),
                source,
            })?;
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(ConfigError::TooLarge);
            }
            let mut contents = String::new();
            file.take(MAX_CONFIG_BYTES + 1)
                .read_to_string(&mut contents)
                .map_err(|source| ConfigError::Io {
                    path: path.to_owned(),
                    source,
                })?;
            if contents.len() as u64 > MAX_CONFIG_BYTES {
                return Err(ConfigError::TooLarge);
            }
            toml::from_str(&contents).map_err(ConfigError::Parse)?
        }
        Err(PathError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Config::default()
        }
        Err(error) => return Err(ConfigError::PrivatePath(error)),
    };
    apply_precedence(&mut config, environment, cli);
    config.validate()?;
    Ok(config)
}

/// Serializes a complete effective configuration in declaration order.
///
/// The output is intentionally canonical rather than preserving user spelling,
/// comments, or table order, so scripts receive the same TOML for equivalent
/// validated configurations.
pub fn to_stable_toml(config: &Config) -> Result<String, ConfigError> {
    toml::to_string_pretty(config).map_err(ConfigError::Serialize)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigChanges {
    pub daemon_or_queue: bool,
    pub display_reconciliation: bool,
    pub history: bool,
    /// Hook configuration is loaded, but installed provider files remain
    /// unchanged until the caller performs an explicit `hook sync`.
    pub hooks_require_sync: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReloadOutcome {
    Unchanged,
    Reloaded(ConfigChanges),
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileFingerprint {
    modified: Option<SystemTime>,
    length: u64,
    identity: (u64, u64),
}

pub struct ConfigManager {
    path: PathBuf,
    environment: Environment,
    cli: ConfigOverrides,
    active: Config,
    observed: Option<FileFingerprint>,
}

impl ConfigManager {
    pub fn new(
        path: PathBuf,
        environment: Environment,
        cli: ConfigOverrides,
    ) -> Result<Self, ConfigError> {
        let active = load(&path, &environment, cli)?;
        let observed = fingerprint(&path)?;
        Ok(Self {
            path,
            environment,
            cli,
            active,
            observed,
        })
    }

    pub fn active(&self) -> &Config {
        &self.active
    }

    pub fn reload_if_changed(&mut self) -> Result<ReloadOutcome, ConfigError> {
        let observed = fingerprint(&self.path)?;
        if observed == self.observed {
            return Ok(ReloadOutcome::Unchanged);
        }
        self.observed = observed;

        let candidate = match load(&self.path, &self.environment, self.cli) {
            Ok(candidate) => candidate,
            Err(_) => return Ok(ReloadOutcome::Rejected),
        };
        Ok(self.replace(candidate))
    }

    /// Re-reads the configured path regardless of its last observed metadata.
    /// A failed load leaves the active last-known-good snapshot untouched.
    pub fn reload_now(&mut self) -> Result<ReloadOutcome, ConfigError> {
        self.observed = fingerprint(&self.path)?;
        let candidate = load(&self.path, &self.environment, self.cli)?;
        Ok(self.replace(candidate))
    }

    fn replace(&mut self, candidate: Config) -> ReloadOutcome {
        if candidate == self.active {
            return ReloadOutcome::Unchanged;
        }
        let changes = ConfigChanges {
            daemon_or_queue: candidate.daemon != self.active.daemon
                || candidate.queue != self.active.queue,
            display_reconciliation: candidate.toast != self.active.toast
                || candidate.attention != self.active.attention
                || candidate.display != self.active.display,
            history: candidate.history != self.active.history,
            hooks_require_sync: candidate.hooks != self.active.hooks,
        };
        self.active = candidate;
        ReloadOutcome::Reloaded(changes)
    }
}

fn fingerprint(path: &Path) -> Result<Option<FileFingerprint>, ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(FileFingerprint {
            modified: metadata.modified().ok(),
            length: metadata.len(),
            identity: file_identity(&metadata),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file exceeds 256 KiB")]
    TooLarge,
    #[error("invalid configuration: {0}")]
    Parse(toml::de::Error),
    #[error("failed to serialize effective configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error("invalid private configuration path: {0}")]
    PrivatePath(PathError),
    #[error("{field} must be between {minimum} and {maximum}")]
    OutOfRange {
        field: &'static str,
        minimum: u32,
        maximum: u32,
    },
    #[error("{field} must not exceed {maximum:?}")]
    DurationOutOfRange {
        field: &'static str,
        maximum: Duration,
    },
    #[error("hooks.{provider} event {event:?} cannot be both enabled and disabled")]
    ConflictingHookEvent {
        provider: &'static str,
        event: HookEvent,
    },
    #[error("configuration IO failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    fn empty_environment() -> Environment {
        Environment::from_pairs(std::iter::empty::<(OsString, OsString)>())
    }

    fn write_config(path: &Path, contents: &str) {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).expect("config file");
        file.write_all(contents.as_bytes())
            .expect("config contents");
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("permissions");
    }

    #[test]
    fn loads_complete_defaults_when_user_config_is_absent() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let config = load(
            &temp.path().join("missing.toml"),
            &empty_environment(),
            ConfigOverrides::default(),
        )
        .expect("defaults");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn stable_toml_is_complete_deterministic_and_round_trips() {
        let config = Config::default();
        let first = to_stable_toml(&config).expect("TOML");
        let second = to_stable_toml(&config).expect("TOML");
        assert_eq!(first, second);
        assert_eq!(toml::from_str::<Config>(&first).unwrap(), config);
        for table in [
            "[daemon]",
            "[queue]",
            "[toast]",
            "[toast.animation]",
            "[attention]",
            "[history]",
            "[display]",
            "[hooks.claude]",
            "[hooks.codex]",
        ] {
            assert!(first.contains(table), "missing {table} in {first}");
        }
        assert!(first.contains("timeout = \"3s\""));
        assert!(first.contains("enter_duration = \"180ms\""));
        assert!(first.ends_with('\n'));
    }

    #[test]
    fn rejects_unknown_fields_and_bounded_values() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        write_config(&path, "[toast]\nunknown = true\n");
        assert!(matches!(
            load(&path, &empty_environment(), Default::default()),
            Err(ConfigError::Parse(_))
        ));

        write_config(&path, "[queue]\nmax_pending = 10001\n");
        assert!(matches!(
            load(&path, &empty_environment(), Default::default()),
            Err(ConfigError::OutOfRange {
                field: "queue.max_pending",
                ..
            })
        ));
    }

    #[test]
    fn precedence_is_cli_then_environment_then_file_then_defaults() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        write_config(
            &path,
            "[display]\ncolor = \"always\"\nunicode = \"always\"\n\n[toast.animation]\nenabled = true\n",
        );
        let environment = Environment::from_pairs([
            (OsString::from("NO_COLOR"), OsString::new()),
            (OsString::from("TERM"), OsString::from("dumb")),
        ]);
        let config = load(
            &path,
            &environment,
            ConfigOverrides {
                color: Some(FeatureMode::Always),
                unicode: None,
                animation_enabled: Some(true),
            },
        )
        .expect("config");
        assert_eq!(config.display.color, FeatureMode::Always);
        assert_eq!(config.display.unicode, FeatureMode::Never);
        assert!(config.toast.animation.enabled);
    }

    #[test]
    fn invalid_reload_preserves_last_known_good_snapshot() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        write_config(&path, "[toast]\nwidth = 50\n");
        let mut manager = ConfigManager::new(
            path.clone(),
            empty_environment(),
            ConfigOverrides::default(),
        )
        .expect("manager");
        assert_eq!(manager.active().toast.width, 50);

        write_config(&path, "[toast]\nwidth = 2\n# make fingerprint longer\n");
        assert_eq!(
            manager.reload_if_changed().expect("reload"),
            ReloadOutcome::Rejected
        );
        assert_eq!(manager.active().toast.width, 50);
        assert!(manager.reload_now().is_err());
        assert_eq!(manager.active().toast.width, 50);
    }

    #[test]
    fn reload_separates_display_reconciliation_from_hook_sync() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        write_config(&path, "");
        let mut manager = ConfigManager::new(
            path.clone(),
            empty_environment(),
            ConfigOverrides::default(),
        )
        .expect("manager");
        write_config(
            &path,
            "[toast]\nposition = \"bottom-left\"\n\n[hooks.codex]\npreset = \"normal\"\n",
        );
        let ReloadOutcome::Reloaded(changes) = manager.reload_if_changed().expect("reload") else {
            panic!("expected reload");
        };
        assert!(changes.display_reconciliation);
        assert!(changes.hooks_require_sync);
        assert!(!changes.history);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_user_configuration() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        write_config(&path, "");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            load(&path, &empty_environment(), Default::default()),
            Err(ConfigError::PrivatePath(PathError::InsecurePermissions(_)))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_user_configuration() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let target = temp.path().join("target.toml");
        let path = temp.path().join("config.toml");
        write_config(&target, "");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        assert!(matches!(
            load(&path, &empty_environment(), Default::default()),
            Err(ConfigError::PrivatePath(PathError::SymbolicLink(_)))
        ));
    }
}
