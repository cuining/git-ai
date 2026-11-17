use std::fs;
use std::path::PathBuf;

fn wix_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join("windows")
        .join("git-ai.wxs");
    fs::read_to_string(path).expect("read Windows MSI source")
}

#[test]
fn msi_does_not_expose_wsl_installation() {
    let wix = wix_source();

    for setting in ["INSTALL_WSL", "WslInstallArgument", "--wsl"] {
        assert!(
            !wix.contains(setting),
            "MSI should not expose the WSL setting {setting}"
        );
    }
}

#[test]
fn msi_configures_windows_install_hooks() {
    let wix = wix_source();

    assert!(
        wix.contains(
            r#"Value="install-hooks --env --api-base &quot;[API_BASE]&quot; --api-key &quot;[API_KEY]&quot;""#
        ),
        "MSI should configure the Windows environment and pass the API settings"
    );
    assert!(
        wix.contains(r#"ExeCommand="[ConfigureGitAi]""#),
        "MSI should run the configured install-hooks command"
    );
}
