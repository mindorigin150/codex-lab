use assert_cmd::Command;

#[test]
fn debug_bwrap_digest_reports_the_embedded_digest() -> anyhow::Result<()> {
    let output = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .args(["debug", "bwrap-digest"])
        .output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("digest output is UTF-8");
    let digest = stdout.trim();
    assert!(
        digest == "none"
            || (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );
    Ok(())
}
