use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;

use super::Phase;
use super::explorer_permission_profile;
use super::packet_has_valid_role_prefix;

#[test]
fn explorer_permissions_intersect_parent_allowlist_with_read_only() {
    let explorer = explorer_permission_profile(&PermissionProfile::workspace_write());
    let file_system = explorer.file_system_sandbox_policy();

    assert_eq!(file_system.kind, FileSystemSandboxKind::Restricted);
    assert!(
        file_system
            .entries
            .iter()
            .all(|entry| entry.access != FileSystemAccessMode::Write)
    );
    assert_eq!(
        explorer.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
}

#[test]
fn explorer_permissions_reduce_unrestricted_parent_to_read_only() {
    assert_eq!(
        explorer_permission_profile(&PermissionProfile::Disabled),
        PermissionProfile::read_only()
    );
}

#[test]
fn worker_plan_packet_requires_exact_nonempty_role_prefix() {
    assert!(packet_has_valid_role_prefix(
        "worker-plan: inspect then edit",
        Phase::WorkerPlan
    ));
    assert!(!packet_has_valid_role_prefix(
        "worker-plan:",
        Phase::WorkerPlan
    ));
    assert!(!packet_has_valid_role_prefix(
        "orc: worker-plan: inspect then edit",
        Phase::WorkerPlan
    ));
}
