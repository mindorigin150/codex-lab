use super::effective_file_system_sandbox_policy;
use super::intersect_permission_profiles;
use super::intersect_runtime_permission_profiles;
use super::merge_file_system_policy_with_additional_permissions;
use super::normalize_additional_permissions;
use super::should_require_platform_sandbox;
use codex_protocol::models::AdditionalPermissionProfile as PermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::ManagedFileSystemPermissions;
use codex_protocol::models::NetworkPermissions;
use codex_protocol::models::PermissionProfile as RuntimePermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use dunce::canonicalize;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

fn managed_runtime_profile(
    entries: Vec<FileSystemSandboxEntry>,
    network: NetworkSandboxPolicy,
) -> RuntimePermissionProfile {
    RuntimePermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Restricted {
            entries,
            glob_scan_max_depth: None,
        },
        network,
    }
}

#[test]
fn runtime_profile_intersection_keeps_read_only_under_unrestricted_parent() {
    let temp_dir = TempDir::new().expect("create temp dir");

    assert_eq!(
        intersect_runtime_permission_profiles(
            RuntimePermissionProfile::read_only(),
            RuntimePermissionProfile::Disabled,
            temp_dir.path(),
        ),
        RuntimePermissionProfile::read_only()
    );
}

#[test]
fn runtime_profile_intersection_limits_restricted_allowlist_and_preserves_deny() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let allowed = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("allowed"))
        .expect("allowed path should be absolute");
    let denied = allowed.join("secret");
    let parent = managed_runtime_profile(
        vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: allowed.clone(),
                },
                access: FileSystemAccessMode::Write,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: denied.clone(),
                },
                access: FileSystemAccessMode::Deny,
            },
        ],
        NetworkSandboxPolicy::Enabled,
    );

    let intersected = intersect_runtime_permission_profiles(
        RuntimePermissionProfile::read_only(),
        parent,
        temp_dir.path(),
    );
    let policy = intersected.file_system_sandbox_policy();

    assert_eq!(
        policy.resolve_access_with_cwd(allowed.as_path(), temp_dir.path()),
        FileSystemAccessMode::Read
    );
    assert_eq!(
        policy.resolve_access_with_cwd(denied.as_path(), temp_dir.path()),
        FileSystemAccessMode::Deny
    );
    assert_eq!(
        intersected.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
}

#[test]
fn runtime_profile_intersection_downgrades_workspace_write_to_read_only() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let workspace = AbsolutePathBuf::from_absolute_path(temp_dir.path())
        .expect("workspace path should be absolute");
    let parent = RuntimePermissionProfile::workspace_write_with(
        std::slice::from_ref(&workspace),
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );

    let intersected = intersect_runtime_permission_profiles(
        RuntimePermissionProfile::read_only(),
        parent,
        temp_dir.path(),
    );
    let policy = intersected.file_system_sandbox_policy();

    assert_eq!(
        policy.resolve_access_with_cwd(temp_dir.path(), temp_dir.path()),
        FileSystemAccessMode::Read
    );
    assert_eq!(policy.has_full_disk_write_access(), false);
}

#[test]
fn runtime_profile_intersection_preserves_minimal_platform_roots_under_root_read() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let parent = managed_runtime_profile(
        vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            access: FileSystemAccessMode::Read,
        }],
        NetworkSandboxPolicy::Restricted,
    );

    let intersected = intersect_runtime_permission_profiles(
        RuntimePermissionProfile::read_only(),
        parent,
        temp_dir.path(),
    );
    let policy = intersected.file_system_sandbox_policy();

    assert!(policy.include_platform_defaults());
    assert!(policy.entries.iter().any(|entry| {
        entry.access == FileSystemAccessMode::Read
            && matches!(
                &entry.path,
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::Minimal
                }
            )
    }));
}

#[test]
fn runtime_profile_intersection_preserves_unbounded_deny_glob_scan_depth() {
    fn profile_with_depth(
        entries: Vec<FileSystemSandboxEntry>,
        glob_scan_max_depth: Option<usize>,
    ) -> RuntimePermissionProfile {
        RuntimePermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries,
                glob_scan_max_depth: glob_scan_max_depth.and_then(std::num::NonZeroUsize::new),
            },
            network: NetworkSandboxPolicy::Restricted,
        }
    }

    fn root_read() -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        }
    }

    fn deny_glob(pattern: &str) -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: pattern.to_string(),
            },
            access: FileSystemAccessMode::Deny,
        }
    }

    fn intersection_depth(
        requested: RuntimePermissionProfile,
        granted: RuntimePermissionProfile,
        cwd: &Path,
    ) -> Option<usize> {
        let RuntimePermissionProfile::Managed {
            file_system:
                ManagedFileSystemPermissions::Restricted {
                    glob_scan_max_depth,
                    ..
                },
            ..
        } = intersect_runtime_permission_profiles(requested, granted, cwd)
        else {
            panic!("intersection should remain a restricted managed profile");
        };
        glob_scan_max_depth.map(usize::from)
    }

    let temp_dir = TempDir::new().expect("create temp dir");
    let requested =
        profile_with_depth(vec![root_read(), deny_glob("**/requested-secret/**")], None);
    let granted = profile_with_depth(
        vec![root_read(), deny_glob("**/granted-secret/**")],
        Some(3),
    );
    assert_eq!(
        intersection_depth(requested, granted, temp_dir.path()),
        None
    );

    let requested =
        profile_with_depth(vec![root_read(), deny_glob("**/requested-secret/**")], None);
    let granted = profile_with_depth(vec![root_read()], Some(3));
    assert_eq!(
        intersection_depth(requested, granted, temp_dir.path()),
        None
    );
}

