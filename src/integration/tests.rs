use super::command::*;
use super::config_edit::*;
use super::env::*;
use super::file_ops::*;
use super::registry::*;
use super::targets::*;
#[cfg(windows)]
use super::test_support::symlink_file;
use super::types::*;
use super::version::*;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

#[test]
fn windows_powershell_encoded_hook_command_preserves_script_invocation() {
    use base64::Engine;

    let hook_path = Path::new(r"C:\Users\O'Neil λ\App Data\hooks\herdr-agent-state.ps1");
    let command = powershell_encoded_hook_command(hook_path, "session");
    let encoded = command
        .strip_prefix("powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand ")
        .expect("encoded PowerShell command");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("base64 payload");
    let mut chunks = bytes.chunks_exact(2);
    let utf16 = chunks
        .by_ref()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    assert!(chunks.remainder().is_empty(), "UTF-16LE payload");
    assert_eq!(
        String::from_utf16(&utf16).expect("PowerShell script"),
        r"& 'C:\Users\O''Neil λ\App Data\hooks\herdr-agent-state.ps1' session"
    );
}

#[cfg(windows)]
#[test]
fn windows_antigravity_cli_hook_command_uses_encoded_powershell() {
    let hook_path = Path::new(r"C:\Users\reporter\.gemini\config\hooks\herdr-agent-state.ps1");
    assert_eq!(
        antigravity_cli_hook_command(hook_path, "session"),
        powershell_encoded_hook_command(hook_path, "session")
    );
}

#[test]
fn extract_version_triple_parses_common_outputs() {
    assert_eq!(extract_version_triple("0.14.0"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(
        extract_version_triple("kimi-code 0.14.0 (linux/x64)"),
        Some((0, 14, 0))
    );
    assert_eq!(extract_version_triple("0.14"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("0.14.1-beta.2"), Some((0, 14, 1)));
    assert_eq!(extract_version_triple("no version here"), None);
    assert_eq!(extract_version_triple(""), None);
}

#[test]
fn extract_version_triple_orders_versions() {
    let old = extract_version_triple("0.12.1").unwrap();
    let min = extract_version_triple(KIMI_MIN_VERSION).unwrap();
    let new = extract_version_triple("0.15.0").unwrap();
    assert!(old < min);
    assert!(min <= min);
    assert!(min < new);
}

#[test]
fn agent_version_requirement_only_set_for_kimi() {
    let requirement = agent_version_requirement(crate::api::schema::IntegrationTarget::Kimi)
        .expect("kimi must have a version requirement");
    assert_eq!(requirement.binary, "kimi");
    assert_eq!(requirement.min_version, KIMI_MIN_VERSION);
    assert!(agent_version_requirement(crate::api::schema::IntegrationTarget::Claude).is_none());
    assert!(agent_version_requirement(crate::api::schema::IntegrationTarget::Codex).is_none());
}

#[test]
fn enforce_agent_version_warns_when_binary_missing() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "herdr-test-binary-that-does-not-exist",
        args: &["--version"],
        min_version: "0.14.0",
    };
    let warning = enforce_agent_version(&requirement)
        .expect("missing binary must not fail the install")
        .expect("missing binary must produce a warning");
    assert!(warning.contains("could not run"));
    assert!(warning.contains("0.14.0"));
}

#[cfg(unix)]
#[test]
fn enforce_agent_version_rejects_old_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.12.1"],
        min_version: "0.14.0",
    };
    let err = enforce_agent_version(&requirement).expect_err("old version must fail the install");
    let message = err.to_string();
    assert!(message.contains("0.12.1"));
    assert!(message.contains("0.14.0"));
    assert!(message.contains("upgrade"));
}

#[cfg(unix)]
#[test]
fn enforce_agent_version_accepts_current_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.14.0"],
        min_version: "0.14.0",
    };
    let result =
        enforce_agent_version(&requirement).expect("matching version must not fail the install");
    assert!(result.is_none(), "matching version must not warn");
}

