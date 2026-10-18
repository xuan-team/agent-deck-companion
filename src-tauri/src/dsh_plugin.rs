//! Companion-managed installation of the `dsh-deck-bridge` plugin into a DSH
//! profile.
//!
//! DSH keeps plugins per profile: `<DSH_HOME>/profiles/<name>/package.json`
//! lists the dependency and `dsh.profile.bundles` must name it too. The
//! supported way to maintain that pair is DSH's own `dsh plugin` command, which
//! forwards to the pnpm copy DSH keeps inside the profile. This module therefore
//! never rewrites those files itself — it builds one fixed argv and runs the
//! same command the user would type.
//!
//! Nothing here is interpolated from user input. The profile name is a directory
//! name validated against the shape DSH mints, and the plugin directory is one
//! the companion ships (or an explicit environment override) — never a path from
//! an imported profile or a hardware event.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde::Serialize;

/// The plugin DSH loads; also the dependency name in a profile's package.json.
pub const PLUGIN_ID: &str = "dsh-deck-bridge";
/// Integration id used by the Hooks page for this entry. It is deliberately not
/// `dsh`: that is the DSH *agent*, and this entry installs a plugin into DSH's
/// own profile rather than a lifecycle hook into an agent.
pub const INTEGRATION_ID: &str = "dsh-plugin";
/// The CLI package the installer is invoked through.
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
/// `npx` may legitimately take a while on a cold cache (it resolves and can
/// install the DSH CLI before DSH's own installer runs).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);

/// A profile name is a single directory name under `profiles/`.
fn valid_profile(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name != "node_modules"
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
        })
}

/// Profiles that exist under `<DSH_HOME>/profiles`, in stable order.
pub fn profiles(dsh_home: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dsh_home.join("profiles")) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| valid_profile(name))
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// The profile the companion would install into.
///
/// The running `dsh web` process wins, because that is the profile the user is
/// actually looking at; otherwise a lone profile is unambiguous. Several idle
/// profiles and no running server is not something to guess at.
pub fn active_profile(dsh_home: &Path) -> Option<String> {
    if let Some(profile) = crate::dsh::web_profile(&sysinfo::System::new_all()) {
        if valid_profile(&profile) {
            return Some(profile);
        }
    }
    let mut names = profiles(dsh_home);
    match names.len() {
        0 => None,
        1 => Some(names.remove(0)),
        _ => names.into_iter().find(|name| name == "web"),
    }
}

fn profile_dir(dsh_home: &Path, profile: &str) -> PathBuf {
    dsh_home.join("profiles").join(profile)
}

fn read_profile_package(dsh_home: &Path, profile: &str) -> Option<serde_json::Value> {
    let text = fs::read_to_string(profile_dir(dsh_home, profile).join("package.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the profile both depends on and bundles the plugin, which is the
/// pair `dsh plugin` maintains and the pair DSH's loader requires.
fn installed_in(package: &serde_json::Value) -> bool {
    let dependency = package
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(PLUGIN_ID))
        .is_some();
    let bundled = package
        .get("dsh")
        .and_then(|dsh| dsh.get("profile"))
        .and_then(|profile| profile.get("bundles"))
        .and_then(|bundles| bundles.as_array())
        .is_some_and(|bundles| {
            bundles
                .iter()
                .any(|entry| entry.as_str() == Some(PLUGIN_ID))
        });
    dependency && bundled
}

fn installed_version(dsh_home: &Path, profile: &str) -> Option<String> {
    let text = fs::read_to_string(
        profile_dir(dsh_home, profile)
            .join("node_modules")
            .join(PLUGIN_ID)
            .join("package.json"),
    )
    .ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("version")
        .and_then(|version| version.as_str())
        .map(str::to_owned)
}

/// The argv handed to `npx`, kept separate so it can be asserted without
/// running anything.
fn dsh_argv(profile: &str, bridge: &Path, action: Action) -> Vec<String> {
    let mut argv = vec![
        "--yes".to_owned(),
        DSH_PACKAGE.to_owned(),
        "plugin".to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
    ];
    match action {
        Action::Install => {
            argv.push("add".to_owned());
            argv.push(format!("link:{}", bridge.display()));
        }
        Action::Remove => {
            argv.push("remove".to_owned());
            argv.push(PLUGIN_ID.to_owned());
        }
    }
    argv
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Remove,
}

/// Find the `npx` to run.
///
/// A Finder-launched app inherits a minimal PATH that usually has no Node, so
/// the user's login shell is asked to resolve it. The override exists so a
/// packaged app on an unusual setup has a documented escape hatch.
pub fn resolve_npx() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("AGENT_DECK_NPX") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("AGENT_DECK_NPX does not exist: {}", path.display()));
    }
    if let Ok(output) = Command::new("/bin/zsh")
        .args(["-lc", "command -v npx"])
        .stdin(Stdio::null())
        .output()
    {
        if output.status.success() {
            let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !resolved.is_empty() {
                return Ok(PathBuf::from(resolved));
            }
        }
    }
    // Last resort: a bare name still works when the app was launched from a
    // shell that carries the user's PATH.
    Ok(PathBuf::from("npx"))
}

