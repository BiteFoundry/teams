//! End-to-end integration tests against an in-process mock Microsoft Graph v1.0 + Entra OAuth server.

mod channels_paging;
mod cli_and_markdown;
mod core_flows;
mod history_pagination;
mod media_and_mcp;
mod server;
mod threads_export;

#[cfg(windows)]
#[test]
fn windows_cli_starts_and_reports_its_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .arg("--version")
        .output()
        .expect("start Windows CLI");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("teams {}", env!("CARGO_PKG_VERSION"))
    );
}