fn clear_integration_path_env() {
    std::env::remove_var(PI_CODING_AGENT_DIR_ENV_VAR);
    std::env::remove_var(OMP_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(CLAUDE_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(CODEX_HOME_ENV_VAR);
    std::env::remove_var(COPILOT_HOME_ENV_VAR);
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_STATE_HOME");
    #[cfg(windows)]
    std::env::remove_var("APPDATA");
    std::env::remove_var(QODERCLI_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(QWEN_HOME_ENV_VAR);
    std::env::remove_var(CURSOR_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(ANTIGRAVITY_CLI_SETTINGS_DIR_ENV_VAR);
    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(GROK_HOME_ENV_VAR);
}

fn kimi_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

fn kimi_config_hooks(config: &str) -> Vec<toml::Value> {
    let parsed: toml::Value = toml::from_str(config).unwrap();
    parsed
        .get("hooks")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn assert_kimi_hook(
    config: &str,
    hook_path: &Path,
    event: &str,
    matcher: Option<&str>,
    action: &str,
) {
    let command = kimi_hook_command(hook_path, action);
    let hooks = kimi_config_hooks(config);
    assert!(
        hooks.iter().any(|hook| {
            hook.get("event").and_then(toml::Value::as_str) == Some(event)
                && hook.get("matcher").and_then(toml::Value::as_str) == matcher
                && hook.get("command").and_then(toml::Value::as_str) == Some(command.as_str())
                && hook.get("timeout").and_then(toml::Value::as_integer) == Some(10)
        }),
        "missing kimi hook for {event} ({matcher:?}) -> {action}"
    );
}

fn unique_base() -> PathBuf {
    clear_integration_path_env();
    std::env::temp_dir().join(format!(
        "herdr-integration-install-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(windows)]
#[test]
fn home_dir_uses_userprofile_when_home_is_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let previous_home = std::env::var_os("HOME");
    let previous_userprofile = std::env::var_os("USERPROFILE");
    std::env::remove_var("HOME");
    std::env::set_var("USERPROFILE", &base);

    assert_eq!(home_dir().unwrap(), base);

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    }
    if let Some(userprofile) = previous_userprofile {
        std::env::set_var("USERPROFILE", userprofile);
    } else {
        std::env::remove_var("USERPROFILE");
    }
}

#[cfg(windows)]
#[test]
fn windows_devin_dir_uses_appdata_without_xdg_override() {
    let previous_appdata;
    {
        let _lock = integration_env_lock();
        previous_appdata = std::env::var_os("APPDATA");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let base = unique_base();
        let appdata = base.join("appdata");
        let xdg = base.join("xdg");
        std::env::set_var("APPDATA", &appdata);

        assert_eq!(devin_dir().unwrap(), appdata.join("devin"));

        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        assert_eq!(devin_dir().unwrap(), xdg.join("devin"));

        if let Some(value) = previous_xdg {
            std::env::set_var("XDG_CONFIG_HOME", value);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }
    {
        let _lock = integration_env_lock();
        assert_eq!(std::env::var_os("APPDATA"), previous_appdata);
    }
}

#[cfg(windows)]
#[test]
fn windows_supports_portable_integrations() {
    use crate::api::schema::IntegrationTarget;

    assert!(integration_target_supported(IntegrationTarget::Hermes));
    assert!(integration_target_supported(IntegrationTarget::Cursor));
    assert!(integration_target_supported(IntegrationTarget::Devin));
    assert!(integration_target_supported(IntegrationTarget::Mastracode));
    assert!(integration_target_supported(IntegrationTarget::Grok));

    assert!(integration_target_supported(IntegrationTarget::Pi));
    assert!(integration_target_supported(IntegrationTarget::Omp));
    assert!(integration_target_supported(IntegrationTarget::Claude));
    assert!(integration_target_supported(IntegrationTarget::Codex));
    assert!(integration_target_supported(IntegrationTarget::Copilot));
    assert!(integration_target_supported(IntegrationTarget::Opencode));
    assert!(integration_target_supported(IntegrationTarget::Kilo));
    assert!(integration_target_supported(IntegrationTarget::Droid));
    assert!(integration_target_supported(IntegrationTarget::Kimi));
    assert!(integration_target_supported(IntegrationTarget::Qodercli));
    assert!(integration_target_supported(IntegrationTarget::Qwen));
}

#[cfg(windows)]
#[test]
fn windows_availability_includes_native_integrations() {
    use crate::api::schema::IntegrationTarget;

    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    fs::write(bin.join("pi.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("omp.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("opencode.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("kilo.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("hermes.exe"), "").unwrap();
    fs::write(bin.join("cursor-agent.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("devin.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("mastracode.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("grok.cmd"), "@echo off\r\n").unwrap();

    assert!(integration_target_available(IntegrationTarget::Pi));
    assert!(integration_target_available(IntegrationTarget::Omp));
    assert!(integration_target_available(IntegrationTarget::Opencode));
    assert!(integration_target_available(IntegrationTarget::Kilo));
    assert!(integration_target_available(IntegrationTarget::Hermes));
    assert!(integration_target_available(IntegrationTarget::Cursor));
    assert!(integration_target_available(IntegrationTarget::Devin));
    assert!(integration_target_available(IntegrationTarget::Mastracode));
    assert!(integration_target_available(IntegrationTarget::Grok));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(unix)]
fn command_available_requires_executable_file_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    let command = bin.join("claude");
    fs::write(&command, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!command_available("claude"));

    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(command_available("claude"));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn command_available_finds_windows_command_shims_on_path() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    fs::write(bin.join("claude.cmd"), "@echo off\r\n").unwrap();
    assert!(command_available("claude"));

    fs::write(bin.join("codex.exe"), "").unwrap();
    assert!(command_available("codex"));

    assert!(!command_available("missing-agent"));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn qodercli_availability_checks_windows_aliases() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    fs::write(bin.join("qoder.cmd"), "@echo off\r\n").unwrap();

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Qodercli
    ));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn hermes_layout_makes_target_available() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let local_app_data = base.join("local-app-data");
    let hermes_bin = local_app_data.join("hermes").join("bin");
    fs::create_dir_all(&hermes_bin).unwrap();
    fs::write(hermes_bin.join("hermes.exe"), "").unwrap();
    let original_hermes_home = std::env::var_os(HERMES_HOME_ENV_VAR);
    let original_home = std::env::var_os("HOME");
    let original_local_app_data = std::env::var_os("LOCALAPPDATA");
    let original_path = std::env::var_os("PATH");
    std::env::remove_var(HERMES_HOME_ENV_VAR);
    std::env::remove_var("HOME");
    std::env::set_var("LOCALAPPDATA", &local_app_data);
    std::env::set_var("PATH", "");

    assert!(hermes_install_layout_available());
    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Hermes
    ));

    if let Some(hermes_home) = original_hermes_home {
        std::env::set_var(HERMES_HOME_ENV_VAR, hermes_home);
    } else {
        std::env::remove_var(HERMES_HOME_ENV_VAR);
    }
    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(local_app_data) = original_local_app_data {
        std::env::set_var("LOCALAPPDATA", local_app_data);
    } else {
        std::env::remove_var("LOCALAPPDATA");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_availability_finds_standalone_binary_under_codex_home() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let bin = home
        .join(".codex/packages/standalone/releases/0.137.0-test")
        .join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(codex_executable_name());
    fs::write(&binary, "").unwrap();
    make_executable(&binary).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn integration_recommendations_mark_standalone_codex_available() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let bin = home
        .join(".codex/packages/standalone/releases/0.137.0-test")
        .join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(codex_executable_name());
    fs::write(&binary, "").unwrap();
    make_executable(&binary).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");

    let codex = integration_recommendations()
        .into_iter()
        .find(|recommendation| {
            recommendation.target == crate::api::schema::IntegrationTarget::Codex
        })
        .expect("codex recommendation should be present");

    assert!(codex.available);
    assert_eq!(codex.state, IntegrationStatusKind::NotInstalled);
    assert!(codex.needs_install());

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn integration_recommendation_installs_available_or_outdated_targets() {
    let mut recommendation = IntegrationRecommendation {
        target: crate::api::schema::IntegrationTarget::Claude,
        label: "claude",
        command: "claude",
        available: false,
        path: PathBuf::from("/tmp/herdr-agent-state.sh"),
        state: IntegrationStatusKind::NotInstalled,
    };
    assert!(!recommendation.needs_install());

    recommendation.available = true;
    assert!(recommendation.needs_install());

    recommendation.available = false;
    recommendation.state = IntegrationStatusKind::Outdated;
    assert!(recommendation.needs_install());

    recommendation.available = true;
    recommendation.state = IntegrationStatusKind::Current;
    assert!(!recommendation.needs_install());
}

#[test]
fn install_pi_writes_embedded_asset_to_pi_extensions_dir() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);

    let path = install_pi().unwrap();
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
    assert_eq!(content, PI_EXTENSION_ASSET);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_creates_extensions_dir_when_agent_dir_exists() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).unwrap();
    std::env::set_var("HOME", &home);

    let path = install_pi().unwrap();

    assert_eq!(
        path,
        agent_dir.join("extensions").join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(path.is_file());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_uses_pi_coding_agent_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agent_dir = base.join("custom-pi-agent");
    let ext_dir = agent_dir.join("extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, &agent_dir);

    let path = install_pi().unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_expands_tilde_in_pi_coding_agent_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join("custom-pi-agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, "~/custom-pi-agent");

    let path = install_pi().unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    std::env::remove_var("HOME");
    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_writes_embedded_asset_to_omp_extensions_dir() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_omp().unwrap();
    let content = fs::read_to_string(&installed.extension_path).unwrap();

    assert_eq!(
        installed.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(!installed.removed_legacy_pi_extension);
    assert_eq!(content, OMP_EXTENSION_ASSET);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_removes_legacy_pi_integration_from_omp_extensions_dir() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let legacy_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&legacy_path, PI_EXTENSION_ASSET).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_omp().unwrap();

    assert_eq!(
        installed.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(installed.removed_legacy_pi_extension);
    assert!(!legacy_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_preserves_non_herdr_file_with_pi_install_name() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let user_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&user_path, "// user extension\n").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_omp().unwrap();

    assert_eq!(
        installed.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(!installed.removed_legacy_pi_extension);
    assert_eq!(
        fs::read_to_string(user_path).unwrap(),
        "// user extension\n"
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_uses_pi_config_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join("custom-omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var(OMP_CONFIG_DIR_ENV_VAR, "custom-omp");

    let installed = install_omp().unwrap();

    assert_eq!(
        installed.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(!installed.removed_legacy_pi_extension);

    std::env::remove_var("HOME");
    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_refuses_shared_pi_extension_directory() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agent_dir = base.join("shared-agent");
    let ext_dir = agent_dir.join("extensions");
    let pi_extension = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(&pi_extension, PI_EXTENSION_ASSET).unwrap();
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, &agent_dir);
    std::env::set_var(OMP_CONFIG_DIR_ENV_VAR, "ignored-omp-config");

    let err = install_omp().unwrap_err().to_string();

    assert!(err.contains("Pi and OMP resolve to the same extension directory"));
    assert!(err.contains(&ext_dir.display().to_string()));
    assert!(pi_extension.is_file());
    assert!(!ext_dir.join(OMP_EXTENSION_INSTALL_NAME).exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_creates_extensions_dir_when_agent_dir_exists() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let agent_dir = home.join(".omp/agent");
    let ext_dir = agent_dir.join("extensions");
    fs::create_dir_all(&agent_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_omp().unwrap();

    assert_eq!(
        installed.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(ext_dir.is_dir());
    assert!(!installed.removed_legacy_pi_extension);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_omp_removes_embedded_extension_when_present() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME),
        OMP_EXTENSION_ASSET,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_omp().unwrap();

    assert_eq!(
        result.extension_path,
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(result.removed_extension);
    assert!(!result.extension_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_omp_errors_when_extension_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_omp().unwrap_err().to_string();

    assert!(err.contains("omp extension directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_pi_removes_embedded_extension_when_present() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET).unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_pi().unwrap();

    assert_eq!(
        result.extension_path,
        ext_dir.join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(result.removed_extension);
    assert!(!result.extension_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_treat_missing_version_marker_as_legacy() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&extension_path, "// installed by herdr\n").unwrap();
    std::env::set_var("HOME", &home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, None);
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_detect_previous_pi_version() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(
        &extension_path,
        "// HERDR_INTEGRATION_ID=pi\n// HERDR_INTEGRATION_VERSION=4\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, Some(4));
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_detect_previous_omp_version() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let extension_path = ext_dir.join(OMP_EXTENSION_INSTALL_NAME);
    fs::write(
        &extension_path,
        "// HERDR_INTEGRATION_ID=omp\n// HERDR_INTEGRATION_VERSION=4\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Omp
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, Some(4));
    assert_eq!(outdated[0].expected_version, OMP_INTEGRATION_VERSION);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_accept_current_version_marker() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET).unwrap();
    std::env::set_var("HOME", &home);

    assert!(outdated_installed_integrations().is_empty());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_errors_when_extension_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_pi().unwrap_err().to_string();

    assert!(err.contains("pi extension directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_writes_hook_and_updates_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_claude().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();

    assert_eq!(
        installed.hook_path,
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );
    assert_eq!(hook_content, CLAUDE_HOOK_ASSET);
    assert!(settings["permissions"]["allow"].is_array());
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["matcher"],
        "^(startup|resume|clear|compact|fork)$"
    );
    assert!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" session"));
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());
    for event in ["PostToolUse", "Stop"] {
        assert_eq!(settings["hooks"][event][0]["hooks"][0]["async"], true);
        assert!(settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(" cache"));
    }
    assert!(settings.get("statusLine").is_none());
    if cfg!(not(windows)) {
        assert!(claude_dir
            .join("hooks")
            .join("herdr-statusline-tap.sh")
            .is_file());
    }

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_claude_wraps_existing_statusline_and_uninstall_restores_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let settings_path = claude_dir.join("settings.json");
    let user_statusline = json!({
        "type": "command",
        "command": "bash ~/.claude/tokenline.sh",
        "padding": 0
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({"statusLine": user_statusline})).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);
    let tap_path = claude_dir.join("hooks").join("herdr-statusline-tap.sh");

    install_claude().unwrap();
    let installed_bytes = fs::read_to_string(&settings_path).unwrap();
    let settings: Value = serde_json::from_str(&installed_bytes).unwrap();
    let command = settings["statusLine"]["command"].as_str().unwrap();

    assert_eq!(
        super::statusline_tap::unwrap_statusline_command(command),
        Some("bash ~/.claude/tokenline.sh".to_string())
    );
    assert!(
        command.contains(&tap_path.display().to_string()),
        "{command}"
    );
    assert_eq!(settings["statusLine"]["padding"], 0);
    assert!(tap_path.is_file());

    install_claude().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), installed_bytes);

    uninstall_claude().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();

    assert_eq!(settings["statusLine"], user_statusline);
    assert!(!tap_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[cfg(windows)]
#[test]
fn install_claude_leaves_statusline_untouched_on_windows() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let settings_path = claude_dir.join("settings.json");
    let user_statusline = json!({
        "type": "command",
        "command": "powershell -File C:\\Users\\u\\statusline.ps1"
    });
    fs::write(
        &settings_path,
        serde_json::to_string(&json!({"statusLine": user_statusline})).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_claude().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();

    assert_eq!(settings["statusLine"], user_statusline);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_uses_claude_config_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let claude_dir = base.join("custom-claude");
    fs::create_dir_all(&claude_dir).unwrap();
    std::env::set_var(CLAUDE_CONFIG_DIR_ENV_VAR, &claude_dir);

    let installed = install_claude().unwrap();

    assert_eq!(installed.settings_path, claude_dir.join("settings.json"));
    assert_eq!(
        installed.hook_path,
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    std::env::set_var("HOME", &home);

    install_claude().unwrap();
    install_claude().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"].as_array().unwrap().len(),
        1
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());
    for event in ["PostToolUse", "Stop"] {
        assert_eq!(settings["hooks"][event][0]["hooks"][0]["async"], true);
        assert!(settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(" cache"));
    }

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_removes_deprecated_completion_hooks_and_preserves_user_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    let settings = serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-post", "timeout": 10}
                ]
            }],
            "PostToolUseFailure": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-failure", "timeout": 10}
                ]
            }],
            "SubagentStop": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-subagent", "timeout": 10}
                ]
            }],
            "SessionEnd": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' release", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-session-end", "timeout": 10}
                ]
            }]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_claude().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "echo keep-post"
    );
    assert_eq!(
        settings["hooks"]["PostToolUseFailure"][0]["hooks"][0]["command"],
        "echo keep-failure"
    );
    assert_eq!(
        settings["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
        "echo keep-subagent"
    );
    assert_eq!(
        settings["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
        "echo keep-session-end"
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["async"], true);
    assert!(settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" cache"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_v9_integration_status_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&claude_hooks_dir).unwrap();
    let hook_path = claude_hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=claude\n# HERDR_INTEGRATION_VERSION=9\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(9));
    assert_eq!(claude.expected_version, 12);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

    install_claude().unwrap();
    let status = integration_status_at(
        crate::api::schema::IntegrationTarget::Claude,
        hook_path,
        CLAUDE_INTEGRATION_VERSION,
    );
    assert_eq!(status.installed_version, Some(12));
    assert_eq!(status.state, IntegrationStatusKind::Current);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_v10_integration_status_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&claude_hooks_dir).unwrap();
    let hook_path = claude_hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=claude\n# HERDR_INTEGRATION_VERSION=10\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(10));
    assert_eq!(claude.expected_version, 12);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

    install_claude().unwrap();
    let status = integration_status_at(
        crate::api::schema::IntegrationTarget::Claude,
        hook_path,
        CLAUDE_INTEGRATION_VERSION,
    );
    assert_eq!(status.installed_version, Some(12));
    assert_eq!(status.state, IntegrationStatusKind::Current);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_v2_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&claude_hooks_dir).unwrap();
    let hook_path = claude_hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=claude\n# HERDR_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(2));
    assert_eq!(claude.expected_version, 12);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_claude_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CLAUDE_HOOK_ASSET).unwrap();
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]
            }],
            "UserPromptSubmit": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep", "timeout": 10}
                ]
            }],
            "PermissionRequest": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' blocked", hook_path.display()), "timeout": 10}]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "PostToolUseFailure": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "SubagentStop": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "Stop": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]
            }],
            "SessionEnd": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' release", hook_path.display()), "timeout": 10}]
            }]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_claude().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_settings);
    assert!(!result.hook_path.exists());
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_errors_when_claude_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_claude().unwrap_err().to_string();

    assert!(err.contains("claude directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_v2_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=codex\n# HERDR_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let codex = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Codex)
        .unwrap();

    assert_eq!(codex.path, hook_path);
    assert_eq!(codex.installed_version, Some(2));
    assert_eq!(codex.expected_version, 9);
    assert_eq!(codex.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_v8_integration_status_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=codex\n# HERDR_INTEGRATION_VERSION=8\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let codex = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Codex)
        .unwrap();
    assert_eq!(codex.installed_version, Some(8));
    assert_eq!(codex.expected_version, 9);
    assert_eq!(codex.state, IntegrationStatusKind::Outdated);

    install_codex().unwrap();
    let status = integration_status_at(
        crate::api::schema::IntegrationTarget::Codex,
        hook_path,
        CODEX_INTEGRATION_VERSION,
    );
    assert_eq!(status.installed_version, Some(9));
    assert_eq!(status.state, IntegrationStatusKind::Current);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_writes_hook_and_updates_hooks_and_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_codex().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(&installed.hooks_path).unwrap()).unwrap();
    let config = fs::read_to_string(&installed.config_path).unwrap();

    assert_eq!(installed.hook_path, codex_dir.join(CODEX_HOOK_INSTALL_NAME));
    assert_eq!(installed.hooks_path, codex_dir.join("hooks.json"));
    assert_eq!(installed.config_path, codex_dir.join("config.toml"));
    assert_eq!(hook_content, CODEX_HOOK_ASSET);
    assert!(hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" session"));
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert!(hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with(" usage"));
    assert!(config.contains("model = \"gpt-5.4\""));
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
    assert!(!config.contains("codex_hooks"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_uses_codex_home_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let codex_dir = base.join("custom-codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
    std::env::set_var(CODEX_HOME_ENV_VAR, &codex_dir);

    let installed = install_codex().unwrap();

    assert_eq!(installed.hook_path, codex_dir.join(CODEX_HOOK_INSTALL_NAME));
    assert_eq!(installed.hooks_path, codex_dir.join("hooks.json"));
    assert_eq!(installed.config_path, codex_dir.join("config.toml"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_is_idempotent_for_hook_entries_and_feature_flag() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\ncodex_hooks = false\nother = true\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_codex().unwrap();
    install_codex().unwrap();

    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap()).unwrap();
    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert_eq!(hooks["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(config.matches("hooks = true").count(), 1);
    assert!(!config.contains("codex_hooks"));
    assert!(config.contains("other = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_only_migrates_top_level_feature_flags() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
            codex_dir.join("config.toml"),
            "profile = \"work\"\n\n[profiles.work.features]\nhooks = false\ncodex_hooks = false\n\n[features]\ncodex_hooks = true\nother = true\n",
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    install_codex().unwrap();

    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
    assert!(config.contains("[features]\nhooks = true\nother = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_codex_removes_herdr_hooks_and_leaves_config_alone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CODEX_HOOK_ASSET).unwrap();
    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]}],
            "UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                {"type": "command", "command": "echo keep", "timeout": 10}
            ]}],
            "PreToolUse": [{"hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": format!("bash '{}' blocked", hook_path.display()), "timeout": 10}]}],
            "Stop": [{"hooks": [
                {"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10},
                {"type": "command", "command": format!("bash '{}' usage", hook_path.display()), "timeout": 10}
            ]}]
        }
    });
    fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string(&hooks).unwrap(),
    )
    .unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\nhooks = true\nother = true\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_codex().unwrap();
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap()).unwrap();
    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_hooks);
    assert!(!result.hook_path.exists());
    assert!(hooks["hooks"].get("SessionStart").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert_eq!(
        hooks["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(config.contains("hooks = true"));
    assert!(config.contains("other = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_codex().unwrap_err().to_string();

    assert!(err.contains("codex config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_writes_hook_and_updates_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    fs::write(
            kimi_dir.join("config.toml"),
            "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\nmatcher = \"task.completed\"\ncommand = \"echo keep\"\ntimeout = 3\n",
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_kimi().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let config = fs::read_to_string(&installed.config_path).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert_eq!(
        installed.hook_path,
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.config_path, kimi_dir.join("config.toml"));
    assert_eq!(hook_content, KIMI_HOOK_ASSET);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len() + 1);
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(config.contains(KIMI_CONFIG_BLOCK_END));
    for (event, matcher, action) in KIMI_HOOK_EVENTS {
        assert_kimi_hook(&config, &installed.hook_path, event, matcher, action);
    }

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn kimi_question_hooks_report_blocked_until_the_question_finishes() {
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "blocked",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&("PreToolUse", Some(KIMI_OTHER_TOOL_MATCHER), "working",)));
}

#[test]
fn install_kimi_uses_kimi_code_home_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let kimi_dir = base.join("custom-kimi");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var(KIMI_CODE_HOME_ENV_VAR, &kimi_dir);

    let installed = install_kimi().unwrap();

    assert_eq!(
        installed.hook_path,
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.config_path, kimi_dir.join("config.toml"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_is_idempotent_for_config_block() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var("HOME", &home);

    install_kimi().unwrap();
    install_kimi().unwrap();

    let config = fs::read_to_string(kimi_dir.join("config.toml")).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_kimi_removes_hook_and_config_block_preserves_other_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_kimi().unwrap();
    fs::write(
            &installed.config_path,
            format!(
                "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\ncommand = \"echo keep\"\n\n{}",
                fs::read_to_string(&installed.config_path).unwrap()
            ),
        )
        .unwrap();

    let result = uninstall_kimi().unwrap();
    let config = fs::read_to_string(kimi_dir.join("config.toml")).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert!(result.removed_hook_file);
    assert!(result.updated_config);
    assert!(!result.hook_path.exists());
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_END));
    assert_eq!(hooks.len(), 1);
    assert_eq!(
        hooks[0].get("event").and_then(toml::Value::as_str),
        Some("Notification")
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_kimi().unwrap_err().to_string();

    assert!(err.contains("kimi code config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_copilot_writes_hook_and_updates_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let copilot_dir = home.join(".copilot");
    fs::create_dir_all(&copilot_dir).unwrap();
    let hook_path = copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME);
    let stale_session_start_command = format!(
        "bash {}",
        shell_single_quote(&hook_path.display().to_string())
    );
    fs::write(
            copilot_dir.join("settings.json"),
            format!(
                r#"{{"theme":"dark","hooks":{{"PreToolUse":[{{"type":"command","command":"echo keep","timeoutSec":10}}],"sessionStart":[{{"type":"command","bash":{},"timeoutSec":10}}]}}}}"#,
                serde_json::to_string(&stale_session_start_command).unwrap()
            ),
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_copilot().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();

    assert_eq!(
        installed.hook_path,
        copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.settings_path, copilot_dir.join("settings.json"));
    assert_eq!(hook_content, COPILOT_HOOK_ASSET);
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(settings["hooks"]["PreToolUse"][0]["command"], "echo keep");
    assert!(settings["hooks"]["SessionStart"][0][direct_command_field()]
        .as_str()
        .unwrap()
        .contains(COPILOT_HOOK_INSTALL_NAME));
    for event in COPILOT_REMOVED_LIFECYCLE_HOOK_EVENTS {
        if let Some(entries) = settings["hooks"].get(event) {
            assert!(
                !entries.to_string().contains(COPILOT_HOOK_INSTALL_NAME),
                "expected herdr hooks.{event} entries to be removed"
            );
        }
    }
    assert!(settings["hooks"].get("sessionStart").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn copilot_v1_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let copilot_hooks_dir = home.join(".copilot").join("hooks");
    fs::create_dir_all(&copilot_hooks_dir).unwrap();
    let hook_path = copilot_hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=copilot\n# HERDR_INTEGRATION_VERSION=1\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let copilot = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Copilot)
        .unwrap();

    assert_eq!(copilot.path, hook_path);
    assert_eq!(copilot.installed_version, Some(1));
    assert_eq!(copilot.expected_version, COPILOT_INTEGRATION_VERSION);
    assert_eq!(copilot.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_copilot_uses_copilot_home_env_and_is_idempotent() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let copilot_dir = base.join("custom-copilot");
    fs::create_dir_all(&copilot_dir).unwrap();
    std::env::set_var(COPILOT_HOME_ENV_VAR, &copilot_dir);

    let installed = install_copilot().unwrap();
    install_copilot().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(copilot_dir.join("settings.json")).unwrap())
            .unwrap();

    assert_eq!(
        installed.hook_path,
        copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        settings["hooks"]["SessionStart"].as_array().unwrap().len(),
        1
    );
    for event in COPILOT_REMOVED_LIFECYCLE_HOOK_EVENTS {
        assert!(
            settings["hooks"].get(event).is_none(),
            "expected hooks.{event} to be absent"
        );
    }
    assert!(settings["hooks"].get("sessionStart").is_none());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_copilot_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let copilot_dir = home.join(".copilot");
    let hooks_dir = copilot_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);
    fs::write(&hook_path, COPILOT_HOOK_ASSET).unwrap();
    let command = format!(
        "bash {}",
        shell_single_quote(&hook_path.display().to_string())
    );
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {"type": "command", direct_command_field(): command, "timeoutSec": 10},
                {"type": "command", "command": "echo keep", "timeoutSec": 10}
            ],
            "PostToolUse": [{"type": "command", direct_command_field(): command, "timeoutSec": 10}],
            "notification": [{
                "type": "command",
                "matcher": "permission_prompt|elicitation_dialog|agent_idle",
                direct_command_field(): command,
                "timeoutSec": 10
            }]
        }
    });
    fs::write(
        copilot_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_copilot().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(copilot_dir.join("settings.json")).unwrap())
            .unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_settings);
    assert!(!result.hook_path.exists());
    assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(settings["hooks"]["PreToolUse"][0]["command"], "echo keep");
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("notification").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn copilot_v3_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let copilot_hooks_dir = home.join(".copilot").join("hooks");
    fs::create_dir_all(&copilot_hooks_dir).unwrap();
    let hook_path = copilot_hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=copilot\n# HERDR_INTEGRATION_VERSION=3\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let copilot = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Copilot)
        .unwrap();

    assert_eq!(copilot.installed_version, Some(3));
    assert_eq!(copilot.expected_version, 4);
    assert_eq!(copilot.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
const COPILOT_USER_STATUSLINE_COMMAND: &str = "~/.copilot/statusline.sh";

/// Seeds a Copilot config dir under `base` (selected through `COPILOT_HOME`)
/// with the given `settings.json` text and returns the directory.
#[cfg(not(windows))]
fn seed_copilot_dir(base: &Path, name: &str, settings: &str) -> PathBuf {
    let copilot_dir = base.join(name);
    fs::create_dir_all(&copilot_dir).unwrap();
    fs::write(copilot_dir.join("settings.json"), settings).unwrap();
    std::env::set_var(COPILOT_HOME_ENV_VAR, &copilot_dir);
    copilot_dir
}

#[cfg(not(windows))]
fn copilot_statusline_settings(command: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "theme": "dark",
        "statusLine": {"type": "command", "command": command, "padding": 1}
    }))
    .unwrap()
}

#[cfg(not(windows))]
fn read_copilot_settings(copilot_dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(copilot_dir.join("settings.json")).unwrap()).unwrap()
}