#[cfg(unix)]
fn symlink_dir(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[test]
fn full_access_restricted_policy_skips_platform_sandbox_when_network_is_enabled() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    }]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Enabled,
            /*has_managed_network_requirements*/ false
        ),
        false
    );
}

#[test]
fn root_write_policy_with_carveouts_still_uses_platform_sandbox() {
    let blocked = AbsolutePathBuf::resolve_path_against_base(
        "blocked",
        std::env::current_dir().expect("current dir"),
    );
    let policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: blocked },
            access: FileSystemAccessMode::Deny,
        },
    ]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Enabled,
            /*has_managed_network_requirements*/ false
        ),
        true
    );
}

#[test]
fn full_access_restricted_policy_still_uses_platform_sandbox_for_restricted_network() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    }]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Restricted,
            /*has_managed_network_requirements*/ false
        ),
        true
    );
}

#[test]
fn normalize_additional_permissions_preserves_network() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let permissions = normalize_additional_permissions(PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(vec![path.clone()]),
        )),
    })
    .expect("permissions");

    assert_eq!(
        permissions.network,
        Some(NetworkPermissions {
            enabled: Some(true),
        })
    );
    assert_eq!(
        permissions.file_system,
        Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(vec![path]),
        ))
    );
}

#[cfg(unix)]
#[test]
fn normalize_additional_permissions_preserves_symlinked_write_paths() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let real_root = temp_dir.path().join("real");
    let link_root = temp_dir.path().join("link");
    let write_dir = real_root.join("write");
    std::fs::create_dir_all(&write_dir).expect("create write dir");
    symlink_dir(&real_root, &link_root).expect("create symlinked root");

    let link_write_dir =
        AbsolutePathBuf::from_absolute_path(link_root.join("write")).expect("link write dir");
    let permissions = normalize_additional_permissions(PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![link_write_dir]),
        )),
        ..Default::default()
    })
    .expect("permissions");

    assert_eq!(
        permissions.file_system,
        Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![
                AbsolutePathBuf::from_absolute_path(link_root.join("write"))
                    .expect("link write dir"),
            ]),
        ))
    );
}

#[test]
fn normalize_additional_permissions_rejects_glob_read_grants() {
    let err = normalize_additional_permissions(PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: "**/*.env".to_string(),
                },
                access: FileSystemAccessMode::Read,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    })
    .expect_err("read glob permissions are unsupported");

    assert_eq!(
        err,
        "glob file system permissions only support deny-read entries"
    );
}

#[test]
fn normalize_additional_permissions_preserves_deny_globs() {
    let permissions = normalize_additional_permissions(PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: "**/*.env".to_string(),
                },
                access: FileSystemAccessMode::Deny,
            }],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    })
    .expect("deny glob permissions are supported");

    assert_eq!(
        permissions,
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: "**/*.env".to_string(),
                    },
                    access: FileSystemAccessMode::Deny,
                }],
                glob_scan_max_depth: std::num::NonZeroUsize::new(2),
            }),
            ..Default::default()
        }
    );
}

#[test]
fn normalize_additional_permissions_drops_empty_nested_profiles() {
    let permissions = normalize_additional_permissions(PermissionProfile {
        network: Some(NetworkPermissions { enabled: None }),
        file_system: Some(FileSystemPermissions::default()),
    })
    .expect("permissions");

    assert_eq!(permissions, PermissionProfile::default());
}

#[test]
fn intersect_permission_profiles_preserves_explicit_empty_requested_reads() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![path]),
        )),
        ..Default::default()
    };
    let granted = requested.clone();

    assert_eq!(
        intersect_permission_profiles(requested.clone(), granted, temp_dir.path()),
        requested
    );
}

