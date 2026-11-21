use std::ffi::OsStr;

use anyhow::Result;

fn needs_multi_thread_runtime<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iter = args.into_iter();
    let _bin = iter.next();
    match iter.next() {
        None => true,
        Some(arg) => match arg.as_ref().to_str() {
            Some("tui" | "chat" | "serve" | "api" | "mcp" | "mcp-server") => true,
            Some("daemon") => matches!(
                iter.next().as_ref().and_then(|s| s.as_ref().to_str()),
                Some("run")
            ),
            _ => false,
        },
    }
}

fn main() -> Result<()> {
    #[cfg(all(windows, debug_assertions))]
    {
        // Unoptimized argument parsing and command futures can exceed the Windows stack.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(run_cli)?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
    #[cfg(not(all(windows, debug_assertions)))]
    run_cli()
}

fn run_cli() -> Result<()> {
    if let Some(res) = teams_cli::cli::try_fast_help_or_version() {
        return res;
    }

    if needs_multi_thread_runtime(std::env::args_os()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let _ = rustls::crypto::ring::default_provider().install_default();
            tokio::task::spawn_blocking(teams_cli::updater::cleanup_stale_old_binary);
            teams_cli::cli::run_from_env().await
        })
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        teams_cli::updater::cleanup_stale_old_binary();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(teams_cli::cli::run_from_env())
    }
}

#[cfg(test)]
mod tests {
    use super::needs_multi_thread_runtime;

    #[test]
    fn detects_multi_thread_commands_only_for_tui_serve_mcp() {
        assert!(needs_multi_thread_runtime(["teams"]));
        assert!(needs_multi_thread_runtime(["teams", "tui"]));
        assert!(needs_multi_thread_runtime(["teams", "chat"]));
        assert!(needs_multi_thread_runtime(["teams", "serve"]));
        assert!(needs_multi_thread_runtime(["teams", "api"]));
        assert!(needs_multi_thread_runtime(["teams", "mcp"]));
        assert!(needs_multi_thread_runtime(["teams", "mcp-server"]));
        assert!(needs_multi_thread_runtime(["teams", "daemon", "run"]));

        for sub in ["status", "stop", "start", "restart"] {
            assert!(
                !needs_multi_thread_runtime(["teams", "daemon", sub]),
                "daemon {sub}"
            );
        }

        for cmd in [
            "chats", "teams", "channels", "read", "thread", "send", "reply", "sync", "search",
            "whoami", "doctor", "--help", "-h", "--version", "-V",
        ] {
            assert!(!needs_multi_thread_runtime(["teams", cmd]), "{cmd}");
        }
    }
}
