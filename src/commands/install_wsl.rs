//! Best-effort installation of DCC AI Tracking into Windows Subsystem for Linux distros.

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(any(windows, test))]
const INSTALL_SCRIPT: &str = include_str!("install_wsl.sh");

pub struct InstallConfig<'a> {
    pub api_base: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

pub fn install(config: InstallConfig<'_>, dry_run: bool) {
    if dry_run {
        println!("Dry-run: skipping WSL installation.");
        return;
    }

    #[cfg(windows)]
    {
        if let Err(error) = crate::http::ensure_remote_api_enabled() {
            eprintln!("[git-ai] warning: {error}; skipping WSL installation.");
            return;
        }
        install_on_windows(config);
    }

    #[cfg(not(windows))]
    {
        let _ = config;
        eprintln!(
            "[git-ai] warning: --wsl is only supported on Windows; skipping WSL installation."
        );
    }
}

#[cfg(windows)]
fn install_on_windows(config: InstallConfig<'_>) {
    let output = match wsl_command().args(["--list", "--quiet"]).output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!(
                "[git-ai] warning: could not list WSL distributions (exit code {}); skipping WSL installation.",
                output.status
            );
            return;
        }
        Err(error) => {
            eprintln!(
                "[git-ai] warning: WSL is not available ({error}); skipping WSL installation."
            );
            return;
        }
    };

    let distros = eligible_distros(&decode_wsl_output(&output.stdout));
    if distros.is_empty() {
        println!("No eligible WSL distributions detected; skipping Linux installation.");
        return;
    }

    let env = forwarded_env(
        config.api_base,
        config.api_key,
        std::env::var("WSLENV").ok().as_deref(),
    );
    let mut packages = std::collections::HashMap::new();
    let failures = install_in_distros(&distros, |distro| {
        println!("Installing DCC AI Tracking in WSL distribution: {distro}");
        let arch = distro_architecture(distro)?;
        let package = packages.entry(arch).or_insert_with_key(|arch| {
            super::upgrade::npm::fetch_wsl_package(arch, env!("CARGO_PKG_VERSION"))
        });
        let (binary, installer) = package.as_ref().map_err(Clone::clone)?;
        install_package(distro, &env, binary, installer)
    });

    if failures.is_empty() {
        println!(
            "Successfully installed DCC AI Tracking in {} WSL distribution(s).",
            distros.len()
        );
    } else {
        eprintln!(
            "[git-ai] warning: DCC AI Tracking could not be installed in {} of {} WSL distribution(s): {}",
            failures.len(),
            distros.len(),
            failures.join(", ")
        );
    }
}

#[cfg(any(windows, test))]
fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2
        && bytes.len().is_multiple_of(2)
        && bytes.chunks_exact(2).any(|pair| pair[1] == 0)
    {
        let words = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(any(windows, test))]
fn eligible_distros(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}'))
        .filter(|line| !line.is_empty())
        .filter(|line| !line.to_ascii_lowercase().starts_with("docker-desktop"))
        .map(str::to_owned)
        .collect()
}

#[cfg(windows)]
fn wsl_command() -> Command {
    let mut command = Command::new("wsl.exe");
    command.creation_flags(crate::utils::CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
fn distro_architecture(distro: &str) -> Result<String, String> {
    let output = wsl_command()
        .args(["--distribution", distro, "--exec", "uname", "-m"])
        .output()
        .map_err(|error| format!("cannot probe Linux architecture: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Linux architecture probe failed ({})",
            output.status
        ));
    }
    Ok(decode_wsl_output(&output.stdout).trim().to_owned())
}

#[cfg(any(windows, test))]
fn install_args(distro: &str) -> [&str; 6] {
    [
        "--distribution",
        distro,
        "--exec",
        "bash",
        "-c",
        INSTALL_SCRIPT,
    ]
}

#[cfg(windows)]
fn install_package(
    distro: &str,
    env: &[(String, String)],
    binary: &[u8],
    installer: &str,
) -> Result<(), String> {
    let mut child = wsl_command()
        .args(install_args(distro))
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start installer: {error}"))?;
    let stdin = child.stdin.take().expect("piped WSL stdin");
    let transfer = write_install_archive(stdin, binary, installer);
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for installer: {error}"))?;
    if !status.success() {
        return Err(format!("Linux installer failed ({status})"));
    }
    transfer.map_err(|error| format!("cannot transfer Linux package: {error}"))
}

#[cfg(any(windows, all(test, unix)))]
fn write_install_archive(
    writer: impl std::io::Write,
    binary: &[u8],
    installer: &str,
) -> std::io::Result<()> {
    let mut archive = tar::Builder::new(writer);
    for (name, contents) in [
        ("dcc-ai-tracking", binary),
        ("install.sh", installer.as_bytes()),
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o600);
        header.set_size(contents.len() as u64);
        archive.append_data(&mut header, name, contents)?;
    }
    archive.finish()
}