/// Runs `command` with `input` on stdin outside any herdr pane (no reporting)
/// and returns its stdout and exit code.
#[cfg(not(windows))]
fn run_statusline_command(mut command: std::process::Command, input: &str) -> (String, i32) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = command
        .env_remove("HERDR_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        String::from_utf8(output.stdout).unwrap(),
        output.status.code().unwrap(),
    )
}

#[cfg(not(windows))]
#[test]
fn install_copilot_wraps_an_existing_statusline_and_uninstall_restores_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    let hooks_dir = copilot_dir.join("hooks");
    let tap_path = hooks_dir.join("herdr-statusline-tap.sh");
    let wrapper_path = hooks_dir.join("herdr-statusline-wrap.sh");

    install_copilot().unwrap();
    let installed_bytes = fs::read_to_string(copilot_dir.join("settings.json")).unwrap();
    let settings = read_copilot_settings(&copilot_dir);

    assert_eq!(
        settings["statusLine"]["command"].as_str().unwrap(),
        wrapper_path.display().to_string(),
        "the settings command is the bare wrapper path"
    );
    assert_eq!(settings["statusLine"]["type"], "command");
    assert_eq!(settings["statusLine"]["padding"], 1);
    assert_eq!(settings["theme"], "dark");
    assert_eq!(
        fs::read_to_string(&tap_path).unwrap(),
        COPILOT_STATUSLINE_TAP_ASSET
    );
    assert!(fs::read_to_string(&wrapper_path)
        .unwrap()
        .contains(COPILOT_USER_STATUSLINE_COMMAND));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o111;
        assert_ne!(mode(&wrapper_path), 0, "the wrapper must be executable");
        assert_ne!(mode(&tap_path), 0, "the tap must be executable");
    }

    install_copilot().unwrap();
    assert_eq!(
        fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        installed_bytes,
        "reinstall must not wrap twice"
    );

    uninstall_copilot().unwrap();
    let restored = read_copilot_settings(&copilot_dir);
    assert_eq!(
        restored["statusLine"],
        json!({"type": "command", "command": COPILOT_USER_STATUSLINE_COMMAND, "padding": 1})
    );
    assert!(!tap_path.exists());
    assert!(!wrapper_path.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_copilot_without_statusline_creates_none() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let copilot_dir = seed_copilot_dir(&base, "copilot", r#"{"theme":"dark"}"#);

    install_copilot().unwrap();

    let settings = read_copilot_settings(&copilot_dir);
    assert!(settings.get("statusLine").is_none());
    assert!(!copilot_dir
        .join("hooks")
        .join("herdr-statusline-wrap.sh")
        .exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_copilot_leaves_other_statusline_shapes_alone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    for statusline in [
        json!("bash ~/x.sh"),
        json!({"type": "static", "command": "bash ~/x.sh"}),
        json!({"type": "command", "command": 5}),
        json!({"type": "command", "command": ""}),
        json!({"type": "command"}),
    ] {
        let seeded = json!({"statusLine": statusline.clone()});
        let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded.to_string());

        install_copilot().unwrap();
        assert_eq!(
            read_copilot_settings(&copilot_dir)["statusLine"],
            statusline
        );
        uninstall_copilot().unwrap();
        assert_eq!(
            read_copilot_settings(&copilot_dir)["statusLine"],
            statusline
        );
        let _ = fs::remove_dir_all(&copilot_dir);
    }

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_restores_statusline_even_without_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    // Only the wrapped statusline remains: no hook entry for uninstall to find.
    let mut settings = read_copilot_settings(&copilot_dir);
    settings.as_object_mut().unwrap().remove("hooks");
    fs::write(
        copilot_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_copilot().unwrap();

    assert!(result.updated_settings);
    assert_eq!(
        read_copilot_settings(&copilot_dir)["statusLine"]["command"],
        COPILOT_USER_STATUSLINE_COMMAND
    );
    assert!(!copilot_dir
        .join("hooks")
        .join("herdr-statusline-wrap.sh")
        .exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn copilot_statusline_wrapper_passes_stdin_stdout_and_exit_code_through() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original = "printf 'OUT:'; cat; exit 3";
    let copilot_dir = seed_copilot_dir(&base, "copilot", &copilot_statusline_settings(original));
    let hooks_dir = copilot_dir.join("hooks");
    let wrapper_path = hooks_dir.join("herdr-statusline-wrap.sh");
    install_copilot().unwrap();
    let stdin = r#"{"context_window":{"context_window_size":200000}}"#;
    let expected = (format!("OUT:{stdin}"), 3);

    // Executed directly (no shell in front of it), then through a shell.
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );
    let mut through_sh = std::process::Command::new("sh");
    through_sh.arg("-c").arg(wrapper_path.display().to_string());
    assert_eq!(run_statusline_command(through_sh, stdin), expected);

    // A deleted tap file still runs the original.
    fs::remove_file(hooks_dir.join("herdr-statusline-tap.sh")).unwrap();
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn copilot_statusline_wrapper_keeps_quotes_dollars_and_backslashes_intact() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original = r#"printf '%s' "q'q \$V \\ b""#;
    let direct = {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(original);
        run_statusline_command(command, "")
    };
    assert_eq!(direct.0, r"q'q $V \ b");
    let copilot_dir = seed_copilot_dir(&base, "copilot", &copilot_statusline_settings(original));
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");

    install_copilot().unwrap();

    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), "{}"),
        direct
    );
    uninstall_copilot().unwrap();
    assert_eq!(
        read_copilot_settings(&copilot_dir)["statusLine"]["command"],
        original
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_copilot_repoints_a_wrapper_from_another_directory_instead_of_nesting() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let first_dir = seed_copilot_dir(&base, "first", &seeded);
    install_copilot().unwrap();
    // The config directory moved: same settings, the old wrapper still on disk.
    let second_dir = base.join("second");
    fs::create_dir_all(&second_dir).unwrap();
    fs::copy(
        first_dir.join("settings.json"),
        second_dir.join("settings.json"),
    )
    .unwrap();
    std::env::set_var(COPILOT_HOME_ENV_VAR, &second_dir);

    install_copilot().unwrap();

    let second_wrapper = second_dir.join("hooks").join("herdr-statusline-wrap.sh");
    assert_eq!(
        read_copilot_settings(&second_dir)["statusLine"]["command"]
            .as_str()
            .unwrap(),
        second_wrapper.display().to_string()
    );
    uninstall_copilot().unwrap();
    assert_eq!(
        read_copilot_settings(&second_dir)["statusLine"]["command"],
        COPILOT_USER_STATUSLINE_COMMAND
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn copilot_statusline_is_not_wrapped_when_the_wrapper_path_needs_quoting() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot dir", &seeded);

    install_copilot().unwrap();

    assert_eq!(
        read_copilot_settings(&copilot_dir)["statusLine"]["command"],
        COPILOT_USER_STATUSLINE_COMMAND
    );
    assert!(!copilot_dir
        .join("hooks")
        .join("herdr-statusline-wrap.sh")
        .exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_copilot_keeps_a_backup_of_the_original_next_to_the_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    let backup_path = copilot_dir.join("herdr-statusline-original.json");

    install_copilot().unwrap();

    let backup: Value = serde_json::from_str(&fs::read_to_string(&backup_path).unwrap()).unwrap();
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");
    assert_eq!(
        backup,
        json!({
            "statusLine.command": COPILOT_USER_STATUSLINE_COMMAND,
            "wrapper": wrapper_path.display().to_string(),
        })
    );
    uninstall_copilot().unwrap();
    assert!(!backup_path.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_restores_from_the_backup_when_the_wrapper_is_gone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    // The whole hooks directory is deleted: wrapper, tap and hook file.
    fs::remove_dir_all(copilot_dir.join("hooks")).unwrap();

    let result = uninstall_copilot().unwrap();

    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let restored = read_copilot_settings(&copilot_dir);
    assert_eq!(
        restored["statusLine"],
        json!({"type": "command", "command": COPILOT_USER_STATUSLINE_COMMAND, "padding": 1})
    );
    assert!(!restored.to_string().contains(COPILOT_HOOK_INSTALL_NAME));
    assert!(!copilot_dir.join("herdr-statusline-original.json").exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn reinstall_copilot_rebuilds_a_lost_wrapper_from_the_backup() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");
    install_copilot().unwrap();
    let installed_bytes = fs::read_to_string(copilot_dir.join("settings.json")).unwrap();
    fs::remove_file(&wrapper_path).unwrap();

    let installed = install_copilot().unwrap();

    assert!(installed.warnings.is_empty(), "{:?}", installed.warnings);
    assert_eq!(
        fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        installed_bytes,
        "the rebuilt wrapper is not wrapped again"
    );
    assert!(fs::read_to_string(&wrapper_path)
        .unwrap()
        .contains(COPILOT_USER_STATUSLINE_COMMAND));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_removes_hooks_and_warns_when_wrapper_and_backup_are_gone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");
    fs::remove_file(&wrapper_path).unwrap();
    fs::remove_file(copilot_dir.join("herdr-statusline-original.json")).unwrap();

    let result = uninstall_copilot().unwrap();

    assert!(result.updated_settings, "the hook entries are removed");
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert!(
        result.warnings[0].starts_with(INSTALL_WARNING_PREFIX),
        "{:?}",
        result.warnings
    );
    assert!(
        result.warnings[0].contains("statusLine.command"),
        "{:?}",
        result.warnings
    );
    let settings = read_copilot_settings(&copilot_dir);
    assert_eq!(
        settings["statusLine"]["command"].as_str().unwrap(),
        wrapper_path.display().to_string(),
        "the command is never guessed"
    );
    let hook_path = copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME);
    assert!(!settings
        .to_string()
        .contains(&hook_path.display().to_string()));
    assert!(!hook_path.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_keeps_the_wrapper_and_backup_when_the_statusline_type_changed() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");
    let backup_path = copilot_dir.join("herdr-statusline-original.json");
    let mut settings = read_copilot_settings(&copilot_dir);
    settings["statusLine"]["type"] = json!("script");
    fs::write(
        copilot_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_copilot().unwrap();

    assert!(result.updated_settings, "the hook entries are removed");
    let hook_path = copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME);
    assert!(!hook_path.exists());
    let settings = read_copilot_settings(&copilot_dir);
    assert!(!settings
        .to_string()
        .contains(&hook_path.display().to_string()));
    assert_eq!(
        settings["statusLine"]["command"].as_str().unwrap(),
        wrapper_path.display().to_string()
    );
    assert!(wrapper_path.exists(), "the settings still name the wrapper");
    assert!(backup_path.exists(), "the settings still name the wrapper");
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert!(result.warnings[0].starts_with(INSTALL_WARNING_PREFIX));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_never_restores_the_backup_of_another_wrapper() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    // The settings name a wrapper that is gone; the backup belongs to this
    // directory's wrapper, so it says nothing about that one's original.
    let other_wrapper = base.join("elsewhere").join("herdr-statusline-wrap.sh");
    let mut settings = read_copilot_settings(&copilot_dir);
    settings["statusLine"]["command"] = json!(other_wrapper.display().to_string());
    fs::write(
        copilot_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_copilot().unwrap();

    assert_eq!(
        read_copilot_settings(&copilot_dir)["statusLine"]["command"]
            .as_str()
            .unwrap(),
        other_wrapper.display().to_string(),
        "the command is never guessed"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("statusLine.command")),
        "{:?}",
        result.warnings
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn reinstall_copilot_replaces_the_wrapper_file_instead_of_rewriting_it() {
    use std::os::unix::fs::MetadataExt;

    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    let hooks_dir = copilot_dir.join("hooks");
    let wrapper_path = hooks_dir.join("herdr-statusline-wrap.sh");
    install_copilot().unwrap();
    let first = fs::metadata(&wrapper_path).unwrap().ino();
    // Holding the first file keeps its inode from being reused.
    let _held = fs::File::open(&wrapper_path).unwrap();

    install_copilot().unwrap();

    assert_ne!(
        fs::metadata(&wrapper_path).unwrap().ino(),
        first,
        "a running statusline must never read a partly written wrapper"
    );
    assert!(fs::read_to_string(&wrapper_path)
        .unwrap()
        .contains(COPILOT_USER_STATUSLINE_COMMAND));
    let leftovers = fs::read_dir(&hooks_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_copilot_refuses_to_guess_when_the_wrapper_is_unreadable_and_unbacked() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded = copilot_statusline_settings(COPILOT_USER_STATUSLINE_COMMAND);
    let copilot_dir = seed_copilot_dir(&base, "copilot", &seeded);
    install_copilot().unwrap();
    let wrapper_path = copilot_dir.join("hooks").join("herdr-statusline-wrap.sh");
    let installed_bytes = fs::read_to_string(copilot_dir.join("settings.json")).unwrap();
    fs::write(&wrapper_path, "#!/bin/sh\n").unwrap();
    fs::remove_file(copilot_dir.join("herdr-statusline-original.json")).unwrap();

    let err = uninstall_copilot().unwrap_err().to_string();

    assert!(err.contains("statusLine.command"), "{err}");
    assert_eq!(
        fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        installed_bytes
    );
    assert!(wrapper_path.exists(), "the only copy is never removed");

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn copilot_statusline_wrapper_runs_an_executable_path_with_a_space_directly() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let script_dir = base.join("Application Support");
    fs::create_dir_all(&script_dir).unwrap();
    let script = script_dir.join("line.sh");
    fs::write(&script, "#!/bin/sh\nprintf 'OUT:'; cat; exit 3\n").unwrap();
    make_executable(&script).unwrap();
    let original = script.display().to_string();
    let copilot_dir = seed_copilot_dir(&base, "copilot", &copilot_statusline_settings(&original));
    let hooks_dir = copilot_dir.join("hooks");
    let wrapper_path = hooks_dir.join("herdr-statusline-wrap.sh");
    install_copilot().unwrap();
    let stdin = r#"{"context_window":{"context_window_size":200000}}"#;
    let expected = (format!("OUT:{stdin}"), 3);

    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );
    // Without the tap the wrapper runs it the same way.
    fs::remove_file(hooks_dir.join("herdr-statusline-tap.sh")).unwrap();
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_copilot_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_copilot().unwrap_err().to_string();

    assert!(err.contains("copilot config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_devin_writes_hook_and_updates_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).unwrap();
    fs::write(
        devin_dir.join("config.json"),
        r#"{"theme_mode":"dark","hooks":{}}"#,
    )
    .unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
    std::env::set_var("HOME", base.join("home"));

    let installed = install_devin().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();

    assert_eq!(installed.hook_path, devin_dir.join(DEVIN_HOOK_INSTALL_NAME));
    assert_eq!(installed.settings_path, devin_dir.join("config.json"));
    assert_eq!(hook_content, DEVIN_HOOK_ASSET);
    assert_eq!(settings["theme_mode"], "dark");
    for (event, action) in DEVIN_HOOK_EVENTS {
        let command = settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.contains(DEVIN_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected devin {event} hook command to end with {action}, got {command}"
        );
    }

    clear_integration_path_env();
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_devin_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
    std::env::set_var("HOME", base.join("home"));

    install_devin().unwrap();
    install_devin().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(devin_dir.join("config.json")).unwrap()).unwrap();
    for (event, _) in DEVIN_HOOK_EVENTS {
        assert_eq!(
            settings["hooks"][event].as_array().unwrap().len(),
            1,
            "expected hooks.{event} to be idempotent"
        );
    }

    clear_integration_path_env();
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_devin_removes_legacy_lifecycle_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
    std::env::set_var("HOME", base.join("home"));

    let hook_path = devin_dir.join(DEVIN_HOOK_INSTALL_NAME);
    let mut hooks = Map::new();
    for (event, action) in DEVIN_REMOVED_LIFECYCLE_HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([
                {
                    "hooks": [{
                        "type": "command",
                        "command": hook_command(&hook_path, Some(action)),
                        "timeout": 10
                    }]
                }
            ]),
        );
    }
    fs::write(
        devin_dir.join("config.json"),
        serde_json::to_string_pretty(&json!({ "hooks": hooks })).unwrap(),
    )
    .unwrap();

    install_devin().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(devin_dir.join("config.json")).unwrap()).unwrap();
    for (event, action) in DEVIN_REMOVED_LIFECYCLE_HOOK_EVENTS {
        let legacy_command = hook_command(&hook_path, Some(action));
        let entries = settings["hooks"][event].as_array();
        assert!(
            entries.is_none_or(|entries| {
                entries.iter().all(|entry| {
                    entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_none_or(|hooks| {
                            hooks.iter().all(|hook| {
                                hook.get("command").and_then(Value::as_str)
                                    != Some(legacy_command.as_str())
                            })
                        })
                })
            }),
            "expected legacy devin {event} -> {action} hook to be removed"
        );

        if !DEVIN_HOOK_EVENTS
            .iter()
            .any(|(installed_event, _)| installed_event == &event)
        {
            continue;
        }

        let session_command = hook_command(&hook_path, Some("session"));
        let entries = entries.unwrap();
        assert!(
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command").and_then(Value::as_str)
                                == Some(session_command.as_str())
                        })
                    })
            }),
            "expected devin {event} session hook to be installed"
        );
    }

    clear_integration_path_env();
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_devin_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
    std::env::set_var("HOME", base.join("home"));

    install_devin().unwrap();

    let hook_path = devin_dir.join(DEVIN_HOOK_INSTALL_NAME);
    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(devin_dir.join("config.json")).unwrap()).unwrap();
    settings["hooks"]["UserPromptSubmit"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": "echo keep",
                "timeout": 10
            }]
        }));
    fs::write(
        devin_dir.join("config.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_devin().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(devin_dir.join("config.json")).unwrap()).unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_settings);
    assert!(!hook_path.exists());
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    clear_integration_path_env();
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_devin_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let xdg_config = base.join("xdg");
    fs::create_dir_all(&xdg_config).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
    std::env::set_var("HOME", base.join("home"));

    let err = install_devin().unwrap_err().to_string();
    assert!(err.contains("devin config directory not found"));

    clear_integration_path_env();
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_droid_writes_hook_to_settings_and_cleans_legacy_hooks_json() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    let legacy_hook_path = droid_dir.join("hooks").join(DROID_HOOK_INSTALL_NAME);
    fs::create_dir_all(legacy_hook_path.parent().unwrap()).unwrap();
    fs::create_dir_all(&droid_dir).unwrap();
    let legacy_command = format!(
        "bash {}",
        shell_single_quote(&legacy_hook_path.display().to_string())
    );
    fs::write(
            droid_dir.join("hooks.json"),
            format!(
                r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":{},"timeout":10}}]}}],"PreToolUse":[{{"matcher":"Read","hooks":[{{"type":"command","command":"echo keep","timeout":10}}]}}]}}}}"#,
                serde_json::to_string(&legacy_command).unwrap(),
            ),
        )
        .unwrap();
    fs::write(
        droid_dir.join("settings.json"),
        r#"{"theme":"factory-dark"}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_droid().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();
    let legacy_hooks: Value =
        serde_json::from_str(&fs::read_to_string(&installed.hooks_path).unwrap()).unwrap();

    assert_eq!(
        installed.hook_path,
        droid_dir.join("hooks").join(DROID_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.hooks_path, droid_dir.join("hooks.json"));
    assert_eq!(installed.settings_path, droid_dir.join("settings.json"));
    assert!(installed.updated_legacy_hooks);
    assert_eq!(hook_content, DROID_HOOK_ASSET);
    assert_eq!(settings["theme"], "factory-dark");
    assert!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(DROID_HOOK_INSTALL_NAME));
    assert!(settings["hooks"]["SessionStart"][0]
        .get("matcher")
        .is_none());
    for (event, action) in DROID_HOOK_EVENTS {
        let command = settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.contains(DROID_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected droid {event} hook command to end with {action}, got {command}"
        );
    }
    assert_eq!(legacy_hooks["hooks"]["PreToolUse"][0]["matcher"], "Read");
    assert!(legacy_hooks["hooks"].get("SessionStart").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_droid_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    fs::create_dir_all(&droid_dir).unwrap();
    std::env::set_var("HOME", &home);

    install_droid().unwrap();
    install_droid().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(droid_dir.join("settings.json")).unwrap())
            .unwrap();
    for (event, _) in DROID_HOOK_EVENTS {
        assert_eq!(
            settings["hooks"][event].as_array().unwrap().len(),
            1,
            "expected hooks.{event} to be idempotent"
        );
    }

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn droid_v1_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let droid_hooks_dir = home.join(".factory").join("hooks");
    fs::create_dir_all(&droid_hooks_dir).unwrap();
    let hook_path = droid_hooks_dir.join(DROID_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=droid\n# HERDR_INTEGRATION_VERSION=1\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let droid = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Droid)
        .unwrap();

    assert_eq!(droid.path, hook_path);
    assert_eq!(droid.installed_version, Some(1));
    assert_eq!(droid.expected_version, DROID_INTEGRATION_VERSION);
    assert_eq!(droid.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_droid_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    let hooks_dir = droid_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(DROID_HOOK_INSTALL_NAME);
    fs::write(&hook_path, DROID_HOOK_ASSET).unwrap();
    let command = format!(
        "bash {}",
        shell_single_quote(&hook_path.display().to_string())
    );
    fs::write(
            droid_dir.join("hooks.json"),
            format!(
                r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":{},"timeout":10}},{{"type":"command","command":"echo keep","timeout":10}}]}}],"PreToolUse":[{{"matcher":"Read","hooks":[{{"type":"command","command":"echo read","timeout":10}}]}}]}}}}"#,
                serde_json::to_string(&command).unwrap(),
            ),
        )
        .unwrap();
    fs::write(
            droid_dir.join("settings.json"),
            format!(
                r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":{},"timeout":10}}]}}],"PostToolUse":[{{"matcher":"Edit","hooks":[{{"type":"command","command":"echo post","timeout":10}}]}}]}}}}"#,
                serde_json::to_string(&command).unwrap(),
            ),
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_droid().unwrap();
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(droid_dir.join("hooks.json")).unwrap()).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(droid_dir.join("settings.json")).unwrap())
            .unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_hooks);
    assert!(result.updated_settings);
    assert!(!result.hook_path.exists());
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert_eq!(hooks["hooks"]["PreToolUse"][0]["matcher"], "Read");
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Edit");

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_droid_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_droid().unwrap_err().to_string();

    assert!(err.contains("droid config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_writes_server_and_tui_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_opencode().unwrap();

    assert_eq!(
        installed.plugin_path,
        opencode_dir
            .join("plugins")
            .join(OPENCODE_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(&installed.plugin_path).unwrap(),
        OPENCODE_PLUGIN_ASSET
    );
    assert_eq!(
        installed.tui_plugin_path,
        opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(&installed.tui_plugin_path).unwrap(),
        OPENCODE_TUI_PLUGIN_ASSET
    );
    assert_eq!(installed.tui_config_path, opencode_dir.join("tui.jsonc"));
    let tui_config: Value =
        serde_json::from_str(&fs::read_to_string(&installed.tui_config_path).unwrap()).unwrap();
    assert_eq!(tui_config["plugin"], json!([OPENCODE_TUI_PLUGIN_SPEC]));
    let cli_config_path = installed
        .cli_config_path
        .expect("cli.json should be created when OpenCode has nothing to migrate");
    assert_eq!(cli_config_path, opencode_dir.join("cli.json"));
    let cli_config: Value =
        serde_json::from_str(&fs::read_to_string(&cli_config_path).unwrap()).unwrap();
    assert_eq!(cli_config["plugins"], json!([OPENCODE_V2_TUI_PLUGIN_SPEC]));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn opencode_reuses_json_registration_in_symlinked_config_directory() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dotfiles = base.join("dotfiles");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::create_dir_all(&dotfiles).unwrap();
    std::os::unix::fs::symlink(&dotfiles, &dir).unwrap();
    std::env::set_var("HOME", &home);
    let json_path = dir.join("tui.json");
    let original = "{\n  // User preferences\n  \"theme\":\"system\",\n  \"plugin\":[\"other\",[\"./herdr-tui-session.js\",{\"enabled\":true}]]\n}\n";
    fs::write(&json_path, original).unwrap();

    for _ in 0..2 {
        let installed = install_opencode().unwrap();
        assert_eq!(installed.tui_config_path, json_path);
        assert!(installed.cli_config_path.is_none());
        assert!(!dir.join("tui.jsonc").exists());
        assert_eq!(fs::read_to_string(&json_path).unwrap(), original);
        assert_eq!(
            integration_status_at(
                crate::api::schema::IntegrationTarget::Opencode,
                installed.plugin_path,
                OPENCODE_INTEGRATION_VERSION,
            )
            .state,
            IntegrationStatusKind::Current
        );
    }

    // Older installs may have registered the same plugin in both files.
    let jsonc_path = dir.join("tui.jsonc");
    fs::write(
        &jsonc_path,
        r#"{"plugin":["./herdr-tui-session.js","another"]}"#,
    )
    .unwrap();
    assert_eq!(install_opencode().unwrap().tui_config_path, jsonc_path);
    let result = uninstall_opencode().unwrap();
    assert_eq!(
        result.updated_tui_configs,
        vec![jsonc_path.clone(), json_path.clone()]
    );
    let json = fs::read_to_string(&json_path).unwrap();
    assert!(json.contains("// User preferences"));
    assert!(!json.contains("herdr-tui-session.js"));
    assert!(json.contains("\"other\""));
    assert!(json.contains("\"system\""));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(jsonc_path).unwrap()).unwrap(),
        json!({"plugin":["another"]})
    );
    assert!(uninstall_opencode().unwrap().updated_tui_configs.is_empty());
    assert_eq!(fs::read_link(&dir).unwrap(), dotfiles);
    assert!(!result.plugin_path.exists());
    assert!(!result.tui_plugin_path.exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_install_defers_v2_registration_while_migration_pending() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    fs::write(opencode_dir.join("tui.json"), "{}").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_opencode().unwrap();

    assert!(installed.cli_config_path.is_none());
    assert!(!opencode_dir.join("cli.json").exists());
    assert!(opencode_dir
        .join(OPENCODE_V2_TUI_PLUGIN_DIR)
        .join("tui.js")
        .is_file());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_v2_install_status_and_uninstall_preserve_cli_preferences() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HOME", &home);
    let cli = dir.join("cli.json");
    fs::write(
        &cli,
        r#"{"theme":{"name":"catppuccin"},"plugins":["other"]}"#,
    )
    .unwrap();
    let installed = install_opencode().unwrap();
    assert_eq!(installed.cli_config_path, Some(cli.clone()));
    let status = || {
        integration_status_at(
            crate::api::schema::IntegrationTarget::Opencode,
            installed.plugin_path.clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .state
    };
    assert_eq!(status(), IntegrationStatusKind::Current);
    let entry = dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).join("tui.js");
    assert_eq!(
        fs::read_to_string(&entry).unwrap(),
        OPENCODE_V2_TUI_PLUGIN_ASSET
    );
    fs::remove_file(&entry).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode().unwrap();
    super::opencode_config::remove_cli_plugin(&dir, OPENCODE_V2_TUI_PLUGIN_SPEC).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode().unwrap();
    uninstall_opencode().unwrap();
    assert!(!entry.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(cli).unwrap()).unwrap(),
        json!({"theme":{"name":"catppuccin"},"plugins":["other"]})
    );
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_hard_link_rejection_precedes_install_and_uninstall_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("cli.json");
    let original = r#"{"plugins":["./herdr-opencode"],"theme":"system"}"#;
    fs::write(&config, original).unwrap();
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).unwrap();
    let target = crate::api::schema::IntegrationTarget::Opencode;
    for error in [
        install_target(target).unwrap_err(),
        uninstall_target(target).unwrap_err(),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("cli.json"));
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert_eq!(fs::read_to_string(&alias).unwrap(), original);
    assert_eq!(crate::platform::config_file_link_count(&config).unwrap(), 2);
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_json_config_validation_precedes_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("tui.json");
    fs::write(&config, r#"{"plugin":{}}"#).unwrap();
    assert!(install_opencode()
        .unwrap_err()
        .to_string()
        .contains("plugin list"));
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    let original = r#"{"plugin":["./herdr-tui-session.js"]}"#;
    fs::write(&config, original).unwrap();
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).unwrap();
    for error in [
        install_opencode().unwrap_err(),
        uninstall_opencode().unwrap_err(),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("tui.json"));
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert_eq!(fs::read_to_string(&alias).unwrap(), original);
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[cfg(windows)]
#[test]
fn opencode_recovery_copy_blocks_retry_before_parsing_or_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("cli.json");
    let backup = dir.join("cli.json.herdr-backup");
    let original = r#"{"plugins":["./herdr-opencode"],"theme":"system"}"#;
    fs::write(&backup, original).unwrap();
    let target = crate::api::schema::IntegrationTarget::Opencode;
    for contents in [Some("{"), Some(original), None] {
        if let Some(contents) = contents {
            fs::write(&config, contents).unwrap();
        } else {
            fs::remove_file(&config).unwrap();
        }
        for error in [
            install_target(target).unwrap_err(),
            uninstall_target(target).unwrap_err(),
        ] {
            assert!(error.to_string().contains("recovery copy"), "{error}");
            assert!(error.to_string().contains("cli.json.herdr-backup"));
        }
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
        if contents.is_none() {
            assert!(!config.exists());
        }
    }
    // A symlink invocation must discover the referent's recovery copy too.
    let referent = base.join("preferences.json");
    fs::write(&referent, "{").unwrap();
    let linked_backup = base.join("preferences.json.herdr-backup");
    fs::rename(&backup, &linked_backup).unwrap();
    if symlink_file(&referent, &config) {
        let link_before = fs::read_link(&config).unwrap();
        let error = install_target(target).unwrap_err();
        assert!(error.to_string().contains("preferences.json.herdr-backup"));
        assert_eq!(fs::read_link(&config).unwrap(), link_before);
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_invalid_cli_config_does_not_overwrite_existing_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    fs::write(dir.join("cli.json"), r#"{"plugins":{}}"#).unwrap();
    assert!(install_opencode().is_err());
    assert_eq!(fs::read_to_string(plugin).unwrap(), "previous integration");
    assert!(!dir.join("tui.jsonc").exists());
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_status_requires_the_tui_plugin_and_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);
    let installed = install_opencode().unwrap();
    let status = || {
        integration_status_at(
            crate::api::schema::IntegrationTarget::Opencode,
            installed.plugin_path.clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .state
    };

    assert_eq!(status(), IntegrationStatusKind::Current);
    fs::remove_file(&installed.tui_plugin_path).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    fs::write(&installed.tui_plugin_path, OPENCODE_TUI_PLUGIN_ASSET).unwrap();
    super::opencode_config::remove_tui_plugin(&opencode_dir, OPENCODE_TUI_PLUGIN_SPEC).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_opencode_removes_plugins_and_managed_tui_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);
    let installed = install_opencode().unwrap();

    let result = uninstall_opencode().unwrap();

    assert!(result.removed_plugin);
    assert!(result.removed_tui_plugin);
    assert_eq!(
        result.updated_tui_configs,
        vec![installed.tui_config_path.clone()]
    );
    assert!(!result.plugin_path.exists());
    assert!(!result.tui_plugin_path.exists());
    assert!(installed.tui_config_path.exists());
    let tui_config: Value =
        serde_json::from_str(&fs::read_to_string(&installed.tui_config_path).unwrap()).unwrap();
    assert_eq!(tui_config, json!({}));
    assert_eq!(installed.plugin_path, result.plugin_path);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_invalid_tui_config_does_not_write_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    fs::write(opencode_dir.join("tui.jsonc"), r#"{"plugin":{}}"#).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("plugin list"));
    assert!(!opencode_dir
        .join("plugins")
        .join(OPENCODE_PLUGIN_INSTALL_NAME)
        .exists());
    assert!(!opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME).exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_opencode_removes_plugins_when_tui_config_is_invalid() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    let plugins_dir = opencode_dir.join("plugins");
    fs::create_dir_all(&plugins_dir).unwrap();
    let plugin_path = plugins_dir.join(OPENCODE_PLUGIN_INSTALL_NAME);
    let tui_plugin_path = opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    fs::write(&plugin_path, OPENCODE_PLUGIN_ASSET).unwrap();
    fs::write(&tui_plugin_path, OPENCODE_TUI_PLUGIN_ASSET).unwrap();
    fs::write(opencode_dir.join("tui.jsonc"), "{\"plugin\":").unwrap();
    let json_path = opencode_dir.join("tui.json");
    fs::write(
        &json_path,
        r#"{"plugin":["./herdr-tui-session.js","other"]}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let err = uninstall_opencode().unwrap_err().to_string();

    assert!(err.contains("failed to parse OpenCode TUI config"));
    assert!(!plugin_path.exists());
    assert!(!tui_plugin_path.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(json_path).unwrap()).unwrap(),
        json!({"plugin":["other"]})
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("opencode config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kilo_writes_plugin_to_plugin_dir() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kilo_dir = home.join(".config/kilo");
    fs::create_dir_all(&kilo_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_kilo().unwrap();
    let plugin_content = fs::read_to_string(&installed.plugin_path).unwrap();

    assert_eq!(
        installed.plugin_path,
        kilo_dir.join("plugin").join(KILO_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(plugin_content, KILO_PLUGIN_ASSET);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_kilo_removes_plugin_when_present() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kilo_plugin_dir = home.join(".config/kilo/plugin");
    fs::create_dir_all(&kilo_plugin_dir).unwrap();
    fs::write(
        kilo_plugin_dir.join(KILO_PLUGIN_INSTALL_NAME),
        KILO_PLUGIN_ASSET,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_kilo().unwrap();

    assert!(result.removed_plugin);
    assert!(!result.plugin_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kilo_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_kilo().unwrap_err().to_string();

    assert!(err.contains("kilo config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_writes_plugin_and_enables_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).unwrap();
    fs::write(hermes_dir.join("config.yaml"), "model:\n  provider: auto\n").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_hermes().unwrap();
    let manifest = fs::read_to_string(
        installed
            .plugin_dir
            .join(HERMES_PLUGIN_MANIFEST_INSTALL_NAME),
    )
    .unwrap();
    let init =
        fs::read_to_string(installed.plugin_dir.join(HERMES_PLUGIN_INIT_INSTALL_NAME)).unwrap();
    let config = fs::read_to_string(&installed.config_path).unwrap();

    assert_eq!(
        installed.plugin_dir,
        hermes_dir.join("plugins").join(HERMES_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(manifest, HERMES_PLUGIN_MANIFEST_ASSET);
    assert_eq!(init, HERMES_PLUGIN_INIT_ASSET);
    assert!(config.contains("plugins:\n  enabled:\n    - herdr-agent-state"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_is_idempotent_for_enabled_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  enabled:\n    - herdr-agent-state\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_hermes().unwrap();
    install_hermes().unwrap();

    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
    assert_eq!(config.matches("herdr-agent-state").count(), 1);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_preserves_flat_plugin_list() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  - platforms/discord\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_hermes().unwrap();

    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
    assert_eq!(
        config,
        "plugins:\n  - herdr-agent-state\n  - platforms/discord\n"
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_converts_flow_plugin_list_to_block_list() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins: [platforms/discord]\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_hermes().unwrap();

    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
    assert_eq!(
        config,
        "plugins:\n  - herdr-agent-state\n  - platforms/discord\n"
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_converts_inline_enabled_list_to_block_list() {
    let config = update_hermes_enabled_plugin("plugins:\n  enabled: [example-plugin]\n", true);
    assert_eq!(
        config,
        "plugins:\n  enabled:\n    - herdr-agent-state\n    - example-plugin\n"
    );
}

#[test]
fn install_hermes_preserves_quoted_inline_enabled_items() {
    let config =
        update_hermes_enabled_plugin("plugins:\n  enabled: [\"null\", 'foo: bar']\n", true);
    assert_eq!(
        config,
        "plugins:\n  enabled:\n    - herdr-agent-state\n    - \"null\"\n    - 'foo: bar'\n"
    );
}

#[test]
fn install_hermes_preserves_inline_enabled_comment() {
    let config = update_hermes_enabled_plugin(
        "plugins:\n  enabled: [example-plugin] # managed locally\n",
        true,
    );
    assert_eq!(
        config,
        "plugins:\n  enabled: # managed locally\n    - herdr-agent-state\n    - example-plugin\n"
    );
}

#[test]
fn install_hermes_preserves_inline_plugins_comment() {
    let config =
        update_hermes_enabled_plugin("plugins: [platforms/discord] # managed locally\n", true);
    assert_eq!(
        config,
        "plugins: # managed locally\n  - herdr-agent-state\n  - platforms/discord\n"
    );
}

#[test]
fn install_hermes_is_idempotent_for_inline_enabled_list_entry() {
    let config = update_hermes_enabled_plugin(
        "plugins:\n  enabled: [herdr-agent-state, example-plugin]\n",
        true,
    );
    assert_eq!(
        config,
        "plugins:\n  enabled: [herdr-agent-state, example-plugin]\n"
    );
}

#[test]
fn install_hermes_is_idempotent_for_quoted_flat_plugin_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  - \"herdr-agent-state\" # installed by herdr\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_hermes().unwrap();

    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
    assert_eq!(
        config,
        "plugins:\n  - \"herdr-agent-state\" # installed by herdr\n"
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_hermes_removes_plugin_and_enabled_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    let plugin_dir = hermes_dir.join("plugins").join(HERMES_PLUGIN_INSTALL_NAME);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join(HERMES_PLUGIN_INIT_INSTALL_NAME),
        HERMES_PLUGIN_INIT_ASSET,
    )
    .unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  enabled:\n    - other-plugin\n    - herdr-agent-state\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_hermes().unwrap();
    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();

    assert!(result.removed_plugin_dir);
    assert!(result.updated_config);
    assert!(!plugin_dir.exists());
    assert!(config.contains("    - other-plugin"));
    assert!(!config.contains("herdr-agent-state"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_hermes_preserves_flat_plugin_list() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    let plugin_dir = hermes_dir.join("plugins").join(HERMES_PLUGIN_INSTALL_NAME);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join(HERMES_PLUGIN_INIT_INSTALL_NAME),
        HERMES_PLUGIN_INIT_ASSET,
    )
    .unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  - other-plugin\n  - herdr-agent-state\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_hermes().unwrap();
    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();

    assert!(result.removed_plugin_dir);
    assert!(result.updated_config);
    assert_eq!(config, "plugins:\n  - other-plugin\n");

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_hermes_removes_flow_plugin_list_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    let plugin_dir = hermes_dir.join("plugins").join(HERMES_PLUGIN_INSTALL_NAME);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join(HERMES_PLUGIN_INIT_INSTALL_NAME),
        HERMES_PLUGIN_INIT_ASSET,
    )
    .unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins: [other-plugin, herdr-agent-state]\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_hermes().unwrap();
    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();

    assert!(result.removed_plugin_dir);
    assert!(result.updated_config);
    assert_eq!(config, "plugins:\n  - other-plugin\n");

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_hermes_removes_inline_enabled_list_entry() {
    let config = update_hermes_enabled_plugin(
        "plugins:\n  enabled: [example-plugin, herdr-agent-state]\n",
        false,
    );
    assert_eq!(config, "plugins:\n  enabled:\n    - example-plugin\n");
}

#[test]
fn uninstall_hermes_preserves_quoted_inline_enabled_items() {
    let config = update_hermes_enabled_plugin(
        "plugins:\n  enabled: ['foo: bar', herdr-agent-state]\n",
        false,
    );
    assert_eq!(config, "plugins:\n  enabled:\n    - 'foo: bar'\n");
}

#[test]
fn uninstall_hermes_converts_single_inline_enabled_entry_to_empty_list() {
    let config = update_hermes_enabled_plugin("plugins:\n  enabled: [herdr-agent-state]\n", false);
    assert_eq!(config, "plugins:\n  enabled: []\n");
}

#[test]
fn uninstall_hermes_removes_commented_flat_plugin_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let hermes_dir = home.join(".hermes");
    let plugin_dir = hermes_dir.join("plugins").join(HERMES_PLUGIN_INSTALL_NAME);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join(HERMES_PLUGIN_INIT_INSTALL_NAME),
        HERMES_PLUGIN_INIT_ASSET,
    )
    .unwrap();
    fs::write(
        hermes_dir.join("config.yaml"),
        "plugins:\n  - other-plugin\n  - herdr-agent-state # installed by herdr\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_hermes().unwrap();
    let config = fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();

    assert!(result.removed_plugin_dir);
    assert!(result.updated_config);
    assert_eq!(config, "plugins:\n  - other-plugin\n");

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_hermes_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_hermes().unwrap_err().to_string();

    assert!(err.contains("hermes config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_hook_assets_share_the_integration_version() {
    let marker = format!("# HERDR_INTEGRATION_VERSION={CLAUDE_INTEGRATION_VERSION}");
    let shell = include_str!("assets/claude/herdr-agent-state.sh");
    let powershell = include_str!("assets/claude/herdr-agent-state.ps1");

    assert!(shell.contains(&marker));
    assert!(powershell.contains(&marker));
    assert!(powershell.contains("report-prompt-cache"));
    assert!(powershell.contains("report-context-usage"));
    assert_eq!(CLAUDE_INTEGRATION_VERSION, 12);
}

#[test]
fn bundled_integration_asset_versions_match_expected_versions() {
    for (name, asset, expected_version) in [
        ("pi", PI_EXTENSION_ASSET, PI_INTEGRATION_VERSION),
        ("omp", OMP_EXTENSION_ASSET, OMP_INTEGRATION_VERSION),
        ("claude", CLAUDE_HOOK_ASSET, CLAUDE_INTEGRATION_VERSION),
        ("codex", CODEX_HOOK_ASSET, CODEX_INTEGRATION_VERSION),
        ("kimi", KIMI_HOOK_ASSET, KIMI_INTEGRATION_VERSION),
        ("copilot", COPILOT_HOOK_ASSET, COPILOT_INTEGRATION_VERSION),
        ("devin", DEVIN_HOOK_ASSET, DEVIN_INTEGRATION_VERSION),
        ("droid", DROID_HOOK_ASSET, DROID_INTEGRATION_VERSION),
        (
            "opencode",
            OPENCODE_PLUGIN_ASSET,
            OPENCODE_INTEGRATION_VERSION,
        ),
        ("kilo", KILO_PLUGIN_ASSET, KILO_INTEGRATION_VERSION),
        (
            "hermes",
            HERMES_PLUGIN_INIT_ASSET,
            HERMES_INTEGRATION_VERSION,
        ),
        (
            "qodercli",
            QODERCLI_HOOK_ASSET,
            QODERCLI_INTEGRATION_VERSION,
        ),
        ("cursor", CURSOR_HOOK_ASSET, CURSOR_INTEGRATION_VERSION),
        (
            "antigravity_cli",
            ANTIGRAVITY_CLI_HOOK_ASSET,
            ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        ),
        (
            "mastracode",
            MASTRACODE_HOOK_ASSET,
            MASTRACODE_INTEGRATION_VERSION,
        ),
        ("grok", GROK_HOOK_ASSET, GROK_INTEGRATION_VERSION),
        ("qwen", QWEN_HOOK_ASSET, QWEN_INTEGRATION_VERSION),
    ] {
        assert_eq!(
            parse_integration_version(asset),
            Some(expected_version),
            "{name} asset version must match its integration version constant"
        );
    }
}

#[test]
fn process_owned_integration_assets_do_not_report_release() {
    for (name, asset) in [
        ("pi", PI_EXTENSION_ASSET),
        ("omp", OMP_EXTENSION_ASSET),
        ("mastracode", MASTRACODE_HOOK_ASSET),
        ("kimi", KIMI_HOOK_ASSET),
        ("kilo", KILO_PLUGIN_ASSET),
        ("hermes", HERMES_PLUGIN_INIT_ASSET),
    ] {
        assert!(
            !asset.contains("pane.release_agent"),
            "{name} process exit should own lifecycle release"
        );
    }
}

#[test]
fn pi_extension_refreshes_session_ref_before_agent_start_state() {
    let agent_start = PI_EXTENSION_ASSET
        .find("pi.on(\"agent_start\", (_event, ctx)")
        .expect("pi extension should receive agent_start context");
    let handler = &PI_EXTENSION_ASSET[agent_start..];
    let update_session = handler
        .find("updateSessionRef(ctx);")
        .expect("pi extension should refresh the active session on agent_start");
    let report_session = handler
        .find("void reportSession();")
        .expect("pi extension should report the refreshed session before state");
    let publish_state = handler
        .find("publishState();")
        .expect("pi extension should publish working state after refreshing session");

    assert!(update_session < report_session);
    assert!(report_session < publish_state);
}

#[test]
fn omp_extension_refreshes_session_ref_before_agent_start_state() {
    let agent_start = OMP_EXTENSION_ASSET
        .find("pi.on(\"agent_start\", (_event, ctx)")
        .expect("omp extension should receive agent_start context");
    let handler = &OMP_EXTENSION_ASSET[agent_start..];
    let update_session = handler
        .find("updateSessionRef(ctx);")
        .expect("omp extension should refresh the active session on agent_start");
    let report_session = handler
        .find("void reportSession();")
        .expect("omp extension should report the refreshed session before state");
    let publish_state = handler
        .find("publishState();")
        .expect("omp extension should publish working state after refreshing session");

    assert!(update_session < report_session);
    assert!(report_session < publish_state);
}

fn omp_handler(event: &str) -> &'static str {
    let start = OMP_EXTENSION_ASSET
        .find(&format!("pi.on(\"{event}\""))
        .unwrap_or_else(|| panic!("omp extension registers {event} handler"));
    let rest = &OMP_EXTENSION_ASSET[start..];
    let end = rest[1..]
        .find("\n\n  pi.")
        .map(|offset| offset + 1)
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn omp_root_activation_requires_ui_context() {
    let activator = OMP_EXTENSION_ASSET
        .find("function activateRootSession(ctx: any, sessionStartSource = \"startup\"): boolean")
        .expect("omp extension should centralize root session activation");
    let helper = &OMP_EXTENSION_ASSET[activator..];
    let non_ui_guard = helper
        .find("ctx?.hasUI !== true")
        .expect("omp extension checks UI context before activating");
    let root_session = helper
        .find("rootSession = true;")
        .expect("omp extension activates root session after UI guard");
    let session_report = helper
        .find("void reportSession(sessionStartSource);")
        .expect("omp extension reports root session");

    assert!(non_ui_guard < root_session);
    assert!(root_session < session_report);
}

#[test]
fn omp_session_start_and_switch_use_root_activation() {
    let session_start = OMP_EXTENSION_ASSET
        .find("pi.on(\"session_start\", (_event, ctx)")
        .expect("omp extension registers session_start handler");
    let session_start_handler = &OMP_EXTENSION_ASSET[session_start..];
    session_start_handler
        .find("if (!activateRootSession(ctx))")
        .expect("omp session_start handler should activate root session");

    let session_switch = OMP_EXTENSION_ASSET
        .find("pi.on(\"session_switch\", (event, ctx)")
        .expect("omp extension registers session_switch handler");
    let session_switch_handler = &OMP_EXTENSION_ASSET[session_switch..];
    session_switch_handler
        .find("if (!activateRootSession(ctx, event?.reason || \"resume\"))")
        .expect("omp session_switch handler should activate root session with switch reason");
}

#[test]
fn omp_session_reports_include_start_source() {
    let report_session = OMP_EXTENSION_ASSET
        .find("function reportSession(sessionStartSource = \"startup\"): Promise<void>")
        .expect("omp extension should label session reports with a lifecycle source");
    let helper = &OMP_EXTENSION_ASSET[report_session..];
    let session_source = helper
        .find("session_start_source: sessionStartSource")
        .expect("omp session reports should include the lifecycle source");
    let session_ref = helper
        .find("...sessionRef")
        .expect("omp session reports should include the native session ref");

    assert!(session_source < session_ref);
}

#[test]
fn omp_socket_requests_are_serialized() {
    let queue = OMP_EXTENSION_ASSET
        .find("let requestQueue = Promise.resolve();")
        .expect("omp extension should keep socket reports ordered");
    let send_request = OMP_EXTENSION_ASSET[queue..]
        .find("function sendRequest(request: unknown): Promise<void>")
        .expect("omp extension should wrap socket sends in an ordered queue");
    let queued_send = OMP_EXTENSION_ASSET[queue + send_request..]
        .find("requestQueue = requestQueue.then(")
        .expect("omp extension should serialize socket requests through the queue");
    let raw_send = OMP_EXTENSION_ASSET[queue + send_request..]
        .find("sendRequestNow(request)")
        .expect("omp extension should enqueue the raw socket send");

    assert!(queued_send < raw_send);
}

#[test]
fn omp_runtime_events_can_activate_root_session_after_resume() {
    for event in [
        "agent_start",
        "tool_approval_requested",
        "tool_approval_resolved",
        "tool_execution_start",
        "tool_execution_end",
    ] {
        let handler = omp_handler(event);
        handler
            .find("!rootSession && !activateRootSession(ctx)")
            .unwrap_or_else(|| panic!("omp {event} handler should recover missing root session"));
    }
}

#[test]
fn omp_ask_and_approval_events_report_blocked_state() {
    let approval_handler = omp_handler("tool_approval_requested");
    approval_handler
        .find("activateBlocked(label);")
        .expect("approval requests should block the pane");

    let approval_resolved = omp_handler("tool_approval_resolved");
    approval_resolved
        .find("deactivateBlocked();")
        .expect("approval resolution should unblock the pane");

    let ask_handler = omp_handler("tool_execution_start");
    ask_handler
        .find("event?.toolName !== \"ask\"")
        .expect("tool execution handler should only treat Ask as blocked");
    ask_handler
        .find("activateBlocked(askBlockedMessage(event.args));")
        .expect("Ask start should block the pane");

    let ask_end_handler = omp_handler("tool_execution_end");
    ask_end_handler
        .find("event?.toolName !== \"ask\"")
        .expect("tool execution end should only treat Ask as blocked");
    ask_end_handler
        .find("deactivateBlocked();")
        .expect("Ask end should unblock the pane");
}

#[test]
fn install_qodercli_writes_hook_and_updates_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).unwrap();
    fs::write(
        qoder_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .unwrap();
    std::env::set_var(QODERCLI_CONFIG_DIR_ENV_VAR, &qoder_dir);

    let installed = install_qodercli().unwrap();

    assert_eq!(
        installed.hook_path,
        qoder_dir.join("hooks").join(QODERCLI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.settings_path, qoder_dir.join("settings.json"));
    assert!(installed.hook_path.is_file());

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();
    let hooks = settings
        .get("hooks")
        .and_then(Value::as_object)
        .expect("hooks should be present");
    for (event, action) in QODERCLI_HOOK_EVENTS {
        assert!(
            hooks.contains_key(event),
            "expected hooks.{event} to be registered"
        );
        let command = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            command.contains(QODERCLI_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected qodercli {event} hook command to end with {action}, got {command}"
        );
    }
    // Pre-existing settings keys must be preserved.
    assert!(settings.get("permissions").is_some());

    std::env::remove_var(QODERCLI_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_qodercli_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).unwrap();
    std::env::set_var(QODERCLI_CONFIG_DIR_ENV_VAR, &qoder_dir);

    install_qodercli().unwrap();
    install_qodercli().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(qoder_dir.join("settings.json")).unwrap())
            .unwrap();
    let hooks = settings.get("hooks").and_then(Value::as_object).unwrap();
    for (event, _) in QODERCLI_HOOK_EVENTS {
        let entries = hooks.get(event).and_then(Value::as_array).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected hooks.{event} to contain exactly one entry, got {entries:?}"
        );
    }

    std::env::remove_var(QODERCLI_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_qodercli_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).unwrap();
    std::env::set_var(QODERCLI_CONFIG_DIR_ENV_VAR, &qoder_dir);

    install_qodercli().unwrap();
    // Inject a foreign hook entry the user might have configured by hand.
    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(qoder_dir.join("settings.json")).unwrap())
            .unwrap();
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "matcher": "*",
            "hooks": [{"type": "command", "command": "echo user-defined"}],
        }));
    fs::write(
        qoder_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_qodercli().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.updated_settings);

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(qoder_dir.join("settings.json")).unwrap())
            .unwrap();
    let hooks = settings.get("hooks").and_then(Value::as_object).unwrap();
    let remaining = hooks.get("SessionStart").and_then(Value::as_array).unwrap();
    assert_eq!(remaining.len(), 1);
    let cmd = remaining[0]["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(cmd, "echo user-defined");

    std::env::remove_var(QODERCLI_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_qodercli_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let missing = base.join(".qoder");
    std::env::set_var(QODERCLI_CONFIG_DIR_ENV_VAR, &missing);

    let err = install_qodercli().unwrap_err().to_string();
    assert!(
        err.contains("qodercli config directory not found"),
        "unexpected error: {err}"
    );

    std::env::remove_var(QODERCLI_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_qwen_writes_session_hook_and_preserves_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let qwen_dir = base.join(".qwen");
    fs::create_dir_all(&qwen_dir).unwrap();
    fs::write(
        qwen_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .unwrap();
    std::env::set_var(QWEN_HOME_ENV_VAR, &qwen_dir);

    let installed = install_qwen().unwrap();

    assert_eq!(
        installed.hook_path,
        qwen_dir.join("hooks").join(QWEN_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.settings_path, qwen_dir.join("settings.json"));
    assert!(installed.hook_path.is_file());

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();
    let entries = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["matcher"], "*");
    assert_eq!(entries[0]["hooks"][0]["timeout"], 10_000);
    let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains(QWEN_HOOK_INSTALL_NAME));
    assert!(command.ends_with("session"));
    let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop_entries.len(), 1);
    assert_eq!(stop_entries[0]["hooks"][0]["timeout"], 10_000);
    let stop_command = stop_entries[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(stop_command.contains(QWEN_HOOK_INSTALL_NAME));
    assert!(stop_command.ends_with(" usage"));
    assert!(settings.get("permissions").is_some());
    let hook_asset = fs::read_to_string(&installed.hook_path).unwrap();
    assert!(hook_asset.contains("HERDR_INTEGRATION_ID=qwen"));
    assert!(hook_asset.contains("HERDR_INTEGRATION_VERSION=2"));
    assert!(hook_asset.contains("herdr:qwen"));

    install_qwen().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"].as_array().unwrap().len(),
        1
    );
    assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);

    std::env::remove_var(QWEN_HOME_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_qwen_removes_only_herdr_hook() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let qwen_dir = base.join(".qwen");
    fs::create_dir_all(&qwen_dir).unwrap();
    std::env::set_var(QWEN_HOME_ENV_VAR, &qwen_dir);

    install_qwen().unwrap();
    let settings_path = qwen_dir.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "matcher": "resume",
            "hooks": [{"type": "command", "command": "echo user-defined"}],
        }));
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let result = uninstall_qwen().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.updated_settings);

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    let remaining = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["hooks"][0]["command"], "echo user-defined");
    assert!(
        settings["hooks"]
            .get("Stop")
            .and_then(Value::as_array)
            .is_none_or(|entries| entries.is_empty()),
        "the usage hook should be removed"
    );

    std::env::remove_var(QWEN_HOME_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_qwen_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let missing = base.join(".qwen");
    std::env::set_var(QWEN_HOME_ENV_VAR, &missing);

    let err = install_qwen().unwrap_err().to_string();
    assert!(err.contains("qwen code config directory not found"));

    std::env::remove_var(QWEN_HOME_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_and_uninstall_letta_preserve_unrelated_settings_and_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let letta_dir = home.join(".letta");
    fs::create_dir_all(&letta_dir).unwrap();
    let settings_path = letta_dir.join("settings.json");
    fs::write(
        &settings_path,
        r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo user"}]}]}}"#,
    )
    .unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);

    let installed = install_letta().unwrap();
    assert_eq!(
        installed.hook_path,
        letta_dir.join("hooks").join(LETTA_HOOK_INSTALL_NAME)
    );
    let first_install = fs::read_to_string(&settings_path).unwrap();
    let settings: Value = serde_json::from_str(&first_install).unwrap();
    let entries = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["hooks"][0]["command"], "echo user");
    assert!(entries[1].get("matcher").is_none());
    assert_eq!(entries[1]["hooks"][0]["timeout"], LETTA_HOOK_TIMEOUT_MS);
    assert_eq!(entries[1]["hooks"][0]["quiet"], true);
    assert!(entries[1]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("session"));
    assert_eq!(settings["theme"], "dark");

    install_letta().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), first_install);

    let result = uninstall_letta().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.updated_settings);
    assert!(!installed.hook_path.exists());
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["theme"], "dark");
    let remaining = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["hooks"][0]["command"], "echo user");

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn letta_session_hook_is_silent_and_encodes_default_conversation() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(home.join(".letta")).unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let installed = install_letta().unwrap();

    let capture = base.join("args.txt");
    let fake_herdr = base.join("herdr");
    fs::write(
        &fake_herdr,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\n",
            capture.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_herdr, permissions).unwrap();

    let mut child = Command::new("sh")
        .arg(&installed.hook_path)
        .arg("session")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w1:p2")
        .env("HERDR_SOCKET_PATH", "/tmp/herdr.sock")
        .env("HERDR_BIN_PATH", &fake_herdr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"event_type":"SessionStart","conversation_id":"default","agent_id":"agent-123","is_new_session":false}"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let args = fs::read_to_string(capture).unwrap();
    assert!(args.contains("report-agent-session w1:p2"));
    assert!(args.contains("--source herdr:letta --agent letta"));
    assert!(args.contains("--agent-session-id default:agent-123"));
    assert!(args.contains("--session-start-source resume"));

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_letta_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);

    let err = install_letta().unwrap_err().to_string();
    assert!(err.contains("letta code config directory not found"));

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_letta_does_not_publish_hook_when_settings_are_invalid() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let letta_dir = home.join(".letta");
    fs::create_dir_all(&letta_dir).unwrap();
    fs::write(letta_dir.join("settings.json"), "not json").unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);

    assert!(install_letta().is_err());
    assert!(!letta_dir
        .join("hooks")
        .join(LETTA_HOOK_INSTALL_NAME)
        .exists());

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn letta_staged_install_can_restore_the_prior_file() {
    let base = unique_base();
    fs::create_dir_all(&base).unwrap();
    let target = base.join("settings.json");
    fs::write(&target, "old").unwrap();

    let (staged, backup) = prepare_letta_install_file(&target, b"new", false, true).unwrap();
    let had_original = publish_letta_install_file(&target, &staged, &backup).unwrap();
    assert!(had_original);
    assert_eq!(fs::read_to_string(&target).unwrap(), "new");

    rollback_letta_install_file(&target, &backup, had_original).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    assert!(!backup.exists());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_cursor_writes_hook_and_updates_hooks_json() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    fs::write(
        cursor_dir.join("hooks.json"),
        r#"{"version":1,"hooks":{"stop":[{"command":"echo keep-me"}]}}"#,
    )
    .unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);

    let installed = install_cursor().unwrap();

    assert_eq!(
        installed.hook_path,
        cursor_dir.join(CURSOR_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.hooks_path, cursor_dir.join("hooks.json"));
    assert_eq!(
        fs::read_to_string(&installed.hook_path).unwrap(),
        CURSOR_HOOK_ASSET
    );

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(cursor_dir.join("hooks.json")).unwrap()).unwrap();
    let hooks = hooks_file.get("hooks").and_then(Value::as_object).unwrap();
    let session_start = hooks.get("sessionStart").and_then(Value::as_array).unwrap();
    assert_eq!(session_start.len(), 1);
    assert_eq!(
        session_start[0].get("command").and_then(Value::as_str),
        Some(hook_command(&installed.hook_path, Some("session")).as_str())
    );
    assert!(hooks.get("beforeSubmitPrompt").is_none());
    assert!(hooks.get("beforeShellExecution").is_none());
    let stop = hooks.get("stop").and_then(Value::as_array).unwrap();
    assert_eq!(stop.len(), 1);
    assert_eq!(
        stop[0].get("command").and_then(Value::as_str),
        Some("echo keep-me")
    );

    std::env::remove_var(CURSOR_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_cursor_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);

    install_cursor().unwrap();
    install_cursor().unwrap();

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(cursor_dir.join("hooks.json")).unwrap()).unwrap();
    let hooks = hooks_file.get("hooks").and_then(Value::as_object).unwrap();
    let session_start = hooks.get("sessionStart").and_then(Value::as_array).unwrap();
    assert_eq!(session_start.len(), 1);

    std::env::remove_var(CURSOR_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_cursor_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);

    install_cursor().unwrap();
    let mut hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(cursor_dir.join("hooks.json")).unwrap()).unwrap();
    hooks_file["hooks"]["beforeSubmitPrompt"] = json!([{ "command": "echo user-defined" }]);
    fs::write(
        cursor_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_file).unwrap(),
    )
    .unwrap();

    let result = uninstall_cursor().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.updated_hooks);
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).is_file());

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(cursor_dir.join("hooks.json")).unwrap()).unwrap();
    let hooks = hooks_file.get("hooks").and_then(Value::as_object).unwrap();
    assert!(!hooks.contains_key("sessionStart"));
    assert!(hooks.contains_key("beforeSubmitPrompt"));

    std::env::remove_var(CURSOR_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_cursor_uses_cursor_config_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = base.join("custom-cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);

    let installed = install_cursor().unwrap();

    assert_eq!(
        installed.hook_path,
        cursor_dir.join(CURSOR_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.hooks_path, cursor_dir.join("hooks.json"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn cursor_v1_integration_status_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    let hook_path = cursor_dir.join(CURSOR_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=cursor\n# HERDR_INTEGRATION_VERSION=1\n",
    )
    .unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);

    let cursor_status = || {
        installed_integration_statuses()
            .into_iter()
            .find(|status| status.target == crate::api::schema::IntegrationTarget::Cursor)
            .expect("cursor integration status")
    };
    let outdated = cursor_status();
    assert_eq!(outdated.state, IntegrationStatusKind::Outdated);
    assert_eq!(outdated.installed_version, Some(1));
    assert_eq!(outdated.expected_version, CURSOR_INTEGRATION_VERSION);

    install_cursor().unwrap();
    let current = cursor_status();
    assert_eq!(current.state, IntegrationStatusKind::Current);
    assert_eq!(current.installed_version, Some(2));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
const CURSOR_USER_STATUSLINE_COMMAND: &str = "~/.cursor/statusline.sh";

#[cfg(not(windows))]
const CURSOR_CLI_CONFIG_WITH_STATUSLINE: &str = concat!(
    "{\n",
    "  // Cursor's own CLI config\n",
    "  \"version\": 1,\n",
    "  \"editor\": { \"vimMode\": true },\n",
    "  \"statusLine\": {\n",
    "    \"type\": \"command\",\n",
    "    \"command\": \"~/.cursor/statusline.sh\",\n",
    "    \"padding\": 2,\n",
    "    \"updateIntervalMs\": 500,\n",
    "    \"timeoutMs\": 2000\n",
    "  },\n",
    "  \"permissions\": { \"allow\": [\"Shell(ls)\"] }\n",
    "}\n",
);

/// Seeds a Cursor config dir under `base` (selected through `CURSOR_CONFIG_DIR`)
/// and writes `cli-config.json` when given.
#[cfg(not(windows))]
fn seed_cursor_dir(base: &Path, name: &str, cli_config: Option<&str>) -> PathBuf {
    let cursor_dir = base.join(name);
    fs::create_dir_all(&cursor_dir).unwrap();
    if let Some(cli_config) = cli_config {
        fs::write(cursor_dir.join("cli-config.json"), cli_config).unwrap();
    }
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &cursor_dir);
    cursor_dir
}

#[cfg(not(windows))]
fn cursor_statusline_config(command: &str) -> String {
    CURSOR_CLI_CONFIG_WITH_STATUSLINE.replace(
        "\"~/.cursor/statusline.sh\"",
        &serde_json::to_string(command).unwrap(),
    )
}

#[cfg(not(windows))]
fn read_cursor_cli_config(cursor_dir: &Path) -> Value {
    // The seeded config carries a comment, so it is not plain JSON.
    jsonc_parser::cst::CstRootNode::parse(
        &fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap()
    .to_serde_value()
    .unwrap()
}

#[cfg(not(windows))]
#[test]
fn install_cursor_wraps_an_existing_statusline_and_uninstall_restores_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    let tap_path = cursor_dir.join("herdr-statusline-tap.sh");
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");

    install_cursor().unwrap();
    let installed_bytes = fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap();
    let config = read_cursor_cli_config(&cursor_dir);

    assert_eq!(
        config["statusLine"]["command"].as_str().unwrap(),
        wrapper_path.display().to_string(),
        "the settings command is the bare wrapper path"
    );
    assert_eq!(
        installed_bytes,
        CURSOR_CLI_CONFIG_WITH_STATUSLINE.replace(
            "\"~/.cursor/statusline.sh\"",
            &format!("\"{}\"", wrapper_path.display())
        ),
        "only the command string changes"
    );
    assert_eq!(
        fs::read_to_string(&tap_path).unwrap(),
        CURSOR_STATUSLINE_TAP_ASSET
    );
    assert!(fs::read_to_string(&wrapper_path)
        .unwrap()
        .contains(CURSOR_USER_STATUSLINE_COMMAND));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o111;
        assert_ne!(mode(&wrapper_path), 0, "the wrapper must be executable");
        assert_ne!(mode(&tap_path), 0, "the tap must be executable");
    }

    install_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        installed_bytes,
        "reinstall must not wrap twice"
    );

    uninstall_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        CURSOR_CLI_CONFIG_WITH_STATUSLINE
    );
    assert!(!tap_path.exists());
    assert!(!wrapper_path.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_cursor_without_cli_config_creates_none() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", None);

    install_cursor().unwrap();

    assert!(!cursor_dir.join("cli-config.json").exists());
    assert!(!cursor_dir.join("herdr-statusline-wrap.sh").exists());
    uninstall_cursor().unwrap();
    assert!(!cursor_dir.join("cli-config.json").exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_cursor_leaves_cli_config_without_statusline_untouched() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let seeded =
        "{\n  // no statusline here\n  \"version\": 1,\n  \"editor\": { \"vimMode\": true }\n}\n";
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(seeded));

    install_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        seeded
    );
    assert!(!cursor_dir.join("herdr-statusline-wrap.sh").exists());

    uninstall_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        seeded
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_cursor_leaves_other_statusline_shapes_alone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    for statusline in [
        json!("bash ~/x.sh"),
        json!({"type": "static", "command": "bash ~/x.sh"}),
        json!({"type": "command", "command": 5}),
        json!({"type": "command", "command": ""}),
        json!({"type": "command"}),
    ] {
        let seeded = json!({"statusLine": statusline.clone()}).to_string();
        let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(&seeded));

        install_cursor().unwrap();
        assert_eq!(
            fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
            seeded
        );
        uninstall_cursor().unwrap();
        assert_eq!(
            fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
            seeded
        );
        let _ = fs::remove_dir_all(&cursor_dir);
    }

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_statusline_wrapper_passes_stdin_stdout_and_exit_code_through() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original = "printf 'OUT:'; cat; exit 3";
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(&cursor_statusline_config(original)));
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");
    install_cursor().unwrap();
    let stdin = r#"{"context_window":{"total_input_tokens":84000}}"#;
    let expected = (format!("OUT:{stdin}"), 3);

    // Executed directly (no shell in front of it), then through a shell.
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );
    let mut through_sh = std::process::Command::new("sh");
    through_sh.arg("-c").arg(wrapper_path.display().to_string());
    assert_eq!(run_statusline_command(through_sh, stdin), expected);

    // A deleted tap file still runs the original.
    fs::remove_file(cursor_dir.join("herdr-statusline-tap.sh")).unwrap();
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_statusline_wrapper_keeps_quotes_dollars_and_backslashes_intact() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original = r#"printf '%s' "q'q \$V \\ b""#;
    let direct = {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(original);
        run_statusline_command(command, "")
    };
    assert_eq!(direct.0, r"q'q $V \ b");
    let seeded = cursor_statusline_config(original);
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(&seeded));
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");

    install_cursor().unwrap();

    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), "{}"),
        direct
    );
    uninstall_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        seeded
    );
    assert_eq!(
        read_cursor_cli_config(&cursor_dir)["statusLine"]["command"],
        original
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_cursor_repoints_a_wrapper_from_another_directory_instead_of_nesting() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let first_dir = seed_cursor_dir(&base, "first", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    // The config directory moved: same config, the old wrapper still on disk.
    let second_dir = base.join("second");
    fs::create_dir_all(&second_dir).unwrap();
    fs::copy(
        first_dir.join("cli-config.json"),
        second_dir.join("cli-config.json"),
    )
    .unwrap();
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &second_dir);

    install_cursor().unwrap();

    let second_wrapper = second_dir.join("herdr-statusline-wrap.sh");
    assert_eq!(
        read_cursor_cli_config(&second_dir)["statusLine"]["command"]
            .as_str()
            .unwrap(),
        second_wrapper.display().to_string()
    );
    uninstall_cursor().unwrap();
    assert_eq!(
        fs::read_to_string(second_dir.join("cli-config.json")).unwrap(),
        CURSOR_CLI_CONFIG_WITH_STATUSLINE
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_statusline_is_not_wrapped_when_the_wrapper_path_needs_quoting() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, "cursor dir", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));

    install_cursor().unwrap();

    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        CURSOR_CLI_CONFIG_WITH_STATUSLINE
    );
    assert!(!cursor_dir.join("herdr-statusline-wrap.sh").exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_restores_from_the_backup_when_the_wrapper_is_gone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    fs::remove_file(cursor_dir.join("herdr-statusline-wrap.sh")).unwrap();

    let result = uninstall_cursor().unwrap();

    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        CURSOR_CLI_CONFIG_WITH_STATUSLINE
    );
    assert!(!cursor_dir.join("herdr-statusline-original.json").exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_removes_hooks_and_warns_when_wrapper_and_backup_are_gone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    let installed_config = fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap();
    fs::remove_file(cursor_dir.join("herdr-statusline-wrap.sh")).unwrap();
    fs::remove_file(cursor_dir.join("herdr-statusline-original.json")).unwrap();

    let result = uninstall_cursor().unwrap();

    assert!(result.updated_hooks, "the hook entries are removed");
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert!(
        result.warnings[0].starts_with(INSTALL_WARNING_PREFIX),
        "{:?}",
        result.warnings
    );
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        installed_config,
        "the command is never guessed"
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_refuses_to_guess_when_the_wrapper_is_unreadable_and_unbacked() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    let installed_bytes = fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap();
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");
    fs::write(&wrapper_path, "#!/bin/sh\n").unwrap();
    fs::remove_file(cursor_dir.join("herdr-statusline-original.json")).unwrap();

    let err = uninstall_cursor().unwrap_err().to_string();

    assert!(err.contains("statusLine.command"), "{err}");
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        installed_bytes
    );
    assert!(wrapper_path.exists(), "the only copy is never removed");
    assert!(cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_install_and_uninstall_survive_an_unparseable_cli_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let broken = "{ \"statusLine\": ";
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(broken));

    let installed = install_cursor().unwrap();

    assert!(installed.hooks_path.is_file(), "the hooks still install");
    assert!(cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).is_file());
    assert_eq!(installed.warnings.len(), 1, "{:?}", installed.warnings);
    assert!(installed.warnings[0].starts_with(INSTALL_WARNING_PREFIX));
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        broken
    );

    let result = uninstall_cursor().unwrap();

    assert!(result.updated_hooks);
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        broken
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_removes_hooks_when_the_cli_config_is_not_utf8() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", None);
    install_cursor().unwrap();
    let bytes = b"{ \"statusLine\": \"\xff\xfe\" }";
    fs::write(cursor_dir.join("cli-config.json"), bytes).unwrap();

    let result = uninstall_cursor().unwrap();

    assert!(result.updated_hooks);
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert!(result.warnings[0].starts_with(INSTALL_WARNING_PREFIX));
    assert_eq!(fs::read(cursor_dir.join("cli-config.json")).unwrap(), bytes);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

