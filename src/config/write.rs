#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    ToastDelivery(super::ToastDelivery),
    AutoResizeSplits(bool),
    /// Persists `[ui] recent_remote_mount_targets` (most-recent-first,
    /// already deduped/capped by the caller). Written server-side only
    /// (`handle_federation_mount_ready`, `src/app/api/workspaces.rs`, which
    /// is `#[cfg(unix)]`) — `#[allow(dead_code)]` on non-unix builds, matching
    /// this crate's existing precedent for federation-mount-only code paths.
    #[cfg_attr(not(unix), allow(dead_code))]
    RecentRemoteMountTargets(&'a [String]),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
            Self::AutoResizeSplits(_) => "auto-resize splits setting",
            Self::RecentRemoteMountTargets(_) => "recent remote mount targets",
        }
    }

    pub(crate) fn apply(self, content: &str) -> String {
        match self {
            Self::Theme(name) => {
                let content =
                    super::upsert_section_value(content, "theme", "name", &format!("\"{name}\""));
                super::upsert_section_bool(&content, "theme", "auto_switch", false)
            }
            Self::StatusIndicators(style) => super::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            ),
            Self::Sound(enabled) => {
                super::upsert_section_bool(content, "ui.sound", "enabled", enabled)
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Herdr => "\"herdr\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
            Self::AutoResizeSplits(enabled) => {
                super::upsert_section_bool(content, "ui", "auto_resize_splits", enabled)
            }
            Self::RecentRemoteMountTargets(targets) => {
                // Serialized by the `toml` crate rather than hand-quoted: a
                // target is arbitrary user text (`DOMAIN\user@host` is a
                // legal OpenSSH destination, and an `~/.ssh/config` `Host`
                // alias may contain a `"`), and a single unescaped `\` or `"`
                // would make the whole `config.toml` unparsable — `Config::
                // load` answers a top-level parse error by discarding the
                // *entire* file for defaults, silently resetting every
                // unrelated setting on the next start.
                let value = toml::Value::from(targets.to_vec()).to_string();
                super::upsert_section_value(content, "ui", "recent_remote_mount_targets", &value)
            }
        }
    }
}

pub(crate) fn update_file_at(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create config directory: {error}"))?;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "failed to read config before saving {description}: {error}"
            ));
        }
    };
    std::fs::write(path, update(&content))
        .map_err(|error| format!("failed to save {description}: {error}"))
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        edit.apply(content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_remote_mount_targets_edit_escapes_quotes_and_backslashes() {
        // `DOMAIN\user@host` is a legal OpenSSH destination and an
        // `~/.ssh/config` `Host` alias can contain a `"`. Hand-quoting either
        // one produces a `config.toml` that fails to parse, and a top-level
        // parse error makes `Config::load` discard the *whole* file for
        // defaults -- so the value must be TOML-escaped and the file must
        // stay parsable, round-tripping the targets byte-for-byte and
        // preserving unrelated keys.
        let targets = vec!["DOMAIN\\user@host".to_string(), "we\"ird@host".to_string()];
        let content = ConfigEdit::RecentRemoteMountTargets(&targets)
            .apply("onboarding = false\n[ui]\naccent = \"red\"\n");

        let parsed: crate::config::Config = toml::from_str(&content)
            .unwrap_or_else(|err| panic!("config must stay parsable, got {err}:\n{content}"));
        assert_eq!(parsed.ui.recent_remote_mount_targets, targets);
        assert_eq!(
            parsed.ui.accent, "red",
            "an unrelated key must survive the edit"
        );
    }

    #[test]
    fn recent_remote_mount_targets_edit_description_names_the_setting() {
        let targets = Vec::new();
        assert_eq!(
            ConfigEdit::RecentRemoteMountTargets(&targets).description(),
            "recent remote mount targets"
        );
    }
}
