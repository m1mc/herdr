#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    #[allow(dead_code)]
    Sound(bool),
    #[allow(dead_code)]
    VisualBell(bool),
    Alerts { sound: bool, visual_bell: bool },
    ToastDelivery(super::ToastDelivery),
    HideOfflineHosts(bool),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::VisualBell(_) => "visual bell setting",
            Self::Alerts { .. } => "alert settings",
            Self::ToastDelivery(_) => "toast setting",
            Self::HideOfflineHosts(_) => "hide offline hosts setting",
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
            Self::VisualBell(enabled) => {
                super::upsert_section_bool(content, "ui.bell", "visual_bell", enabled)
            }
            Self::Alerts { sound, visual_bell } => {
                let content = super::upsert_section_bool(content, "ui.sound", "enabled", sound);
                super::upsert_section_bool(&content, "ui.bell", "visual_bell", visual_bell)
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Herdr => "\"herdr\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                    super::ToastDelivery::All => "\"all\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
            Self::HideOfflineHosts(hide) => {
                super::upsert_section_bool(content, "ui", "hide_offline_hosts", hide)
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
    let content = match super::io::read_optional_config(path) {
        Ok(Some(content)) => content,
        Ok(None) => String::new(),
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
    fn update_file_at_does_not_move_a_leading_bom_into_the_file() {
        let dir = std::env::temp_dir().join(format!("herdr-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();

        update_file_at(&path, "onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            !written.contains('\u{feff}'),
            "unexpected BOM in {written:?}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_ok(),
            "written config is not valid TOML: {written:?}"
        );
    }

    #[test]
    fn config_edit_hide_offline_hosts_applies() {
        let initial = "[ui]\nsidebar_width = 32\n";
        let updated = ConfigEdit::HideOfflineHosts(true).apply(initial);
        let config: super::super::Config = toml::from_str(&updated).expect("valid config");
        assert!(config.ui.hide_offline_hosts);
        assert_eq!(config.ui.sidebar_width, 32);
        let updated_false = ConfigEdit::HideOfflineHosts(false).apply(&updated);
        let config: super::super::Config = toml::from_str(&updated_false).expect("valid config");
        assert!(!config.ui.hide_offline_hosts);
        assert_eq!(config.ui.sidebar_width, 32);
    }

    #[test]
    fn config_edit_visual_bell_applies() {
        let initial = "[ui]\nsidebar_width = 32\n";
        let updated = ConfigEdit::VisualBell(true).apply(initial);
        let config: super::super::Config = toml::from_str(&updated).expect("valid config");
        assert!(config.ui.bell.visual_bell);
        assert!(config.ui.visual_bell_enabled());

        let updated_false = ConfigEdit::VisualBell(false).apply(&updated);
        let config: super::super::Config = toml::from_str(&updated_false).expect("valid config");
        assert!(!config.ui.bell.visual_bell);
        assert!(!config.ui.visual_bell_enabled());
    }

    #[test]
    fn config_edit_alerts_applies() {
        let initial = "[ui]\nsidebar_width = 32\n";
        let updated = ConfigEdit::Alerts { sound: false, visual_bell: true }.apply(initial);
        let config: super::super::Config = toml::from_str(&updated).expect("valid config");
        assert!(!config.ui.sound.enabled);
        assert!(config.ui.bell.visual_bell);
        assert!(config.ui.visual_bell_enabled());

        let updated_sound = ConfigEdit::Alerts { sound: true, visual_bell: false }.apply(&updated);
        let config: super::super::Config = toml::from_str(&updated_sound).expect("valid config");
        assert!(config.ui.sound.enabled);
        assert!(!config.ui.bell.visual_bell);
        assert!(!config.ui.visual_bell_enabled());
    }
}