/// Installs Cursor over a wrapped statusline and returns the Cursor dir with
/// its wrapper and backup paths.
#[cfg(not(windows))]
fn install_cursor_with_wrapped_statusline(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let cursor_dir = seed_cursor_dir(base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");
    let backup_path = cursor_dir.join("herdr-statusline-original.json");
    assert!(wrapper_path.exists() && backup_path.exists());
    (cursor_dir, wrapper_path, backup_path)
}

/// The hooks are gone while the wrapper and its backup are kept with a warning.
#[cfg(not(windows))]
fn assert_cursor_uninstall_kept_the_wrapper(
    result: &CursorUninstallResult,
    cursor_dir: &Path,
    wrapper_path: &Path,
    backup_path: &Path,
) {
    assert!(result.updated_hooks, "the hook entries are removed");
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());
    assert!(!cursor_dir.join("herdr-statusline-tap.sh").exists());
    assert!(
        wrapper_path.exists(),
        "the config may still name the wrapper"
    );
    assert!(
        backup_path.exists(),
        "the config may still name the wrapper"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains(&backup_path.display().to_string())),
        "{:?}",
        result.warnings
    );
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_keeps_the_wrapper_and_backup_when_a_non_utf8_config_names_the_wrapper() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let (cursor_dir, wrapper_path, backup_path) = install_cursor_with_wrapped_statusline(&base);
    let config_path = cursor_dir.join("cli-config.json");
    let mut bytes = fs::read(&config_path).unwrap();
    bytes.extend_from_slice(b"// \xff\xfe\n");
    fs::write(&config_path, &bytes).unwrap();

    let result = uninstall_cursor().unwrap();

    assert_cursor_uninstall_kept_the_wrapper(&result, &cursor_dir, &wrapper_path, &backup_path);
    assert_eq!(fs::read(&config_path).unwrap(), bytes);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_keeps_the_wrapper_and_backup_when_the_cli_config_is_a_dangling_symlink() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let (cursor_dir, wrapper_path, backup_path) = install_cursor_with_wrapped_statusline(&base);
    let config_path = cursor_dir.join("cli-config.json");
    let target = base.join("unmounted-dotfiles").join("cli-config.json");
    fs::remove_file(&config_path).unwrap();
    std::os::unix::fs::symlink(&target, &config_path).unwrap();

    let result = uninstall_cursor().unwrap();

    assert_cursor_uninstall_kept_the_wrapper(&result, &cursor_dir, &wrapper_path, &backup_path);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_keeps_the_wrapper_and_backup_when_the_cli_config_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = integration_env_lock();
    let base = unique_base();
    let (cursor_dir, wrapper_path, backup_path) = install_cursor_with_wrapped_statusline(&base);
    let config_path = cursor_dir.join("cli-config.json");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads the file anyway, so the case cannot be set up.
    if fs::read(&config_path).is_err() {
        let result = uninstall_cursor().unwrap();

        assert_cursor_uninstall_kept_the_wrapper(&result, &cursor_dir, &wrapper_path, &backup_path);
    }

    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644)).unwrap();
    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_keeps_the_wrapper_and_backup_when_the_statusline_shape_is_unrecognised() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let (cursor_dir, wrapper_path, backup_path) = install_cursor_with_wrapped_statusline(&base);
    let config_path = cursor_dir.join("cli-config.json");
    let installed = fs::read_to_string(&config_path).unwrap();
    // A duplicated key is ambiguous, so the restore leaves the statusline alone.
    let duplicated = installed.replace(
        "\"padding\": 2,",
        &format!(
            "\"command\": \"{}\",\n    \"padding\": 2,",
            wrapper_path.display()
        ),
    );
    assert_ne!(duplicated, installed);
    fs::write(&config_path, &duplicated).unwrap();

    let result = uninstall_cursor().unwrap();

    assert_cursor_uninstall_kept_the_wrapper(&result, &cursor_dir, &wrapper_path, &backup_path);
    assert_eq!(fs::read_to_string(&config_path).unwrap(), duplicated);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_install_skips_the_statusline_of_a_hard_linked_cli_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    fs::hard_link(cursor_dir.join("cli-config.json"), base.join("linked.json")).unwrap();

    let installed = install_cursor().unwrap();

    assert!(installed.hooks_path.is_file(), "the hooks still install");
    assert_eq!(installed.warnings.len(), 1, "{:?}", installed.warnings);
    assert_eq!(
        fs::read_to_string(cursor_dir.join("cli-config.json")).unwrap(),
        CURSOR_CLI_CONFIG_WITH_STATUSLINE
    );
    assert!(!cursor_dir.join("herdr-statusline-wrap.sh").exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn uninstall_cursor_stops_on_an_unparseable_cli_config_that_names_the_wrapper() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(CURSOR_CLI_CONFIG_WITH_STATUSLINE));
    install_cursor().unwrap();
    let config_path = cursor_dir.join("cli-config.json");
    let broken = fs::read_to_string(&config_path).unwrap() + "}";
    fs::write(&config_path, &broken).unwrap();

    uninstall_cursor().unwrap_err();

    assert_eq!(fs::read_to_string(&config_path).unwrap(), broken);
    assert!(cursor_dir.join("herdr-statusline-wrap.sh").exists());
    assert!(cursor_dir.join("herdr-statusline-original.json").exists());
    assert!(cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn cursor_statusline_wrapper_runs_an_executable_path_with_a_space_directly() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let script_dir = base.join("Application Support");
    fs::create_dir_all(&script_dir).unwrap();
    let script = script_dir.join("line.sh");
    fs::write(&script, "#!/bin/sh\nprintf 'OUT:'; cat; exit 3\n").unwrap();
    make_executable(&script).unwrap();
    let original = script.display().to_string();
    let cursor_dir = seed_cursor_dir(&base, ".cursor", Some(&cursor_statusline_config(&original)));
    let wrapper_path = cursor_dir.join("herdr-statusline-wrap.sh");
    install_cursor().unwrap();
    let stdin = r#"{"context_window":{"total_input_tokens":84000}}"#;
    let expected = (format!("OUT:{stdin}"), 3);

    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );
    fs::remove_file(cursor_dir.join("herdr-statusline-tap.sh")).unwrap();
    assert_eq!(
        run_statusline_command(std::process::Command::new(&wrapper_path), stdin),
        expected
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_cursor_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let missing = base.join(".cursor");
    std::env::set_var(CURSOR_CONFIG_DIR_ENV_VAR, &missing);

    let err = install_cursor().unwrap_err().to_string();
    assert!(
        err.contains("cursor config directory not found"),
        "unexpected error: {err}"
    );

    std::env::remove_var(CURSOR_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_mastracode_writes_hook_and_updates_hooks_json() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).unwrap();
    fs::write(
        mastracode_dir.join("hooks.json"),
        r#"{"PostToolUse":[{"type":"command","command":"echo keep-me"}]}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &base);

    let installed = install_mastracode().unwrap();

    assert_eq!(
        installed.hook_path,
        mastracode_dir
            .join("hooks")
            .join(MASTRACODE_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.hooks_path, mastracode_dir.join("hooks.json"));
    assert_eq!(
        fs::read_to_string(&installed.hook_path).unwrap(),
        MASTRACODE_HOOK_ASSET
    );

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(mastracode_dir.join("hooks.json")).unwrap())
            .unwrap();
    let hooks = hooks_file.as_object().unwrap();
    for (event, action) in MASTRACODE_HOOK_EVENTS {
        let entries = hooks.get(event).and_then(Value::as_array).unwrap();
        assert_eq!(entries.len(), 1, "{event} should have one Herdr hook");
        let command = entries[0].get("command").and_then(Value::as_str).unwrap();
        assert_eq!(
            command,
            mastracode_hook_command(&installed.hook_path, action)
        );
        assert_eq!(
            entries[0].get("type").and_then(Value::as_str),
            Some("command")
        );
        assert_eq!(
            entries[0].get("timeout").and_then(Value::as_u64),
            Some(MASTRACODE_HOOK_TIMEOUT_MS)
        );
    }
    assert_eq!(
        hooks["PostToolUse"][0]
            .get("command")
            .and_then(Value::as_str),
        Some("echo keep-me")
    );

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

fn grok_session_command(config: &Value) -> String {
    config["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("grok SessionStart command")
        .to_string()
}

#[test]
fn install_grok_writes_hook_and_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);

    let installed = install_grok().unwrap();

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(installed.hook_path, hooks_dir.join(GROK_HOOK_INSTALL_NAME));
    assert_eq!(
        installed.config_path,
        hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(&installed.hook_path).unwrap(),
        GROK_HOOK_ASSET
    );

    let config: Value =
        serde_json::from_str(&fs::read_to_string(&installed.config_path).unwrap()).unwrap();
    assert_eq!(config, grok_hook_config(&installed.hook_path));
    let session_start = config["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(session_start.len(), 1);
    let command = grok_session_command(&config);
    #[cfg(windows)]
    assert_eq!(command, hook_command(&installed.hook_path, Some("session")));
    #[cfg(not(windows))]
    {
        assert!(command.starts_with("sh "));
        assert!(command.contains("herdr-agent-state.sh"));
        assert!(command.ends_with(" session"));
    }

    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_mastracode_removes_v1_lifecycle_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    let mastracode_dir = base.join(".mastracode");
    let hook_path = mastracode_dir
        .join("hooks")
        .join(MASTRACODE_HOOK_INSTALL_NAME);
    fs::create_dir_all(&mastracode_dir).unwrap();
    fs::write(
        mastracode_dir.join("hooks.json"),
        serde_json::to_string(&json!({
            "SessionStart": [{
                "type": "command",
                "command": hook_command(&hook_path, Some("idle")),
                "timeout": MASTRACODE_HOOK_TIMEOUT_MS
            }],
            "SessionEnd": [{
                "type": "command",
                "command": hook_command(&hook_path, Some("release")),
                "timeout": MASTRACODE_HOOK_TIMEOUT_MS
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &base);

    install_mastracode().unwrap();

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(mastracode_dir.join("hooks.json")).unwrap())
            .unwrap();
    let hooks = hooks_file.as_object().unwrap();
    assert!(!hooks.contains_key("SessionEnd"));
    let session_start = hooks["SessionStart"].as_array().unwrap();
    assert_eq!(session_start.len(), 1);
    assert_eq!(
        session_start[0]["command"].as_str().unwrap(),
        mastracode_hook_command(&hook_path, "session")
    );

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_mastracode_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &base);

    install_mastracode().unwrap();
    install_mastracode().unwrap();

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(base.join(".mastracode").join("hooks.json")).unwrap(),
    )
    .unwrap();
    let hooks = hooks_file.as_object().unwrap();
    for (event, _) in MASTRACODE_HOOK_EVENTS {
        assert_eq!(hooks.get(event).and_then(Value::as_array).unwrap().len(), 1);
    }

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_grok_is_idempotent() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);

    install_grok().unwrap();
    let first =
        fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME)).unwrap();
    install_grok().unwrap();
    let second =
        fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME)).unwrap();
    assert_eq!(first, second);

    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_mastracode_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &base);

    install_mastracode().unwrap();
    let hooks_path = base.join(".mastracode").join("hooks.json");
    let mut hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    hooks_file["UserPromptSubmit"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "type": "command", "command": "echo user-defined" }));
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&hooks_file).unwrap(),
    )
    .unwrap();

    let result = uninstall_mastracode().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.updated_hooks);
    assert!(!base
        .join(".mastracode")
        .join("hooks")
        .join(MASTRACODE_HOOK_INSTALL_NAME)
        .is_file());

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let hooks = hooks_file.as_object().unwrap();
    for (event, _) in MASTRACODE_HOOK_EVENTS {
        if event == "UserPromptSubmit" {
            continue;
        }
        assert!(!hooks.contains_key(event), "{event} should be removed");
    }
    let user_prompt_submit = hooks
        .get("UserPromptSubmit")
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(user_prompt_submit.len(), 1);
    assert_eq!(
        user_prompt_submit[0].get("command").and_then(Value::as_str),
        Some("echo user-defined")
    );

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_grok_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    // Deliberately do not create the ~/.grok directory ahead of time: the
    // installer must refuse instead of conjuring a config dir for an agent
    // that is not installed.
    let missing = base.join(".grok");
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &missing);

    let err = install_grok().unwrap_err().to_string();
    assert!(
        err.contains("grok config directory not found"),
        "unexpected error: {err}"
    );

    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_grok_removes_files() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);

    install_grok().unwrap();
    let result = uninstall_grok().unwrap();
    assert!(result.removed_hook_file);
    assert!(result.removed_config_file);
    assert!(!result.hook_path.is_file());
    assert!(!result.config_path.is_file());

    // Uninstalling again is a no-op.
    let again = uninstall_grok().unwrap();
    assert!(!again.removed_hook_file);
    assert!(!again.removed_config_file);

    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_grok_uses_grok_config_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join("custom-grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);

    let installed = install_grok().unwrap();

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(installed.hook_path, hooks_dir.join(GROK_HOOK_INSTALL_NAME));
    assert_eq!(
        installed.config_path,
        hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME)
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_mastracode_errors_when_event_value_not_array() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).unwrap();
    fs::write(mastracode_dir.join("hooks.json"), r#"{"SessionStart":{}}"#).unwrap();
    std::env::set_var("HOME", &base);

    let err = install_mastracode().unwrap_err().to_string();
    assert!(
        err.contains("hook entries for SessionStart must be an array"),
        "unexpected error: {err}"
    );

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_mastracode_errors_when_event_value_not_array() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).unwrap();
    fs::write(mastracode_dir.join("hooks.json"), r#"{"SessionStart":{}}"#).unwrap();
    std::env::set_var("HOME", &base);

    let err = uninstall_mastracode().unwrap_err().to_string();
    assert!(
        err.contains("hook entries for SessionStart must be an array"),
        "unexpected error: {err}"
    );

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

/// Points the Antigravity statusline settings lookup at a directory under `base`
/// so no test reads or writes the real home directory.
fn isolate_antigravity_cli_settings_dir(base: &Path) -> PathBuf {
    let settings_dir = base.join(".gemini").join("antigravity-cli");
    std::env::set_var(ANTIGRAVITY_CLI_SETTINGS_DIR_ENV_VAR, &settings_dir);
    settings_dir
}

#[cfg(not(windows))]
const ANTIGRAVITY_USER_STATUSLINE_COMMAND: &str = "bash ~/agy-statusline.sh";

#[cfg(not(windows))]
#[test]
fn install_antigravity_cli_wraps_an_existing_statusline_and_uninstall_restores_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    let original_bytes = format!(
        "{{\n  \"theme\": \"dark\",\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": \"{ANTIGRAVITY_USER_STATUSLINE_COMMAND}\",\n    \"padding\": 1,\n    \"enabled\": true,\n    \"stack_with_default\": false\n  }}\n}}\n"
    );
    fs::write(&settings_path, &original_bytes).unwrap();
    let tap_path = agy_dir.join("hooks").join("herdr-statusline-tap.sh");

    install_antigravity_cli().unwrap();
    let installed_bytes = fs::read_to_string(&settings_path).unwrap();
    let settings: Value = serde_json::from_str(&installed_bytes).unwrap();
    let command = settings["statusLine"]["command"].as_str().unwrap();

    assert_eq!(
        super::statusline_tap::unwrap_statusline_command(command),
        Some(ANTIGRAVITY_USER_STATUSLINE_COMMAND.to_string())
    );
    assert!(
        command.contains(&tap_path.display().to_string()),
        "{command}"
    );
    assert_eq!(settings["statusLine"]["padding"], 1);
    assert_eq!(settings["statusLine"]["enabled"], true);
    assert_eq!(settings["statusLine"]["stack_with_default"], false);
    assert_eq!(settings["theme"], "dark");
    assert_eq!(
        fs::read_to_string(&tap_path).unwrap(),
        ANTIGRAVITY_CLI_STATUSLINE_TAP_ASSET
    );

    install_antigravity_cli().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), installed_bytes);

    uninstall_antigravity_cli().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), original_bytes);
    assert!(!tap_path.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_antigravity_cli_leaves_settings_without_statusline_untouched() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    let original_bytes = "{\n  \"theme\": \"dark\"\n}\n";
    fs::write(&settings_path, original_bytes).unwrap();

    install_antigravity_cli().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), original_bytes);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert!(settings.get("statusLine").is_none());

    uninstall_antigravity_cli().unwrap();
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), original_bytes);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn install_antigravity_cli_without_settings_file_still_succeeds() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    // Neither the settings directory nor the file exists.
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);

    let installed = install_antigravity_cli().unwrap();

    assert!(installed.hook_path.is_file());
    assert!(!settings_dir.exists());
    uninstall_antigravity_cli().unwrap();
    assert!(!settings_dir.exists());

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn antigravity_cli_install_and_uninstall_survive_an_unparseable_statusline_settings_file() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    let broken = "{ \"theme\": ";
    fs::write(&settings_path, broken).unwrap();

    let installed = install_antigravity_cli().unwrap();

    assert!(installed.hooks_path.is_file(), "the hooks still install");
    assert_eq!(installed.warnings.len(), 1, "{:?}", installed.warnings);
    assert!(installed.warnings[0].starts_with(INSTALL_WARNING_PREFIX));
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), broken);

    let result = uninstall_antigravity_cli().unwrap();

    assert!(result.updated_hooks);
    assert!(result.removed_hook_file);
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert_eq!(fs::read_to_string(&settings_path).unwrap(), broken);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn antigravity_cli_uninstall_removes_hooks_when_the_settings_file_is_not_utf8() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);
    fs::create_dir_all(&settings_dir).unwrap();
    install_antigravity_cli().unwrap();
    let settings_path = settings_dir.join("settings.json");
    let bytes = b"{ \"theme\": \"\xff\xfe\" }";
    fs::write(&settings_path, bytes).unwrap();

    let result = uninstall_antigravity_cli().unwrap();

    assert!(result.updated_hooks);
    assert!(result.removed_hook_file);
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert_eq!(fs::read(&settings_path).unwrap(), bytes);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[cfg(not(windows))]
#[test]
fn antigravity_cli_uninstall_keeps_the_tap_when_non_utf8_settings_name_it() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    let settings_dir = isolate_antigravity_cli_settings_dir(&base);
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    fs::write(
        &settings_path,
        format!(
            "{{\"statusLine\": {{\"type\": \"command\", \"command\": \"{ANTIGRAVITY_USER_STATUSLINE_COMMAND}\"}}}}\n"
        ),
    )
    .unwrap();
    install_antigravity_cli().unwrap();
    let tap_path = agy_dir.join("hooks").join("herdr-statusline-tap.sh");
    let mut bytes = fs::read(&settings_path).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("herdr-statusline-tap.sh"));
    bytes.extend_from_slice(b"// \xff\xfe\n");
    fs::write(&settings_path, &bytes).unwrap();

    let result = uninstall_antigravity_cli().unwrap();

    assert!(result.updated_hooks);
    assert!(result.removed_hook_file);
    assert!(tap_path.exists(), "the settings may still name the tap");
    assert_eq!(result.warnings.len(), 2, "{:?}", result.warnings);
    assert!(result.warnings[1].contains(&tap_path.display().to_string()));
    assert_eq!(fs::read(&settings_path).unwrap(), bytes);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_antigravity_cli_writes_hook_and_updates_hooks_json() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"lint-checker":{"PreInvocation":[{"type":"command","command":"echo keep-me"}]}}"#,
    )
    .unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    isolate_antigravity_cli_settings_dir(&base);

    let installed = install_antigravity_cli().unwrap();

    assert_eq!(
        installed.hook_path,
        agy_dir
            .join("hooks")
            .join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.hooks_path, agy_dir.join("hooks.json"));
    assert_eq!(
        fs::read_to_string(&installed.hook_path).unwrap(),
        ANTIGRAVITY_CLI_HOOK_ASSET
    );

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(agy_dir.join("hooks.json")).unwrap()).unwrap();
    let hooks = hooks_file.as_object().unwrap();

    // Herdr entries live under a named hook block; Antigravity CLI rejects a
    // file whose top level maps event names straight to arrays.
    let block = hooks
        .get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
        .and_then(Value::as_object)
        .unwrap();

    for (event, action) in ANTIGRAVITY_CLI_HOOK_EVENTS {
        let entries = block.get(event).and_then(Value::as_array).unwrap();
        assert_eq!(entries.len(), 1, "{event} should hold one Herdr entry");
        let handler = &entries[0];

        // Handlers must be a flat list; the matcher/hooks wrapper is only
        // valid for tool events and invalidates the whole file here.
        assert!(
            handler.get("matcher").is_none() && handler.get("hooks").is_none(),
            "{event} must be a flat handler, got {handler}"
        );

        assert_eq!(handler.get("type").and_then(Value::as_str), Some("command"));
        assert_eq!(
            handler.get("timeout").and_then(Value::as_u64),
            Some(ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC)
        );
        let command = handler.get("command").and_then(Value::as_str).unwrap();
        assert_eq!(
            command,
            antigravity_cli_hook_command(&installed.hook_path, action)
        );
    }

    // The integration is session-only. Antigravity CLI cannot express blocked
    // state, skips PostInvocation on interruption, and fires Stop at end of
    // turn rather than process exit, so Herdr never claims lifecycle authority
    // here and screen detection owns agent state.
    for event in ["PreToolUse", "PostToolUse", "PostInvocation", "Stop"] {
        assert!(
            block.get(event).is_none(),
            "{event} must not be registered; lifecycle stays with screen detection"
        );
    }

    // Other named hooks are left untouched.
    assert_eq!(
        hooks
            .get("lint-checker")
            .and_then(|block| block.get("PreInvocation"))
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.get("command"))
            .and_then(Value::as_str),
        Some("echo keep-me")
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn antigravity_cli_v3_install_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    let hook_dir = agy_dir.join("hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    fs::write(
        hook_dir.join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME),
        ANTIGRAVITY_CLI_HOOK_ASSET
            .replace("HERDR_INTEGRATION_VERSION=4", "HERDR_INTEGRATION_VERSION=3"),
    )
    .unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    std::env::set_var(
        ANTIGRAVITY_CLI_SETTINGS_DIR_ENV_VAR,
        base.join(".gemini").join("antigravity-cli"),
    );

    let status = || {
        installed_integration_statuses()
            .into_iter()
            .find(|status| status.target == crate::api::schema::IntegrationTarget::AntigravityCli)
            .expect("antigravity cli integration status")
    };
    let outdated = status();
    assert_eq!(outdated.state, IntegrationStatusKind::Outdated);
    assert_eq!(outdated.installed_version, Some(3));
    assert_eq!(outdated.expected_version, 4);

    install_antigravity_cli().unwrap();
    assert_eq!(status().state, IntegrationStatusKind::Current);

    std::env::remove_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(ANTIGRAVITY_CLI_SETTINGS_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_antigravity_cli_rewrites_stale_herdr_block() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    // An older Herdr install claimed lifecycle authority, wrapped events in
    // matcher/hooks, and left entries Antigravity CLI now rejects.
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"herdr":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"stale"}]}],"PostInvocation":[{"type":"command","command":"stale idle"}],"Legacy":[]}}"#,
    )
    .unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    isolate_antigravity_cli_settings_dir(&base);

    install_antigravity_cli().unwrap();

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(agy_dir.join("hooks.json")).unwrap()).unwrap();
    let block = hooks_file
        .get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
        .and_then(Value::as_object)
        .unwrap();

    // The block is Herdr-owned and rewritten wholesale, so a stale lifecycle
    // install is migrated to session-only rather than merged with.
    assert_eq!(
        block.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["PreInvocation"],
        "stale lifecycle events should be gone"
    );
    let entries = block
        .get("PreInvocation")
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].get("hooks").is_none());
    assert!(entries[0]
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command != "stale" && command != "stale idle"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_antigravity_cli_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);

    let err = install_antigravity_cli().unwrap_err();
    assert!(err.to_string().contains("install antigravity cli first"));
    assert!(!agy_dir.exists(), "install must not create the config dir");

    std::env::remove_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn grok_v1_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    let hooks_dir = grok_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);
    let hook_path = hooks_dir.join(GROK_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=grok\n# HERDR_INTEGRATION_VERSION=1\n",
    )
    .unwrap();
    fs::write(
        hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME),
        serde_json::to_string(&grok_hook_config(&hook_path)).unwrap(),
    )
    .unwrap();

    let grok = installed_integration_statuses()
        .into_iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Grok)
        .expect("grok integration status");
    assert_eq!(grok.installed_version, Some(1));
    assert_eq!(grok.expected_version, GROK_INTEGRATION_VERSION);
    assert_eq!(grok.state, IntegrationStatusKind::Outdated);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn grok_v2_integration_status_is_current() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);
    // A real install writes both the hook script and hooks/herdr.json.
    install_grok().unwrap();

    let statuses = installed_integration_statuses();
    let grok = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Grok)
        .expect("grok integration status");
    assert_eq!(grok.state, IntegrationStatusKind::Current);
    assert_eq!(grok.installed_version, Some(GROK_INTEGRATION_VERSION));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn grok_status_reports_outdated_when_hook_config_missing_or_broken() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &grok_dir);
    install_grok().unwrap();
    let config_path = grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME);

    let grok_state = || {
        installed_integration_statuses()
            .into_iter()
            .find(|status| status.target == crate::api::schema::IntegrationTarget::Grok)
            .expect("grok integration status")
            .state
    };

    // Missing config: grok never runs the hook, so the install is not current.
    fs::remove_file(&config_path).unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Corrupt config.
    fs::write(&config_path, "{not json").unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Config that no longer references the hook script.
    fs::write(
        &config_path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo other"}]}]}}"#,
    )
    .unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Config that mentions the script name without invoking it, and one that
    // invokes it without the required `session` action: both are
    // nonfunctional, so neither may report current.
    fs::write(
        &config_path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo herdr-agent-state.sh"}]}]}}"#,
    )
    .unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);
    let hook_path = grok_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME);
    fs::write(
        &config_path,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"sh '{}'"}}]}}]}}}}"#,
            hook_path.display()
        ),
    )
    .unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Correct command but not a command-type hook: grok will not execute it.
    let session_command = grok_session_command(&grok_hook_config(&hook_path));
    fs::write(
        &config_path,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"http","command":{}}}]}}]}}}}"#,
            serde_json::to_string(&session_command).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // A matcher can prevent the expected hook from running.
    let mut config = grok_hook_config(&hook_path);
    config["hooks"]["SessionStart"][0]["matcher"] = json!("(");
    fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // A malformed sibling group makes grok reject the event's hook groups.
    let mut config = grok_hook_config(&hook_path);
    config["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({}));
    fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Reinstall repairs both files.
    install_grok().unwrap();
    assert_eq!(grok_state(), IntegrationStatusKind::Current);

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_antigravity_cli_removes_hooks_json_entries_and_hook_file() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).unwrap();
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"lint-checker":{"PreInvocation":[{"type":"command","command":"echo keep-me"}]}}"#,
    )
    .unwrap();
    std::env::set_var(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &agy_dir);
    isolate_antigravity_cli_settings_dir(&base);

    // Install first
    let installed = install_antigravity_cli().unwrap();
    assert!(installed.hook_path.is_file());

    // Uninstall
    let result = uninstall_antigravity_cli().unwrap();
    assert!(result.removed_hook_file);
    assert!(!installed.hook_path.is_file());
    assert!(result.updated_hooks);

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(agy_dir.join("hooks.json")).unwrap()).unwrap();
    let hooks = hooks_file.as_object().unwrap();

    // The Herdr block is gone and unrelated named hooks survive.
    assert!(hooks.get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME).is_none());
    assert!(hooks.contains_key("lint-checker"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn grok_dir_honors_grok_home_after_config_dir_seam() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home_dir = base.join("grok-home");
    fs::create_dir_all(&home_dir).unwrap();
    std::env::remove_var(GROK_CONFIG_DIR_ENV_VAR);
    std::env::set_var(GROK_HOME_ENV_VAR, &home_dir);

    // The grok CLI reads its config (and hooks/) from $GROK_HOME, so the
    // integration must install there too.
    let installed = install_grok().unwrap();
    assert_eq!(
        installed.hook_path,
        home_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    // The herdr-level test seam still wins over GROK_HOME when set.
    let seam_dir = base.join("seam");
    fs::create_dir_all(&seam_dir).unwrap();
    std::env::set_var(GROK_CONFIG_DIR_ENV_VAR, &seam_dir);
    let installed = install_grok().unwrap();
    assert_eq!(
        installed.hook_path,
        seam_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    std::env::remove_var(GROK_HOME_ENV_VAR);
    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}
