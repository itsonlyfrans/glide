use glide_platform::PermissionKind;
use std::path::{Path, PathBuf};

pub(super) const LAUNCH_AGENT_LABEL: &str = "com.glide.daemon";

pub(super) fn data_directory(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Glide")
}

pub(super) fn launch_agents_directory(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents")
}

pub(super) fn permission_settings_url(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Accessibility | PermissionKind::Injection => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        PermissionKind::InputMonitoring => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        }
    }
}

pub(super) fn launch_agent_plist(
    executable: &Path,
    data_directory: &Path,
    ui: Option<&Path>,
) -> Result<String, glide_platform::BackendError> {
    // These are macOS paths even when the pure plist tests run on Windows.
    let absolute = |path: &Path| path.to_str().is_some_and(|text| text.starts_with('/'));
    if !absolute(executable) || !absolute(data_directory) || ui.is_some_and(|path| !absolute(path))
    {
        return Err(glide_platform::BackendError::InvalidInput(
            "application paths must be absolute".into(),
        ));
    }
    let executable = executable
        .to_str()
        .ok_or(glide_platform::BackendError::InvalidInput(
            "application path is not valid Unicode".into(),
        ))?;
    let data_directory =
        data_directory
            .to_str()
            .ok_or(glide_platform::BackendError::InvalidInput(
                "data path is not valid Unicode".into(),
            ))?;
    if !xml_text_is_valid(executable)
        || !xml_text_is_valid(data_directory)
        || executable.chars().any(char::is_control)
        || data_directory.chars().any(char::is_control)
        || ui.is_some_and(|path| {
            path.to_str().is_none_or(|value| {
                !xml_text_is_valid(value) || value.chars().any(char::is_control)
            })
        })
    {
        return Err(glide_platform::BackendError::InvalidInput(
            "application path contains an unsupported character".into(),
        ));
    }
    let mut arguments = vec![executable, "--headless", "--data-dir", data_directory];
    if let Some(ui) = ui {
        let ui = ui
            .to_str()
            .ok_or(glide_platform::BackendError::InvalidInput(
                "UI path is not valid Unicode".into(),
            ))?;
        arguments.extend(["--ui", ui]);
    }
    let arguments = arguments
        .into_iter()
        .map(|argument| format!("<string>{}</string>", xml_escape(argument)))
        .collect::<String>();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array>{}</array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><false/>\n</dict></plist>\n",
        xml_escape(LAUNCH_AGENT_LABEL),
        arguments,
    ))
}

fn xml_text_is_valid(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(
            character as u32,
            0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
        )
    })
}

pub(super) fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub(super) fn gui_domain(uid: &str) -> Option<String> {
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("gui/{uid}"))
}

pub(super) fn launch_agent_path(home: &Path) -> PathBuf {
    launch_agents_directory(home).join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_untrusted_path_characters_and_sets_login_behavior() {
        {
            let home = Path::new("/Users/example");
            assert_eq!(
                data_directory(home),
                PathBuf::from("/Users/example/Library/Application Support/Glide")
            );
            assert_eq!(
                launch_agent_path(home),
                PathBuf::from("/Users/example/Library/LaunchAgents/com.glide.daemon.plist")
            );
            assert!(permission_settings_url(PermissionKind::Accessibility)
                .ends_with("Privacy_Accessibility"));
            assert!(permission_settings_url(PermissionKind::Injection)
                .ends_with("Privacy_Accessibility"));
            assert!(permission_settings_url(PermissionKind::InputMonitoring)
                .ends_with("Privacy_ListenEvent"));
        }
        let plist = launch_agent_plist(
            Path::new("/Applications/Glide & Tools/<glided>"),
            Path::new("/Users/A & B/Library/Application Support/Glide"),
            Some(Path::new("/Applications/Glide & Tools/Glide UI.app")),
        )
        .expect("ASCII test paths are valid Unicode");
        assert!(plist.contains("Glide &amp; Tools/&lt;glided&gt;"));
        assert!(plist.contains("A &amp; B/Library/Application Support/Glide"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<key>KeepAlive</key><false/>"));
        assert!(plist.contains("<string>--headless</string><string>--data-dir</string>"));
        assert!(plist.contains(
            "<string>--ui</string><string>/Applications/Glide &amp; Tools/Glide UI.app</string>"
        ));
        assert!(plist.contains("<string>--headless</string>"));
        assert!(!plist.contains("<string>/Applications/Glide & Tools"));

        {
            assert_eq!(xml_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&apos;");
        }
    }

    #[test]
    fn launchctl_domain_rejects_non_numeric_uids() {
        assert_eq!(gui_domain("501").as_deref(), Some("gui/501"));
        assert_eq!(gui_domain(""), None);
        assert_eq!(gui_domain("501/other"), None);
        assert_eq!(gui_domain("501\n"), None);
    }

    #[test]
    fn plist_rejects_characters_forbidden_by_xml() {
        let result = launch_agent_plist(
            Path::new("/Applications/Glide\u{1}"),
            Path::new("/Users/example/Library/Application Support/Glide"),
            None,
        );
        assert!(matches!(
            result,
            Err(glide_platform::BackendError::InvalidInput(_))
        ));
        assert!(launch_agent_plist(
            Path::new("relative/glided"),
            Path::new("/Users/example/Glide"),
            None
        )
        .is_err());
        assert!(launch_agent_plist(
            Path::new("/Applications/glided"),
            Path::new("/Users/example/Glide"),
            Some(Path::new("/Applications/Bad\u{fffe}"))
        )
        .is_err());
    }
}