#[test]
fn intersect_permission_profiles_drops_ungranted_nonempty_path_requests() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path]),
            /*write*/ None,
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, PermissionProfile::default(), temp_dir.path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_explicit_empty_reads_without_grant() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![path]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, PermissionProfile::default(), temp_dir.path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_accepts_child_path_granted_for_requested_cwd() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let child = cwd.join("child");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![child]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted.clone(), cwd.as_path()),
        granted
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_grant_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let later_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("later-cwd"))
        .expect("absolute later cwd");
    let cwd_write_permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    let intersected = intersect_permission_profiles(
        cwd_write_permissions.clone(),
        cwd_write_permissions,
        request_cwd.as_path(),
    );

    assert_eq!(
        intersected,
        PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![request_cwd]),
            )),
            ..Default::default()
        }
    );
    assert_eq!(
        intersect_permission_profiles(
            PermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_roots(
                    /*read*/ None,
                    Some(vec![later_cwd.join("child")]),
                )),
                ..Default::default()
            },
            intersected,
            later_cwd.as_path(),
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_deduplicates_materialized_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd =
        AbsolutePathBuf::from_absolute_path(temp_dir.path().join("cwd")).expect("absolute cwd");
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path: cwd.clone() },
                    access: FileSystemAccessMode::Write,
                },
            ],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(permissions.clone(), permissions, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![cwd]),
            )),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_deny_entries() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(permissions.clone(), permissions, request_cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Write,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Path { path: request_cwd },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_drops_deny_entries_without_filesystem_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let secret = cwd.join("secret");
    let requested = PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path: secret },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
    };
    let granted = PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted.clone(), cwd.as_path()),
        granted
    );
}

#[test]
fn intersect_permission_profiles_rejects_concrete_grants_matched_by_requested_deny_globs() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let env_file = cwd.join(".env");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: "**/*.env".to_string(),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![env_file]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_materializes_relative_deny_globs_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let later_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("later-cwd"))
        .expect("absolute later cwd");
    let cwd_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![cwd_write, deny_env_files],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };

    let intersected =
        intersect_permission_profiles(permissions.clone(), permissions, request_cwd.as_path());

    assert_eq!(
        intersected,
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Path {
                            path: request_cwd.clone(),
                        },
                        access: FileSystemAccessMode::Write,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: request_cwd.join("**/*.env").to_string_lossy().into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: std::num::NonZeroUsize::new(2),
            }),
            ..Default::default()
        }
    );
    assert_eq!(
        intersect_permission_profiles(
            PermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_roots(
                    /*read*/ None,
                    Some(vec![later_cwd.join("token.env")]),
                )),
                ..Default::default()
            },
            intersected,
            later_cwd.as_path(),
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_broader_cwd_grant_for_requested_child_path() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let child = cwd.join("child");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![child]),
        )),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_uses_granted_bounded_glob_scan_depth() {
    let cwd = std::env::current_dir().expect("current dir");
    let root_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files],
            glob_scan_max_depth: std::num::NonZeroUsize::new(4),
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    root_write,
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: AbsolutePathBuf::resolve_path_against_base(
                                "**/*.env",
                                cwd.as_path()
                            )
                            .to_string_lossy()
                            .into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: std::num::NonZeroUsize::new(4),
            }),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_uses_granted_unbounded_glob_scan_depth() {
    let cwd = std::env::current_dir().expect("current dir");
    let root_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    root_write,
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: AbsolutePathBuf::resolve_path_against_base(
                                "**/*.env",
                                cwd.as_path()
                            )
                            .to_string_lossy()
                            .into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        }
    );
}

#[test]
fn merge_file_system_policy_with_additional_permissions_preserves_unreadable_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: denied_path.clone(),
                },
                access: FileSystemAccessMode::Deny,
            },
        ]),
        &FileSystemPermissions::from_read_write_roots(
            Some(vec![allowed_path.clone()]),
            Some(Vec::new()),
        ),
    );

    assert_eq!(
        merged_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        }),
        true
    );
    assert_eq!(
        merged_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: allowed_path },
            access: FileSystemAccessMode::Read,
        }),
        true
    );
}

#[test]
fn merge_file_system_policy_with_additional_permissions_carries_bounded_glob_scan_depth() {
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
        }]),
        &FileSystemPermissions {
            entries: vec![deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        },
    );

    assert_eq!(merged_policy, {
        let mut policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Write,
            },
            deny_env_files,
        ]);
        policy.glob_scan_max_depth = Some(2);
        policy
    });
}

#[test]
fn effective_file_system_sandbox_policy_returns_base_policy_without_additional_permissions() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        },
    ]);

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, /*additional_permissions*/ None);

    assert_eq!(effective_policy, base_policy);
}

#[test]
fn effective_file_system_sandbox_policy_merges_additional_write_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: denied_path.clone(),
            },
            access: FileSystemAccessMode::Deny,
        },
    ]);
    let additional_permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![allowed_path.clone()]),
        )),
        ..Default::default()
    };

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, Some(&additional_permissions));

    assert_eq!(
        effective_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        }),
        true
    );
    assert_eq!(
        effective_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: allowed_path },
            access: FileSystemAccessMode::Write,
        }),
        true
    );
}
