//! Top-level command orchestration that is independent of daemon transport.

use std::io::{self, Write};

use thiserror::Error;

use crate::cli::{
    DoctorArgs, HookAction, HookArgs, HookMutationArgs, HookScopeArg, HookSelectionArgs,
    ProviderArg,
};
use crate::config::HooksConfig;
use crate::doctor::{DoctorOutputError, SystemProbe};
use crate::hooks::{HookError, HookManager, Scope};
use crate::notification::Provider;

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
}
