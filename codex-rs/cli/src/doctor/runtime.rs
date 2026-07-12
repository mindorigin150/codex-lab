//! Captures how this Codex process was launched.
//!
//! Runtime diagnostics answer provenance questions that are hard to infer from
//! user reports: which binary is running, which install channel it resembles,
//! which platform it targets, and whether the search command comes from bundled
//! package files or from PATH.

use std::env;
use std::process::Command;

use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_protocol::models::PermissionProfile;

use super::CheckStatus;
use super::DoctorCheck;
use super::describe_install_context;
use super::doctor_install_context;
use super::push_path_detail;

/// Builds the process provenance row for the current Codex executable.
///
/// This check is informational and should not fail on its own; inconsistent
/// install state is reported by the installation and update checks instead.
pub(super) fn runtime_check() -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let platform = format!("{os}-{arch}");
    let install_method = install_method_name(&install_context);
    let mut details = vec![
        format!("version: {}", env!("CARGO_PKG_VERSION")),
        format!("platform: {platform}"),
        format!(
            "install method: {}",
            describe_install_context(&install_context)
        ),
        format!("commit: {}", build_commit()),
    ];
    push_path_detail(&mut details, "current executable", current_exe.as_deref());

    DoctorCheck::new(
        "runtime.provenance",
        "runtime",
        CheckStatus::Ok,
        format!("running {install_method} on {platform}"),
    )
    .details(details)
}

/// Verifies that the search command selected by the install context is usable.
///
/// Package-layout installs should point at a bundled ripgrep binary, while local
/// installs without that layout usually resolve rg from PATH. A warning here
/// means features that depend on file search may degrade even when the CLI
/// launches.
pub(super) fn search_check() -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let rg_command = install_context.rg_command();
    let provider = search_provider(&install_context);
    let mut details = vec![
        format!("search command: {}", rg_command.display()),
        format!("search provider: {provider}"),
    ];

    let status = if rg_command.components().count() > 1 {
        match std::fs::metadata(&rg_command) {
            Ok(metadata) if metadata.is_file() => {
                details.push("search command readiness: file exists".to_string());
                CheckStatus::Ok
            }
            Ok(_) => {
                details.push("search command readiness: path is not a file".to_string());
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    } else {
        match Command::new(&rg_command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("rg version unknown")
                    .to_string();
                details.push(format!("search command readiness: {version}"));
                CheckStatus::Ok
            }
            Ok(output) => {
                details.push(format!(
                    "search command readiness: exited with status {}",
                    output.status
                ));
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    };

    let summary = match status {
        CheckStatus::Ok => format!("search is OK ({provider})"),
        CheckStatus::Warning => "search command could not be verified".to_string(),
        CheckStatus::Fail => unreachable!(),
    };
    let mut check = DoctorCheck::new("runtime.search", "search", status, summary).details(details);
    if status != CheckStatus::Ok {
        check = check.remediation("Install ripgrep or repair the bundled Codex package.");
    }
    check
}

/// Verifies the exact Linux filesystem sandbox path used by read-only sub-agents.
pub(super) async fn linux_sandbox_check(config: &codex_core::config::Config) -> DoctorCheck {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        DoctorCheck::new(
            "runtime.linux_sandbox",
            "runtime",
            CheckStatus::Ok,
            "Linux bubblewrap sandbox is not required on this platform",
        )
    }

    #[cfg(target_os = "linux")]
    {
        let mut probe_config = config.clone();
        if let Err(error) = probe_config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
        {
            return DoctorCheck::new(
                "runtime.linux_sandbox",
                "runtime",
                CheckStatus::Fail,
                "could not construct the read-only sandbox profile",
            )
            .detail(error.to_string());
        }
        let helper = probe_config
            .codex_linux_sandbox_exe
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unavailable".to_string());
        match codex_core::probe_spawn_agent_sandbox(&probe_config).await {
            Ok(()) => DoctorCheck::new(
                "runtime.linux_sandbox",
                "runtime",
                CheckStatus::Ok,
                "the effective read-only Linux sandbox preflight succeeded",
            )
            .detail(format!("sandbox helper: {helper}"))
            .detail("sandbox probe: /bin/true succeeded under the effective read-only profile"),
            Err(error) => DoctorCheck::new(
                "runtime.linux_sandbox",
                "runtime",
                CheckStatus::Fail,
                "the configured helper cannot create the read-only Linux sandbox",
            )
            .detail(format!("sandbox helper: {helper}"))
            .detail(format!("sandbox probe: {error}"))
            .remediation("Install bubblewrap (`sudo apt-get update && sudo apt-get install -y bubblewrap` on Debian/Ubuntu), restart Codex Lab, and rerun `codex-lab doctor`."),
        }
    }
}

fn install_method_name(context: &InstallContext) -> &'static str {
    match &context.method {
        InstallMethod::Standalone { .. } => "standalone",
        InstallMethod::Npm => "npm",
        InstallMethod::Bun => "bun",
        InstallMethod::Pnpm => "pnpm",
        InstallMethod::Brew => "brew",
        InstallMethod::Other => "local build",
    }
}

fn search_provider(context: &InstallContext) -> &'static str {
    let rg_command = context.rg_command();
    let from_package_layout = context
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
        .is_some_and(|path_dir| rg_command.starts_with(path_dir));
    let from_legacy_standalone = matches!(
        &context.method,
        InstallMethod::Standalone {
            resources_dir: Some(resources_dir),
            ..
        } if rg_command.starts_with(resources_dir)
    );

    if from_package_layout || from_legacy_standalone {
        "bundled"
    } else {
        "system"
    }
}

fn build_commit() -> &'static str {
    option_env!("CODEX_BUILD_COMMIT")
        .or(option_env!("GIT_COMMIT"))
        .unwrap_or("unknown")
}