/// Run `npx` with a hard deadline so a wedged installer cannot pin the app.
fn run_npx(npx: &Path, argv: &[String]) -> Result<String, String> {
    let mut child = Command::new(npx)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", npx.display()))?;

    let deadline = Instant::now() + INSTALL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("cannot read installer output: {error}"))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail = |text: &str| {
                    let text = text.trim();
                    let start = text.len().saturating_sub(600);
                    text[start..].to_owned()
                };
                if status.success() {
                    return Ok(tail(&stdout));
                }
                let detail = tail(&stderr);
                return Err(if detail.is_empty() {
                    format!("installer exited with {status}")
                } else {
                    detail
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return Err(format!(
                    "installer timed out after {}s",
                    INSTALL_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => return Err(format!("cannot wait for installer: {error}")),
        }
    }
}

/// What the Hooks page shows for the DSH plugin entry.
#[derive(Debug, Clone, Serialize)]
pub struct DshPluginState {
    /// Profile the companion would install into, when it can pick one.
    pub profile: Option<String>,
    /// Every profile found, so the UI can explain a choice it could not make.
    pub profiles: Vec<String>,
    pub installed: bool,
    pub version: Option<String>,
    /// Directory offered to DSH's installer as `link:<dir>`.
    pub bridge_dir: Option<String>,
    /// Human-readable state line, and the reason when nothing can be done.
    pub detail: String,
}

/// The directory the companion ships the plugin in.
///
/// A packaged app resolves the bundled resource; a development build falls back
/// to the repository copy next to `src-tauri`, so `npm run tauri:dev` works
/// without a bundle step.
pub fn bridge_dir(resource_dir: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("AGENT_DECK_DSH_BRIDGE_DIR") {
        let path = PathBuf::from(path);
        return if path.is_dir() {
            Ok(path)
        } else {
            Err(format!(
                "AGENT_DECK_DSH_BRIDGE_DIR is not a directory: {}",
                path.display()
            ))
        };
    }
    if let Some(resource_dir) = resource_dir {
        let bundled = resource_dir.join(PLUGIN_ID);
        if bundled.is_dir() {
            return Ok(bundled);
        }
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|root| root.join(PLUGIN_ID))
        .filter(|path| path.is_dir());
    dev.ok_or_else(|| "the bundled dsh-deck-bridge directory is missing".to_owned())
}

pub fn state(dsh_home: &Path, bridge: Result<PathBuf, String>) -> DshPluginState {
    let profiles = profiles(dsh_home);
    let profile = active_profile(dsh_home);
    let package = profile
        .as_deref()
        .and_then(|profile| read_profile_package(dsh_home, profile));
    let installed = package.as_ref().is_some_and(installed_in);
    let version = profile
        .as_deref()
        .filter(|_| installed)
        .and_then(|profile| installed_version(dsh_home, profile));
    let bridge_dir = bridge.as_ref().ok().map(|path| path.display().to_string());
    let detail = match (&bridge, &profile) {
        (Err(error), _) => error.clone(),
        (Ok(_), None) if profiles.is_empty() => {
            "未找到 DSH profile；先运行一次 dsh web 再安装".to_owned()
        }
        (Ok(_), None) => format!(
            "检测到多个 profile（{}），且没有运行中的 dsh web，无法确定装到哪个",
            profiles.join(", ")
        ),
        (Ok(_), Some(profile)) if installed => match &version {
            Some(version) => {
                format!("已装入 profile {profile} · 版本 {version}；改动需重启 dsh web 生效")
            }
            None => format!("已装入 profile {profile}；改动需重启 dsh web 生效"),
        },
        (Ok(_), Some(profile)) => format!("将装入 profile {profile}；安装后需重启 dsh web"),
    };
    DshPluginState {
        profile: profile.filter(|_| !profiles.is_empty() || bridge.is_ok()),
        profiles,
        installed,
        version,
        bridge_dir,
        detail,
    }
}

/// Install or remove the plugin, returning the state afterwards.
pub fn apply(
    dsh_home: &Path,
    bridge: Result<PathBuf, String>,
    install: bool,
) -> Result<DshPluginState, String> {
    let profile = active_profile(dsh_home).ok_or_else(|| {
        if profiles(dsh_home).is_empty() {
            "no DSH profile found; run `dsh web` once first".to_owned()
        } else {
            "several DSH profiles exist and no `dsh web` is running; start the profile you want and retry"
                .to_owned()
        }
    })?;
    let bridge = bridge?;
    let npx = resolve_npx()?;
    let argv = dsh_argv(
        &profile,
        &bridge,
        if install {
            Action::Install
        } else {
            Action::Remove
        },
    );
    eprintln!(
        "[dsh-plugin] {} profile={profile} via {}",
        if install { "install" } else { "remove" },
        npx.display()
    );
    match run_npx(&npx, &argv) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("[dsh-plugin] failed: {error}");
            return Err(error);
        }
    }
    Ok(state(dsh_home, Ok(bridge)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> PathBuf {
        std::env::temp_dir().join(format!("agent-deck-dsh-{}", uuid::Uuid::new_v4()))
    }

    fn write_profile(home: &Path, name: &str, installed: bool, version: Option<&str>) {
        let dir = profile_dir(home, name);
        fs::create_dir_all(dir.join("node_modules").join(PLUGIN_ID)).unwrap();
        let dependency = if installed {
            r#""dsh-deck-bridge": "link:/tmp/bridge""#
        } else {
            r#""something-else": "1.0.0""#
        };
        let bundles = if installed {
            r#"["@deepseek-ai/dsh-base", "dsh-deck-bridge"]"#
        } else {
            r#"["@deepseek-ai/dsh-base"]"#
        };
        fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"dependencies":{{{dependency}}},"dsh":{{"profile":{{"bundles":{bundles}}}}}}}"#
            ),
        )
        .unwrap();
        if let Some(version) = version {
            fs::write(
                dir.join("node_modules")
                    .join(PLUGIN_ID)
                    .join("package.json"),
                format!(r#"{{"name":"{PLUGIN_ID}","version":"{version}"}}"#),
            )
            .unwrap();
        }
    }

    #[test]
    fn profile_names_are_held_to_the_shape_dsh_mints() {
        assert!(valid_profile("web"));
        assert!(valid_profile("default-workspace"));
        assert!(!valid_profile(""));
        assert!(!valid_profile(".."));
        assert!(!valid_profile(".hidden"));
        assert!(!valid_profile("node_modules"));
        assert!(!valid_profile("a/b"));
        assert!(!valid_profile(&"a".repeat(65)));
    }

    #[test]
    fn argv_is_one_fixed_command_with_no_shell() {
        let bridge = Path::new("/opt/bridge");
        assert_eq!(
            dsh_argv("web", bridge, Action::Install),
            vec![
                "--yes",
                "@deepseek-ai/dsh",
                "plugin",
                "--profile",
                "web",
                "add",
                "link:/opt/bridge",
            ]
        );
        assert_eq!(
            dsh_argv("web", bridge, Action::Remove),
            vec![
                "--yes",
                "@deepseek-ai/dsh",
                "plugin",
                "--profile",
                "web",
                "remove",
                "dsh-deck-bridge",
            ]
        );
    }

    /// A profile is only considered installed when the dependency *and* the
    /// bundle entry are present: DSH's loader needs both, and a half-written
    /// profile is exactly what a failed install leaves behind.
    #[test]
    fn installation_needs_both_the_dependency_and_the_bundle_entry() {
        let home = temp_home();
        fs::create_dir_all(home.join("profiles")).unwrap();
        assert!(profiles(&home).is_empty());

        write_profile(&home, "web", false, None);
        let half = serde_json::json!({
            "dependencies": { PLUGIN_ID: "link:/tmp/bridge" },
            "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base"] } }
        });
        assert!(!installed_in(&half));
        let full = serde_json::json!({
            "dependencies": { PLUGIN_ID: "link:/tmp/bridge" },
            "dsh": { "profile": { "bundles": [PLUGIN_ID] } }
        });
        assert!(installed_in(&full));

        let state = state(&home, Ok(PathBuf::from("/tmp/bridge")));
        assert_eq!(state.profile.as_deref(), Some("web"));
        assert_eq!(state.profiles, vec!["web".to_owned()]);
        assert!(!state.installed);
        assert!(state.detail.contains("将装入 profile web"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_installed_profile_reports_its_version() {
        let home = temp_home();
        write_profile(&home, "web", true, Some("0.4.0"));
        let state = state(&home, Ok(PathBuf::from("/tmp/bridge")));
        assert!(state.installed);
        assert_eq!(state.version.as_deref(), Some("0.4.0"));
        assert!(state.detail.contains("版本 0.4.0"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_missing_bridge_directory_is_reported_instead_of_installing() {
        let home = temp_home();
        write_profile(&home, "web", false, None);
        let state = state(&home, Err("bundled dsh-deck-bridge is missing".into()));
        assert!(!state.installed);
        assert_eq!(state.bridge_dir, None);
        assert_eq!(state.detail, "bundled dsh-deck-bridge is missing");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn several_profiles_and_no_running_server_are_not_guessed_at() {
        let home = temp_home();
        write_profile(&home, "alpha", false, None);
        write_profile(&home, "beta", false, None);
        let state = state(&home, Ok(PathBuf::from("/tmp/bridge")));
        assert_eq!(state.profile, None);
        assert_eq!(state.profiles, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert!(state.detail.contains("无法确定装到哪个"));
        fs::remove_dir_all(home).unwrap();
    }
}