#[cfg(any(windows, test))]
fn forwarded_env(
    api_base: Option<&str>,
    api_key: Option<&str>,
    inherited_wslenv: Option<&str>,
) -> Vec<(String, String)> {
    let values = [("API_BASE", api_base), ("API_KEY", api_key)];
    let mut env = values
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| (name.to_owned(), value.to_owned())))
        .collect::<Vec<_>>();
    if env.is_empty() {
        return env;
    }

    let mut wslenv = inherited_wslenv
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    for (name, _) in &env {
        let already_forwarded = wslenv.split(':').any(|entry| {
            entry
                .split_once('/')
                .map_or(entry, |(entry_name, _)| entry_name)
                .eq_ignore_ascii_case(name)
        });
        if !already_forwarded {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(name);
        }
    }
    env.push(("WSLENV".to_owned(), wslenv));
    env
}

#[cfg(any(windows, test))]
fn install_in_distros<F>(distros: &[String], mut install_one: F) -> Vec<&str>
where
    F: FnMut(&str) -> Result<(), String>,
{
    let mut failures = Vec::new();
    for distro in distros {
        if let Err(error) = install_one(distro) {
            eprintln!("[git-ai] warning: WSL installation failed in '{distro}': {error}");
            failures.push(distro.as_str());
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16_wsl_output_and_filters_docker_distros() {
        let text = "\u{feff}Ubuntu\r\ndocker-desktop\r\nDocker-Desktop-Data\r\nDebian\r\n";
        let bytes = text
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();

        assert_eq!(
            eligible_distros(&decode_wsl_output(&bytes)),
            ["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn decodes_utf8_wsl_output_and_ignores_blank_lines() {
        assert_eq!(
            eligible_distros(&decode_wsl_output(b"Ubuntu\n\n Debian \n")),
            ["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn install_arguments_do_not_expose_forwarded_configuration() {
        assert_eq!(
            install_args("Ubuntu Dev"),
            [
                "--distribution",
                "Ubuntu Dev",
                "--exec",
                "bash",
                "-c",
                INSTALL_SCRIPT,
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn wsl_installs_the_verified_linux_payload() {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        let scratch = tempfile::tempdir().unwrap();
        let bin_dir = scratch.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let curl = bin_dir.join("curl");
        std::fs::write(&curl, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();

        let binary = b"#!/bin/sh\nset -eu\n\
            [ \"$*\" = 'install-hooks --env' ]\n\
            [ \"$API_BASE\" = 'https://enterprise.example' ]\n\
            [ \"$API_KEY\" = 'fixture secret' ]\n\
            touch \"$HOME/hooks-initialized\"\n";
        let checksum = Sha256::digest(binary);
        let installer = include_str!("../../install.sh").replacen(
            "EMBEDDED_CHECKSUMS=\"__CHECKSUMS_PLACEHOLDER__\"",
            &format!("EMBEDDED_CHECKSUMS=\"{checksum:x}  dcc-ai-tracking\""),
            1,
        );
        let mut archive = Vec::new();
        write_install_archive(&mut archive, binary, &installer).unwrap();

        let mut path = vec![bin_dir];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let path = std::env::join_paths(path).unwrap();
        let run_guest = |payload: &[u8]| {
            let mut child = Command::new("bash")
                .args(["-c", INSTALL_SCRIPT])
                .env("HOME", scratch.path())
                .env("API_BASE", "https://enterprise.example")
                .env("API_KEY", "fixture secret")
                .env("PATH", &path)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let transfer = child.stdin.take().unwrap().write_all(payload);
            child.wait().unwrap().success() && transfer.is_ok()
        };

        let install_root = scratch.path().join(".dcc-ai-tracking");
        let installed = install_root.join("bin/dcc-ai-tracking");
        assert!(!run_guest(&archive[..archive.len() - 512]));
        assert!(!installed.exists());
        assert!(run_guest(&archive));
        assert_eq!(std::fs::read(&installed).unwrap(), binary);
        assert!(scratch.path().join("hooks-initialized").exists());

        std::fs::write(&installed, b"DCC-managed executable").unwrap();
        std::os::unix::fs::symlink(
            "missing-owner-record",
            install_root.join("dcc-install.json"),
        )
        .unwrap();
        assert!(!run_guest(&archive));
        assert_eq!(
            std::fs::read(&installed).unwrap(),
            b"DCC-managed executable"
        );
    }

    #[test]
    fn forwarded_configuration_uses_wslenv_without_overwriting_existing_entries() {
        assert_eq!(
            forwarded_env(
                Some("https://enterprise.example/path?a=b"),
                Some("key with spaces; untouched"),
                Some("EXISTING/p:API_KEY/u")
            ),
            [
                (
                    "API_BASE".to_owned(),
                    "https://enterprise.example/path?a=b".to_owned()
                ),
                (
                    "API_KEY".to_owned(),
                    "key with spaces; untouched".to_owned()
                ),
                (
                    "WSLENV".to_owned(),
                    "EXISTING/p:API_KEY/u:API_BASE".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn attempts_every_distro_after_failures() {
        let distros = vec![
            "Ubuntu".to_owned(),
            "Debian".to_owned(),
            "Fedora".to_owned(),
        ];
        let mut attempted = Vec::new();

        let failures = install_in_distros(&distros, |distro| {
            attempted.push(distro.to_owned());
            if distro == "Debian" {
                Err("fixture installation failure".to_owned())
            } else {
                Ok(())
            }
        });

        assert_eq!(attempted, distros);
        assert_eq!(failures, ["Debian"]);
    }
}
