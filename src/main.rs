mod cache;
mod colors;
mod config;
mod error;
mod models;
mod server;
mod setup;
mod stats;

mod tui;

mod auth;
mod cloudcode;
mod format;

use std::env;
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use auth::accounts::AccountStore;
use auth::{Account, HttpClient};
use cache::ResponseCache;
use cloudcode::CloudCodeClient;
use colors::*;
use config::Config;
use server::ServerState;

/// A simple animated spinner for terminal feedback
struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn new(message: &str) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let message = message.to_string();

        let handle = std::thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0;
            while running_clone.load(Ordering::Relaxed) {
                print!("\r\x1b[36m{}\x1b[0m {}", frames[i % frames.len()], message);
                let _ = std::io::stdout().flush();
                std::thread::sleep(std::time::Duration::from_millis(80));
                i += 1;
            }
            print!("\r\x1b[K");
            let _ = std::io::stdout().flush();
        });

        Self {
            running,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn get_log_path() -> std::path::PathBuf {
    Config::dir().join("agcp.log")
}

fn get_pid_path() -> std::path::PathBuf {
    Config::dir().join("agcp.pid")
}

fn get_lock_path() -> std::path::PathBuf {
    Config::dir().join("agcp.lock")
}

fn parse_strategy(s: &str) -> Option<auth::accounts::SelectionStrategy> {
    use auth::accounts::SelectionStrategy;
    match s.to_lowercase().as_str() {
        "sticky" => Some(SelectionStrategy::Sticky),
        "roundrobin" | "round-robin" | "rr" => Some(SelectionStrategy::RoundRobin),
        "hybrid" | "smart" => Some(SelectionStrategy::Hybrid),
        _ => None,
    }
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args: Vec<String> = env::args().collect();

    // Check for subcommands first
    if args.len() > 1 {
        match args[1].as_str() {
            "logs" => {
                run_logs_command(&args[2..]);
                return;
            }
            "stop" => {
                run_stop_command();
                return;
            }
            "restart" => {
                run_restart_command().await;
                return;
            }
            "status" => {
                run_status_command();
                return;
            }
            "login" => {
                init_logging_foreground(false);
                let no_browser = args.iter().any(|a| a == "--no-browser");
                if let Err(e) = run_login(no_browser).await {
                    eprintln!("\x1b[31mLogin failed:\x1b[0m {}", e);
                    // Provide specific recovery suggestions based on error type
                    if let Some(suggestion) = e.suggestion() {
                        eprintln!();
                        eprintln!("  \x1b[33mTip:\x1b[0m {}", suggestion);
                    }
                    // Additional context for common issues
                    let err_str = e.to_string().to_lowercase();
                    if err_str.contains("timeout") || err_str.contains("connection") {
                        eprintln!();
                        eprintln!(
                            "  \x1b[2mCheck your internet connection and firewall settings.\x1b[0m"
                        );
                    } else if err_str.contains("callback") || err_str.contains("cancelled") {
                        eprintln!();
                        eprintln!(
                            "  \x1b[2mIf your browser didn't open, try: agcp login --no-browser\x1b[0m"
                        );
                    }
                    std::process::exit(1);
                }
                return;
            }
            "quota" => {
                if let Err(e) = run_quota_command().await {
                    eprintln!("\x1b[31mFailed to fetch quotas:\x1b[0m {}", e);
                    std::process::exit(1);
                }
                return;
            }
            "doctor" => {
                run_doctor_command().await;
                return;
            }
            "test" => {
                run_test_command().await;
                return;
            }
            "config" => {
                run_config_command();
                return;
            }
            "stats" => {
                run_stats_command().await;
                return;
            }
            "setup" => {
                setup::run_setup_command(&args[2..]);
                return;
            }
            "accounts" => {
                run_accounts_command(&args[2..]).await;
                return;
            }
            "-h" | "--help" | "help" => {
                print_help();
                return;
            }
            "-V" | "--version" | "version" => {
                println!("agcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "completions" => {
                if args.len() > 2 {
                    print_completions(&args[2]);
                } else {
                    eprintln!("Usage: agcp completions <bash|zsh|fish>");
                    std::process::exit(1);
                }
                return;
            }
            "upgrade" => {
                run_upgrade_command().await;
                return;
            }
            "tui" => {
                if let Err(e) = tui::run() {
                    eprintln!("\x1b[31mTUI error:\x1b[0m {}", e);
                    std::process::exit(1);
                }
                return;
            }
            arg if !arg.starts_with('-') => {
                eprintln!("\x1b[31mUnknown command:\x1b[0m {}", arg);
                eprintln!();
                eprintln!("Run '\x1b[33magcp --help\x1b[0m' for usage information.");
                std::process::exit(1);
            }
            _ => {} // Options like --port, --debug are handled below
        }
    }

    let mut port: Option<u16> = None;
    let mut host: Option<String> = None;
    let mut foreground = false;
    let mut debug = false;
    let mut fallback = false;
    let mut network = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<u16>() {
                        Ok(p) if p > 0 => port = Some(p),
                        _ => {
                            eprintln!(
                                "\x1b[31mInvalid port:\x1b[0m '{}' is not a valid port number (1-65535)",
                                args[i]
                            );
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("\x1b[31mMissing value:\x1b[0m --port requires a port number");
                    std::process::exit(1);
                }
            }
            "--host" => {
                i += 1;
                if i < args.len() {
                    host = Some(args[i].clone());
                } else {
                    eprintln!(
                        "\x1b[31mMissing value:\x1b[0m --host requires a hostname or IP address"
                    );
                    std::process::exit(1);
                }
            }
            "--foreground" | "-f" => foreground = true,
            "--debug" | "-d" => debug = true,
            "--fallback" => fallback = true,
            "--network" | "--lan" => network = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-V" | "--version" => {
                println!("agcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            arg if arg.starts_with('-') => {
                eprintln!("\x1b[31mUnknown option:\x1b[0m {}", arg);
                eprintln!();
                eprintln!("Run '\x1b[33magcp --help\x1b[0m' for usage information.");
                std::process::exit(1);
            }
            _ => {} // Values for --port/--host are consumed above
        }
        i += 1;
    }

    let config = match Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("\x1b[31mError:\x1b[0m {}", e);
            if let config::ConfigError::ParseError { path, source } = &e {
                // Show more helpful info for parse errors
                eprintln!();
                eprintln!("  Config file: {}", path.display());
                // Extract line/column info if available
                let msg = source.to_string();
                if let Some(line_info) = msg.split(" at line ").nth(1) {
                    eprintln!(
                        "  Location: line {}",
                        line_info.split_whitespace().next().unwrap_or("?")
                    );
                }
                eprintln!();
                eprintln!("  \x1b[2mFix the syntax error and try again.\x1b[0m");
            }
            std::process::exit(1);
        }
    };
    let mut config = config.with_overrides(port, host, debug);

    // Apply fallback flag if specified on command line
    if fallback {
        config.accounts.fallback = true;
    }

    // Apply network mode - bind to all interfaces
    if network {
        config.server.host = "0.0.0.0".to_string();
    }

    // Initialize global config for access from other modules
    config::init_config(config.clone());

    if foreground {
        init_logging_foreground(debug);
        run_server(config).await;
    } else {
        run_daemon(config, debug).await;
    }
}

async fn run_daemon(config: Config, debug: bool) {
    // Check for accounts before daemonizing (so user sees the error)
    match AccountStore::load() {
        Ok(store) if store.accounts.is_empty() => {
            eprintln!("\x1b[33m●\x1b[0m No accounts configured");
            eprintln!();
            eprintln!("  Run '\x1b[32magcp login\x1b[0m' to authenticate with Google.");
            eprintln!();
            std::process::exit(1);
        }
        Err(e) => {
            let err_str = e.to_string().to_lowercase();
            // Check if this looks like a JSON parsing error (corruption)
            if err_str.contains("json")
                || err_str.contains("parse")
                || err_str.contains("expected")
                || err_str.contains("missing field")
            {
                if handle_corrupted_accounts_file(&e) {
                    // User chose to reset, try loading again
                    match AccountStore::load() {
                        Ok(store) if store.accounts.is_empty() => {
                            eprintln!(
                                "\x1b[33m●\x1b[0m Accounts reset. Run '\x1b[32magcp login\x1b[0m' to add an account."
                            );
                            eprintln!();
                            std::process::exit(1);
                        }
                        Ok(_) => {} // Continue with recovered accounts
                        Err(e2) => {
                            eprintln!("\x1b[31m●\x1b[0m Still failed to load accounts: {}", e2);
                            std::process::exit(1);
                        }
                    }
                } else {
                    std::process::exit(1);
                }
            } else {
                eprintln!("\x1b[31m●\x1b[0m Failed to load accounts: {}", e);
                eprintln!();
                eprintln!("  Run '\x1b[32magcp login\x1b[0m' to set up an account.");
                eprintln!();
                std::process::exit(1);
            }
        }
        Ok(_) => {} // Accounts exist, continue
    }

    // Try to acquire exclusive lock - if we can't, another instance is running
    let _lock_file = match try_acquire_lock() {
        Some(lock) => lock,
        None => {
            // Another instance has the lock - check if it's responsive
            if let Some(pid) = read_pid() {
                let addr = format!("{}:{}", config.host(), config.port());
                let is_responsive = addr
                    .parse()
                    .ok()
                    .and_then(|socket_addr| {
                        std::net::TcpStream::connect_timeout(
                            &socket_addr,
                            std::time::Duration::from_secs(2),
                        )
                        .ok()
                    })
                    .is_some();

                if is_responsive {
                    println!("\x1b[32m●\x1b[0m AGCP is already running (PID: {})", pid);
                    print_listening_address(config.host(), config.port());
                    println!();
                    println!("  \x1b[2mUse 'agcp logs' to view logs\x1b[0m");
                    println!("  \x1b[2mUse 'agcp stop' to stop the server\x1b[0m");
                    return;
                }
            }
            eprintln!("\x1b[31m●\x1b[0m Another AGCP instance is starting");
            eprintln!("  Wait a moment and try again");
            std::process::exit(1);
        }
    };

    // Check if already running (fallback for stale lock files)
    if let Some(pid) = read_pid()
        && is_process_running(pid)
    {
        // Verify server is actually responsive (not a zombie)
        let addr = format!("{}:{}", config.host(), config.port());
        let is_responsive = addr
            .parse()
            .ok()
            .and_then(|socket_addr| {
                std::net::TcpStream::connect_timeout(
                    &socket_addr,
                    std::time::Duration::from_secs(2),
                )
                .ok()
            })
            .is_some();

        if is_responsive {
            println!("\x1b[32m●\x1b[0m AGCP is already running (PID: {})", pid);
            print_listening_address(config.host(), config.port());
            println!();
            println!("  \x1b[2mUse 'agcp logs' to view logs\x1b[0m");
            println!("  \x1b[2mUse 'agcp stop' to stop the server\x1b[0m");
            return;
        } else {
            // PID exists but server not responding - clean up stale PID
            eprintln!(
                "\x1b[33m●\x1b[0m Found stale PID file (process {} not responding), cleaning up...",
                pid
            );
            let _ = std::fs::remove_file(get_pid_path());
        }
    }

    // Check if port is available before trying to start
    if !is_port_available(config.host(), config.port()) {
        eprintln!("\x1b[31m●\x1b[0m Port {} is already in use", config.port());
        if let Some(process) = find_process_using_port(config.port()) {
            eprintln!("  Process using port: \x1b[33m{}\x1b[0m", process);
        }
        eprintln!();
        eprintln!(
            "  Try a different port: \x1b[36magcp --port {}\x1b[0m",
            config.port() + 1
        );
        std::process::exit(1);
    }

    // Fork to background
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let exe = env::current_exe().expect("Failed to get current exe");
        let log_path = get_log_path();

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Rotate log file if it's too large (> 10MB)
        const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;
        if let Ok(metadata) = std::fs::metadata(&log_path)
            && metadata.len() > MAX_LOG_SIZE
        {
            let backup_path = log_path.with_extension("log.old");
            let _ = std::fs::remove_file(&backup_path); // Remove old backup
            let _ = std::fs::rename(&log_path, &backup_path); // Rotate current to backup
        }

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("Failed to open log file");

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--foreground");
        if let Some(p) = config.port().checked_sub(0) {
            cmd.args(["--port", &p.to_string()]);
        }
        cmd.args(["--host", config.host()]);
        if debug {
            cmd.arg("--debug");
        }
        if config.accounts.fallback {
            cmd.arg("--fallback");
        }

        cmd.stdout(log_file.try_clone().expect("Failed to clone log file"));
        cmd.stderr(log_file);

        // Detach from terminal
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                write_pid(pid);

                // Show spinner while waiting for startup
                let spinner = Spinner::new("Starting AGCP...");
                std::thread::sleep(std::time::Duration::from_millis(500));
                spinner.stop();

                if is_process_running(pid) {
                    println!("\x1b[32m●\x1b[0m AGCP started (PID: {})", pid);
                    print_listening_address(config.host(), config.port());
                    println!();
                    println!("  \x1b[2mUse 'agcp logs' to view logs\x1b[0m");
                    println!("  \x1b[2mUse 'agcp stop' to stop the server\x1b[0m");
                } else {
                    eprintln!("\x1b[31m●\x1b[0m AGCP failed to start. Check logs:");
                    eprintln!("  agcp logs");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("\x1b[31m●\x1b[0m Failed to start daemon: {}", e);
                std::process::exit(1);
            }
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix, just run in foreground
        init_logging_foreground(debug);
        run_server(config).await;
    }
}

async fn run_server(config: Config) {
    let mut accounts = match AccountStore::load() {
        Ok(store) => {
            if store.accounts.is_empty() {
                error!("No accounts configured. Run 'agcp login' to authenticate.");
                std::process::exit(1);
            }
            let enabled_count = store
                .accounts
                .iter()
                .filter(|a| a.enabled && !a.is_invalid)
                .count();
            info!(
                total = store.accounts.len(),
                enabled = enabled_count,
                strategy = ?store.strategy,
                "Loaded accounts"
            );
            store
        }
        Err(e) => {
            error!(error = %e, "Failed to load accounts");
            std::process::exit(1);
        }
    };

    // Apply config overrides for strategy and quota threshold
    if let Some(strategy) = parse_strategy(&config.accounts.strategy)
        && accounts.strategy != strategy
    {
        info!(strategy = ?strategy, "Using strategy from config");
        accounts.strategy = strategy;
    }
    accounts.quota_threshold = config.accounts.quota_threshold;

    let http_client = HttpClient::new();

    // Verify at least one account has valid credentials by getting a token
    let first_enabled = accounts
        .accounts
        .iter_mut()
        .find(|a| a.enabled && !a.is_invalid);

    if let Some(account) = first_enabled {
        match account.get_access_token(&http_client).await {
            Ok(access_token) => {
                // Try to discover/update project ID and subscription tier
                let existing_project = account.project_id.as_deref();
                match cloudcode::discover_project_and_tier(
                    &http_client,
                    &access_token,
                    existing_project,
                )
                .await
                {
                    Ok(result) => {
                        if let Some(ref project_id) = result.project_id
                            && account.project_id.as_deref() != Some(project_id)
                        {
                            info!(
                                email = %account.email,
                                project_id = %project_id,
                                "Updated project ID from loadCodeAssist"
                            );
                            account.project_id = result.project_id.clone();
                        }
                        if result.subscription_tier != account.subscription_tier {
                            info!(
                                email = %account.email,
                                tier = ?result.subscription_tier,
                                "Updated subscription tier from loadCodeAssist"
                            );
                            account.subscription_tier = result.subscription_tier;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "loadCodeAssist failed, continuing with existing project");
                    }
                }
            }
            Err(e) => {
                warn!(
                    email = %account.email,
                    error = %e,
                    "Failed to get access token for first account, will retry on request"
                );
            }
        }
    }

    if let Err(e) = accounts.save() {
        warn!(error = %e, "Failed to save updated accounts");
    }

    let cache_config = config::get_config().cache.clone();
    let cloudcode_config = config::get_config().cloudcode.clone();
    let state = Arc::new(ServerState {
        accounts: RwLock::new(accounts),
        http_client,
        cloudcode_client: CloudCodeClient::new(&cloudcode_config),
        cache: tokio::sync::Mutex::new(ResponseCache::new(
            cache_config.enabled,
            cache_config.ttl_seconds,
            cache_config.max_entries,
        )),
    });

    let refresh_state = state.clone();
    tokio::spawn(async move {
        background_token_refresh(refresh_state).await;
    });

    let addr: SocketAddr = format!("{}:{}", config.host(), config.port())
        .parse()
        .expect("Invalid address");

    info!(address = %addr, "Starting AGCP proxy server");
    if let Err(e) = run_server_with_shutdown(addr, state).await {
        error!(error = %e, "Server error");
        std::process::exit(1);
    }

    let _ = std::fs::remove_file(get_pid_path());
}

/// Background task that proactively refreshes tokens before they expire
async fn background_token_refresh(state: Arc<ServerState>) {
    use std::time::Duration;

    // Check tokens every 5 minutes
    let check_interval = Duration::from_secs(300);
    // Refresh when token expires in less than 10 minutes
    let refresh_threshold_secs = 600u64;

    loop {
        tokio::time::sleep(check_interval).await;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Check all accounts and refresh tokens that are about to expire
        let mut accounts = state.accounts.write().await;
        for account in accounts.accounts.iter_mut() {
            if !account.enabled || account.is_invalid {
                continue;
            }

            let should_refresh = if let Some(expires) = account.access_token_expires {
                expires.saturating_sub(now) < refresh_threshold_secs
            } else {
                true
            };

            if should_refresh {
                match account.get_access_token(&state.http_client).await {
                    Ok(_) => {
                        tracing::debug!(email = %account.email, "Background token refresh successful");
                    }
                    Err(e) => {
                        tracing::warn!(email = %account.email, error = %e, "Background token refresh failed");
                    }
                }
            }
        }

        // Also refill rate limit tokens for all accounts
        for account in accounts.accounts.iter_mut() {
            account.refill_tokens(5); // Add 5 tokens every 5 minutes
        }
    }
}

fn run_logs_command(args: &[String]) {
    let mut follow = true;
    let mut lines = 50usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--lines" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<usize>() {
                        Ok(n) if n > 0 => lines = n,
                        Ok(_) => {
                            eprintln!(
                                "\x1b[33mWarning:\x1b[0m --lines must be positive, using default (50)"
                            );
                        }
                        Err(_) => {
                            eprintln!(
                                "\x1b[33mWarning:\x1b[0m '{}' is not a valid number for --lines, using default (50)",
                                args[i]
                            );
                        }
                    }
                }
            }
            "--no-follow" => follow = false,
            _ => {}
        }
        i += 1;
    }

    let log_path = get_log_path();

    if !log_path.exists() {
        println!("\x1b[2mNo logs yet. Start the server with 'agcp'\x1b[0m");
        return;
    }

    // Print last N lines (read from end of file to avoid loading entire file)
    let mut file = File::open(&log_path).expect("Failed to open log file");
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let tail_lines = if file_len == 0 {
        Vec::new()
    } else {
        const CHUNK_SIZE: u64 = 64 * 1024;
        let mut collected: Vec<String> = Vec::new();
        let mut remaining = file_len;

        while remaining > 0 && collected.len() < lines + 1 {
            let chunk = remaining.min(CHUNK_SIZE);
            let offset = remaining - chunk;
            file.seek(SeekFrom::Start(offset)).expect("Failed to seek");
            let mut buf = vec![0u8; chunk as usize];
            if file.read_exact(&mut buf).is_err() {
                break;
            }
            let chunk_str = String::from_utf8_lossy(&buf);
            let mut chunk_lines: Vec<String> = chunk_str.lines().map(String::from).collect();
            if offset > 0 && !chunk_lines.is_empty() {
                let partial = chunk_lines.remove(0);
                if let Some(last) = collected.last_mut() {
                    *last = format!("{}{}", partial, last);
                }
            }
            chunk_lines.append(&mut collected);
            collected = chunk_lines;
            remaining = offset;
        }

        let start = collected.len().saturating_sub(lines);
        collected[start..].to_vec()
    };

    for line in &tail_lines {
        println!("{}", line);
    }

    if !follow {
        return;
    }

    // Follow mode
    println!("\x1b[2m--- Following logs (Ctrl+C to stop) ---\x1b[0m");

    let mut file = File::open(&log_path).expect("Failed to open log file");
    file.seek(SeekFrom::End(0)).expect("Failed to seek");

    let mut reader = BufReader::new(file);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                print!("{}", line);
            }
            Err(_) => break,
        }
    }
}

fn run_config_command() {
    println!();
    println!("{}{}AGCP Configuration{}", BOLD, GREEN, RESET);
    println!();

    let config_path = Config::path();
    println!("{}Config file:{}", BOLD, RESET);
    if config_path.exists() {
        println!("  {} {}", CYAN, config_path.display());
    } else {
        println!(
            "  {} {}{}(not created yet){}",
            config_path.display(),
            DIM,
            YELLOW,
            RESET
        );
    }
    println!();

    let config = Config::load().unwrap_or_default();

    println!("{}Current settings:{}", BOLD, RESET);
    println!();

    println!("  {}[server]{}", DIM, RESET);
    println!("    port = {}{}{}", CYAN, config.server.port, RESET);
    println!("    host = {}\"{}\"{}", CYAN, config.server.host, RESET);
    if config.server.api_key.is_some() {
        println!(
            "    api_key = {}\"****\"{} {}(set){}",
            CYAN, RESET, DIM, RESET
        );
    }
    println!();

    println!("  {}[logging]{}", DIM, RESET);
    println!("    debug = {}{}{}", CYAN, config.logging.debug, RESET);
    println!(
        "    log_requests = {}{}{}",
        CYAN, config.logging.log_requests, RESET
    );
    println!();

    println!("  {}[accounts]{}", DIM, RESET);
    println!(
        "    strategy = {}\"{}\"{}",
        CYAN, config.accounts.strategy, RESET
    );
    println!(
        "    quota_threshold = {}{}{}",
        CYAN, config.accounts.quota_threshold, RESET
    );
    println!(
        "    fallback = {}{}{}",
        CYAN, config.accounts.fallback, RESET
    );
    println!();

    println!("{}Environment variables:{}", BOLD, RESET);
    let api_key_set = std::env::var("API_KEY").is_ok();
    if api_key_set {
        println!("  {}API_KEY{} = {}(set){}", YELLOW, RESET, DIM, RESET);
    } else {
        println!("  {}(none set){}", DIM, RESET);
    }
    println!();

    println!("{}Other paths:{}", BOLD, RESET);
    println!(
        "  Accounts: {}",
        Config::dir().join("accounts.json").display()
    );
    println!("  Logs:     {}", get_log_path().display());
    println!();
}

fn run_stop_command() {
    if let Some(pid) = read_pid() {
        if is_process_running(pid) {
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            #[cfg(windows)]
            {
                use sysinfo::{Pid, System};
                let mut sys = System::new();
                sys.refresh_processes(
                    sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
                    true,
                );
                if let Some(process) = sys.process(Pid::from_u32(pid)) {
                    process.kill();
                }
            }

            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if !is_process_running(pid) {
                    break;
                }
            }

            if is_process_running(pid) {
                eprintln!(
                    "\x1b[33m●\x1b[0m AGCP is taking too long to stop (PID: {})",
                    pid
                );
            } else {
                let _ = std::fs::remove_file(get_pid_path());
                println!("\x1b[31m●\x1b[0m AGCP stopped");
            }
        } else {
            let _ = std::fs::remove_file(get_pid_path());
            println!("\x1b[2m●\x1b[0m AGCP is not running");
        }
    } else {
        println!("\x1b[2m●\x1b[0m AGCP is not running");
    }
}

async fn run_restart_command() {
    if let Some(pid) = read_pid()
        && is_process_running(pid)
    {
        println!("\x1b[33m●\x1b[0m Stopping AGCP (PID: {})...", pid);

        #[cfg(unix)]
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        #[cfg(windows)]
        {
            use sysinfo::{Pid, System};
            let mut sys = System::new();
            sys.refresh_processes(
                sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
                true,
            );
            if let Some(process) = sys.process(Pid::from_u32(pid)) {
                process.kill();
            }
        }

        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !is_process_running(pid) {
                break;
            }
        }

        if is_process_running(pid) {
            eprintln!("\x1b[31m●\x1b[0m Failed to stop AGCP, cannot restart");
            std::process::exit(1);
        }

        let _ = std::fs::remove_file(get_pid_path());
    }

    // Small delay to ensure port is released
    std::thread::sleep(std::time::Duration::from_millis(200));

    let config = Config::load().unwrap_or_default();
    run_daemon(config, false).await;
}

fn run_status_command() {
    if let Some(pid) = read_pid() {
        if is_process_running(pid) {
            let config = Config::load().unwrap_or_default();

            println!("{}●{} AGCP is running (PID: {})", GREEN, RESET, pid);
            print_listening_address(config.host(), config.port());

            // Try to fetch stats from running server
            let addr = format!("{}:{}", config.host(), config.port());
            if let Ok(stats) = fetch_stats_sync(&addr) {
                // Uptime
                if let Some(uptime_secs) = stats["uptime_seconds"].as_u64() {
                    println!("  Uptime: {}{}{}", CYAN, format_uptime(uptime_secs), RESET);
                }

                // Request count
                if let Some(total) = stats["total_requests"].as_u64()
                    && total > 0
                {
                    println!("  Requests: {}{}{} total", CYAN, total, RESET);
                }
            }

            if let Ok(store) = auth::accounts::AccountStore::load() {
                let enabled = store
                    .accounts
                    .iter()
                    .filter(|a| a.enabled && !a.is_invalid)
                    .count();
                let strategy = format!("{:?}", store.strategy).to_lowercase();
                println!(
                    "  Accounts: {}{}{} active ({} strategy)",
                    CYAN, enabled, RESET, strategy
                );

                // Show active account if in sticky mode
                if let Some(ref active_id) = store.active_account_id
                    && let Some(account) = store.accounts.iter().find(|a| &a.id == active_id)
                {
                    println!("  Current:  {}{}{}", DIM, account.email, RESET);
                }
            }

            println!();
            println!("  {}Use 'agcp logs' to view logs{}", DIM, RESET);
            println!("  {}Use 'agcp stop' to stop the server{}", DIM, RESET);
        } else {
            let _ = std::fs::remove_file(get_pid_path());
            println!("{}●{} AGCP is not running", DIM, RESET);
            println!();
            println!("  {}Start with 'agcp'{}", DIM, RESET);
        }
    } else {
        println!("{}●{} AGCP is not running", DIM, RESET);
        println!();
        println!("  {}Start with 'agcp'{}", DIM, RESET);
    }
}

/// Handle corrupted accounts.json file - offer to backup and reset
/// Returns true if user chose to reset, false if they cancelled
fn handle_corrupted_accounts_file(error: &dyn std::error::Error) -> bool {
    use auth::accounts::AccountStore;

    eprintln!("\x1b[31m●\x1b[0m Accounts file is corrupted");
    eprintln!();
    eprintln!("  \x1b[2mError: {}\x1b[0m", error);
    eprintln!();

    let accounts_path = AccountStore::path();
    let backup_path = accounts_path.with_extension("json.corrupted");

    eprintln!("  The accounts file at:");
    eprintln!("    \x1b[36m{}\x1b[0m", accounts_path.display());
    eprintln!();
    eprintln!("  appears to be invalid JSON. This can happen if:");
    eprintln!("    - The file was manually edited incorrectly");
    eprintln!("    - A write was interrupted (power loss, crash)");
    eprintln!("    - The file format changed between versions");
    eprintln!();

    eprint!("  Back up corrupted file and start fresh? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    let confirmed = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
    if !confirmed {
        eprintln!();
        eprintln!("  \x1b[2mCancelled. You can manually fix or delete the file.\x1b[0m");
        return false;
    }

    // Backup the corrupted file
    if let Err(e) = std::fs::rename(&accounts_path, &backup_path) {
        eprintln!();
        eprintln!("\x1b[31m●\x1b[0m Failed to backup corrupted file: {}", e);
        return false;
    }

    eprintln!();
    eprintln!("\x1b[32m●\x1b[0m Corrupted file backed up to:");
    eprintln!("    \x1b[36m{}\x1b[0m", backup_path.display());
    eprintln!();

    true
}

/// Synchronous version of fetch_stats_http for use in non-async context
fn fetch_stats_sync(addr: &str) -> Result<serde_json::Value, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("{}", e))?,
        Duration::from_secs(2),
    )
    .map_err(|e| e.to_string())?;

    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;

    let request = format!(
        "GET /stats HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| e.to_string())?;

    let response_str = String::from_utf8_lossy(&response);

    // Find the body (after \r\n\r\n)
    if let Some(body_start) = response_str.find("\r\n\r\n") {
        let body = &response_str[body_start + 4..];
        serde_json::from_str(body).map_err(|e| e.to_string())
    } else {
        Err("Invalid HTTP response".to_string())
    }
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(get_pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Try to acquire an exclusive lock on the lock file
/// Returns the lock file handle if successful (must be kept alive while running)
fn try_acquire_lock() -> Option<std::fs::File> {
    use fs2::FileExt;

    let lock_path = get_lock_path();
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .ok()?;

    // Try to acquire exclusive lock (non-blocking)
    if file.try_lock_exclusive().is_ok() {
        Some(file)
    } else {
        None
    }
}

#[cfg(unix)]
fn write_pid(pid: u32) {
    let pid_path = get_pid_path();
    if let Some(parent) = pid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(pid_path, pid.to_string());
}

/// Check if a process with the given PID is running.
/// Works on both Unix (via libc) and Windows (via sysinfo).
fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // On Unix, send signal 0 to check if process exists
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use sysinfo::{Pid, System};
        let mut sys = System::new();
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true,
        );
        sys.process(Pid::from_u32(pid)).is_some()
    }
    #[cfg(not(any(unix, windows)))]
    {
        // On other platforms, assume process is not running
        let _ = pid;
        false
    }
}

/// Check if a port is available for binding
fn is_port_available(host: &str, port: u16) -> bool {
    std::net::TcpListener::bind((host, port)).is_ok()
}

/// Get the local LAN IP address by connecting to an external address.
/// Returns None if unable to determine.
fn get_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    // Connect to a public DNS server (doesn't actually send data)
    // This makes the OS choose the appropriate local interface
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// Print the listening address, showing LAN IP when bound to all interfaces
fn print_listening_address(host: &str, port: u16) {
    if host == "0.0.0.0" {
        // Network mode - show the actual LAN IP
        if let Some(lan_ip) = get_local_ip() {
            println!("  Listening on {CYAN}http://{lan_ip}:{port}{RESET} {DIM}(network){RESET}");
            println!("  {DIM}Also available on http://127.0.0.1:{port}{RESET}");
        } else {
            println!(
                "  Listening on {CYAN}http://0.0.0.0:{port}{RESET} {DIM}(all interfaces){RESET}"
            );
        }
    } else {
        println!("  Listening on {CYAN}http://{host}:{port}{RESET}");
    }
}

/// Try to find which process is using a port (Linux only)
#[cfg(target_os = "linux")]
fn find_process_using_port(port: u16) -> Option<String> {
    use std::process::Command;
    // Try ss first (more modern), then fall back to lsof
    if let Ok(output) = Command::new("ss")
        .args(["-tlnp", &format!("sport = :{}", port)])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse ss output to find process name
        for line in stdout.lines().skip(1) {
            if let Some(users) = line.split("users:").nth(1) {
                // Format: users:(("process",pid=1234,fd=5))
                if let Some(start) = users.find("((\"")
                    && let Some(end) = users[start + 3..].find('"')
                {
                    return Some(users[start + 3..start + 3 + end].to_string());
                }
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn find_process_using_port(_port: u16) -> Option<String> {
    None
}

fn init_logging_foreground(debug: bool) {
    let filter = if debug {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agcp=debug,warn"))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agcp=info,warn"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();
}

async fn run_server_with_shutdown(
    addr: SocketAddr,
    state: Arc<ServerState>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(address = %addr, "Server listening");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("Received shutdown signal, stopping server");
                break;
            }
            result = listener.accept() => {
                let (stream, remote_addr) = result?;
                let state = state.clone();

                tokio::spawn(async move {
                    if let Err(e) = server::handle_connection(stream, remote_addr, state).await {
                        warn!(error = %e, remote = %remote_addr, "Connection error");
                    }
                });
            }
        }
    }

    info!("Server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn print_help() {
    // Lolcat-style rainbow gradient for logo (smooth color transition)
    fn rainbow_char(c: char, pos: usize) -> String {
        if c == ' ' {
            return " ".to_string();
        }
        // 6 colors cycling: red → orange → yellow → green → cyan → blue → magenta
        let colors = [
            "\x1b[38;5;196m", // red
            "\x1b[38;5;208m", // orange
            "\x1b[38;5;226m", // yellow
            "\x1b[38;5;46m",  // green
            "\x1b[38;5;51m",  // cyan
            "\x1b[38;5;21m",  // blue
            "\x1b[38;5;201m", // magenta
        ];
        let color = colors[pos % colors.len()];
        format!("{}{}", color, c)
    }

    fn rainbow_line(line: &str, offset: usize) -> String {
        line.chars()
            .enumerate()
            .map(|(i, c)| rainbow_char(c, i + offset))
            .collect::<String>()
            + RESET
    }

    let logo_lines = [
        " ▗▄▖  ▗▄▄▖ ▗▄▄▖▗▄▄▖ ",
        "▐▌ ▐▌▐▌   ▐▌   ▐▌ ▐▌",
        "▐▛▀▜▌▐▌▝▜▌▐▌   ▐▛▀▘ ",
        "▐▌ ▐▌▝▚▄▞▘▝▚▄▄▖▐▌   ",
    ];

    println!();
    for (i, line) in logo_lines.iter().enumerate() {
        println!("{}{}", BOLD, rainbow_line(line, i * 2));
    }

    println!(
        r#"
{DIM}Anthropic → Google Cloud Code Proxy{RESET}

{BOLD}USAGE:{RESET}  {GREEN}agcp{RESET} [COMMAND] [OPTIONS]

{BOLD}COMMANDS{RESET}
┌─────────────┬────────────────────────────────────────┐
│ {YELLOW}login{RESET}       │ Authenticate with Google OAuth         │
│ {YELLOW}setup{RESET}       │ Configure AI tools to use AGCP         │
│ {YELLOW}accounts{RESET}    │ Manage multiple accounts               │
│ {YELLOW}config{RESET}      │ Show current configuration             │
│ {YELLOW}doctor{RESET}      │ Check configuration and connectivity   │
│ {YELLOW}test{RESET}        │ Send a test request to verify setup    │
│ {YELLOW}quota{RESET}       │ Show model quota usage                 │
│ {YELLOW}stats{RESET}       │ Show request/response statistics       │
│ {YELLOW}logs{RESET}        │ View server logs (follows by default)  │
│ {YELLOW}stop{RESET}        │ Stop the background server             │
│ {YELLOW}restart{RESET}     │ Restart the background server          │
│ {YELLOW}status{RESET}      │ Check if server is running             │
│ {YELLOW}upgrade{RESET}     │ Check for and install updates          │
│ {YELLOW}completions{RESET} │ Generate shell completions             │
│ {YELLOW}tui{RESET}         │ Launch interactive terminal UI         │
│ {YELLOW}version{RESET}     │ Show version information               │
│ {YELLOW}help{RESET}        │ Show this help message                 │
└─────────────┴────────────────────────────────────────┘

{BOLD}OPTIONS{RESET}
┌──────────────────────┬───────────────────────────────────────┐
│ {YELLOW}-p{RESET}, {YELLOW}--port{RESET} <PORT>    │ Server port {DIM}(default: 8080){RESET}           │
│ {YELLOW}--host{RESET} <HOST>        │ Bind address {DIM}(default: 127.0.0.1){RESET}     │
│ {YELLOW}--network{RESET}            │ Listen on all interfaces (LAN access) │
│ {YELLOW}-f{RESET}, {YELLOW}--foreground{RESET}     │ Run in foreground (don't daemonize)   │
│ {YELLOW}-d{RESET}, {YELLOW}--debug{RESET}          │ Enable debug logging                  │
│ {YELLOW}--fallback{RESET}           │ Enable model fallback on exhaustion   │
│ {YELLOW}-h{RESET}, {YELLOW}--help{RESET}           │ Show this help message                │
│ {YELLOW}-V{RESET}, {YELLOW}--version{RESET}        │ Show version information              │
├──────────────────────┼───────────────────────────────────────┤
│ {YELLOW}-n{RESET}, {YELLOW}--lines{RESET} <N>      │ {DIM}logs:{RESET} Show last N lines {DIM}(default: 50){RESET} │
│ {YELLOW}--no-follow{RESET}          │ {DIM}logs:{RESET} Don't follow log output         │
└──────────────────────┴───────────────────────────────────────┘

{BOLD}MODEL ALIASES{RESET}
┌─────────────────┬────────────────────────────┐
│ {YELLOW}opus{RESET}            │ claude-opus-4-6-thinking   │
│ {YELLOW}opus-4-5{RESET}        │ claude-opus-4-5-thinking   │
│ {YELLOW}sonnet{RESET}          │ claude-sonnet-4-5          │
│ {YELLOW}sonnet-thinking{RESET} │ claude-sonnet-4-5-thinking │
│ {YELLOW}flash{RESET}           │ gemini-3-flash             │
│ {YELLOW}pro{RESET}             │ gemini-3-pro-high          │
│ {YELLOW}3-flash{RESET}         │ gemini-3-flash             │
│ {YELLOW}3-pro{RESET}           │ gemini-3-pro-high          │
│ {YELLOW}oss{RESET}             │ gpt-oss-120b-medium        │
└─────────────────┴────────────────────────────┘

{BOLD}EXAMPLES{RESET}
  {GREEN}agcp login{RESET}                    {DIM}# First-time setup{RESET}
  {GREEN}agcp login --no-browser{RESET}       {DIM}# Headless server (manual code){RESET}
  {GREEN}agcp setup{RESET}                    {DIM}# Configure AI tools to use AGCP{RESET}
  {GREEN}agcp{RESET}                          {DIM}# Start proxy as daemon{RESET}
  {GREEN}agcp --port 3000{RESET}              {DIM}# Start on custom port{RESET}
  {GREEN}agcp --fallback{RESET}               {DIM}# Enable model fallback{RESET}
  {GREEN}agcp logs{RESET}                     {DIM}# View logs{RESET}
  {GREEN}agcp logs -n 100 --no-follow{RESET}  {DIM}# Last 100 lines, no follow{RESET}
  {GREEN}agcp -f -d{RESET}                    {DIM}# Foreground with debug{RESET}

{DIM}Config: ~/.config/agcp/config.toml
Logs:   ~/.config/agcp/agcp.log{RESET}
"#
    );
}

/// Extract authorization code from input (either full URL or just the code).
/// Validates state if present in the URL.
fn extract_code_from_input(input: &str, expected_state: &str) -> error::Result<String> {
    let input = input.trim();

    // Check if it's a full URL
    if input.starts_with("http://") || input.starts_with("https://") {
        // Parse URL and extract code parameter
        if let Some(query_start) = input.find('?') {
            let query = &input[query_start + 1..];
            let params: std::collections::HashMap<String, String> = query
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    match (parts.next(), parts.next()) {
                        (Some(k), Some(v)) => Some((k.to_string(), percent_decode_simple(v))),
                        _ => None,
                    }
                })
                .collect();

            // Check for error
            if let Some(error) = params.get("error") {
                return Err(error::Error::Auth(error::AuthError::OAuthFailed(format!(
                    "OAuth error: {}",
                    error
                ))));
            }

            // Validate state if present
            if let Some(state) = params.get("state")
                && state != expected_state
            {
                eprintln!(
                    "\n\x1b[33m⚠ State mismatch detected. This could indicate a security issue.\x1b[0m"
                );
                eprintln!("\x1b[33mProceeding anyway as this is manual mode...\x1b[0m\n");
            }

            // Get the code
            if let Some(code) = params.get("code") {
                return Ok(code.clone());
            }

            return Err(error::Error::Auth(error::AuthError::OAuthFailed(
                "No authorization code found in URL".to_string(),
            )));
        }

        return Err(error::Error::Auth(error::AuthError::OAuthFailed(
            "Invalid callback URL - no query parameters".to_string(),
        )));
    }

    // Assume it's just the code
    if input.is_empty() {
        return Err(error::Error::Auth(error::AuthError::OAuthFailed(
            "No input provided".to_string(),
        )));
    }

    Ok(input.to_string())
}

/// Simple percent decoding for URL parameters
fn percent_decode_simple(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2)
                && let Ok(byte) = u8::from_str_radix(&format!("{}{}", h1, h2), 16)
            {
                result.push(byte as char);
                continue;
            }
        } else if c == '+' {
            result.push(' ');
            continue;
        }
        result.push(c);
    }
    result
}

async fn run_login(no_browser: bool) -> error::Result<()> {
    use auth::{
        CALLBACK_PORT, exchange_code, get_authorization_url, get_user_email, start_callback_server,
    };

    let redirect_uri = format!("http://localhost:{}/oauth-callback", CALLBACK_PORT);
    let (auth_url, pkce, state) = get_authorization_url(&redirect_uri);

    info!("Starting OAuth login flow");

    let code = if no_browser {
        // Headless mode - user manually pastes the callback URL or code
        println!(
            "\n\x1b[33m📋 No-browser mode: You will manually paste the authorization code.\x1b[0m\n"
        );
        println!("\x1b[1mStep 1:\x1b[0m Copy this URL and open it in a browser:\n");
        println!("  \x1b[36m{}\x1b[0m\n", auth_url);
        println!("\x1b[1mStep 2:\x1b[0m Sign in with your Google account.\n");
        println!("\x1b[1mStep 3:\x1b[0m After signing in, your browser will try to redirect to:");
        println!(
            "  \x1b[2mhttp://localhost:{}/oauth-callback?code=XXXX&state=YYYY\x1b[0m\n",
            auth::CALLBACK_PORT
        );
        println!("  The page won't load (that's expected on a headless server).");
        println!("  Copy the \x1b[1mfull URL\x1b[0m from your browser's address bar.\n");

        // Read input from stdin
        print!("Paste the redirect URL here: ");
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();

        // Extract code from input (could be full URL or just the code)
        extract_code_from_input(input, &state)?
    } else {
        // Normal mode - open browser and wait for callback
        println!("Opening browser for authentication...");
        println!();
        println!("If the browser doesn't open, visit this URL:");
        println!("{}", auth_url);
        println!();

        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(&auth_url).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-open")
                .arg(&auth_url)
                .spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", &auth_url])
                .spawn();
        }

        let (actual_port, rx) = start_callback_server(state).await?;

        if actual_port != CALLBACK_PORT {
            warn!(port = actual_port, "Using alternate callback port");
        }

        let spinner = Spinner::new("Waiting for authorization...");
        match rx.await {
            Ok(Ok(code)) => {
                spinner.stop();
                code
            }
            Ok(Err(e)) => {
                spinner.stop();
                return Err(error::Error::Auth(error::AuthError::OAuthFailed(e)));
            }
            Err(_) => {
                spinner.stop();
                return Err(error::Error::Auth(error::AuthError::OAuthFailed(
                    "Callback cancelled".to_string(),
                )));
            }
        }
    };

    let spinner = Spinner::new("Exchanging tokens...");
    let http_client = HttpClient::new();
    let (access_token, refresh_token, expires_in) =
        exchange_code(&http_client, &code, &pkce.verifier, &redirect_uri).await?;
    let email = get_user_email(&http_client, &access_token).await?;
    spinner.stop();

    println!("\x1b[32m✓\x1b[0m Logged in as: {}", email);

    let spinner = Spinner::new("Discovering project and subscription...");
    let (project_id, subscription_tier) =
        match cloudcode::discover_project_and_tier(&http_client, &access_token, None).await {
            Ok(result) => {
                spinner.stop();
                if let Some(ref id) = result.project_id {
                    println!("\x1b[32m✓\x1b[0m Project ID: {}", id);
                }
                if let Some(ref tier) = result.subscription_tier {
                    let tier_badge = match tier.as_str() {
                        "ultra" => "\x1b[35mUltra\x1b[0m",
                        "pro" => "\x1b[36mPro\x1b[0m",
                        _ => "\x1b[33mFree\x1b[0m",
                    };
                    println!("\x1b[32m✓\x1b[0m Subscription: {}", tier_badge);
                }
                (result.project_id, result.subscription_tier)
            }
            Err(e) => {
                spinner.stop();
                warn!(error = %e, "Failed to discover project ID, using default");
                (Some("rising-fact-p41fc".to_string()), None)
            }
        };

    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut account = Account::new(email, refresh_token);
    account.project_id = project_id;
    account.subscription_tier = subscription_tier;
    account.access_token = Some(access_token);
    account.access_token_expires = Some(now + expires_in);

    account.save()?;

    println!("Account saved to ~/.config/agcp/account.json");
    println!();
    println!("You can now start the proxy with: agcp");

    Ok(())
}

async fn run_quota_command() -> error::Result<()> {
    // Start spinner immediately
    let spinner = Spinner::new("Fetching quotas...");

    let result = async {
        let mut account = match Account::load() {
            Ok(Some(acc)) => acc,
            Ok(None) => {
                return Err(error::Error::Auth(error::AuthError::OAuthFailed(
                    "No account configured. Run 'agcp login' to authenticate.".to_string(),
                )));
            }
            Err(e) => {
                return Err(error::Error::Auth(error::AuthError::OAuthFailed(format!(
                    "Failed to load account: {}",
                    e
                ))));
            }
        };

        let http_client = auth::HttpClient::new();
        let access_token = account
            .get_access_token(&http_client)
            .await
            .map_err(|e| error::Error::Auth(error::AuthError::RefreshFailed(e.to_string())))?;

        cloudcode::fetch_model_quotas(&http_client, &access_token, account.project_id.as_deref())
            .await
            .map_err(|e| error::Error::Api(error::ApiError::InvalidRequest { message: e }))
    }
    .await;

    spinner.stop();

    let quotas = result?;
    cloudcode::render_quota_display(&quotas);

    Ok(())
}

async fn run_upgrade_command() {
    let current_version = env!("CARGO_PKG_VERSION");
    let repo = env!("CARGO_PKG_REPOSITORY");

    println!();
    print!("Checking for updates... ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    // Extract owner/repo from repository URL
    let repo_path = repo
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")
        .unwrap_or("skyline69/agcp");

    // Fetch latest release from GitHub API
    let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo_path);

    // Show spinner while fetching
    let api_url_clone = api_url.clone();
    let fetch_future = tokio::spawn(async move { fetch_latest_version(&api_url_clone).await });

    let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut i = 0;
    let mut fetch_future = std::pin::pin!(fetch_future);

    let result = loop {
        tokio::select! {
            biased;
            res = &mut fetch_future => {
                print!("\r                              \r");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                break res.unwrap_or_else(|e| Err(format!("Task failed: {}", e)));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                print!("\r{} Checking for updates... ", spinner[i % spinner.len()]);
                std::io::Write::flush(&mut std::io::stdout()).ok();
                i += 1;
            }
        }
    };

    let latest_version = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}Failed to check for updates:{} {}", RED, RESET, e);
            eprintln!();
            eprintln!("You can check manually at:");
            eprintln!("  {}{}/releases{}", CYAN, repo, RESET);
            std::process::exit(1);
        }
    };

    // Compare versions (strip 'v' prefix if present)
    let latest_clean = latest_version.strip_prefix('v').unwrap_or(&latest_version);
    let current_clean = current_version.strip_prefix('v').unwrap_or(current_version);

    println!("  {}Current:{} v{}", DIM, RESET, current_clean);
    println!("  {}Latest:{} v{}", DIM, RESET, latest_clean);
    println!();

    if current_clean == latest_clean {
        println!("{}✓ Already up to date!{}", GREEN, RESET);
        println!();
        return;
    }

    // Simple version comparison (works for semver)
    let is_newer = compare_versions(latest_clean, current_clean);

    if !is_newer {
        println!(
            "{}✓ You're running a newer version than the latest release.{}",
            GREEN, RESET
        );
        println!();
        return;
    }

    println!(
        "{}Update available!{} v{} → v{}",
        YELLOW, RESET, current_clean, latest_clean
    );
    println!();
    println!("{}To upgrade:{}", BOLD, RESET);
    println!();

    // Check if cargo is available
    let has_cargo = std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_cargo {
        println!("  {}# Via cargo (recommended){}", DIM, RESET);
        println!("  {}cargo install agcp --force{}", CYAN, RESET);
        println!();
    }

    println!("  {}# From source{}", DIM, RESET);
    println!("  {}git pull && cargo build --release{}", CYAN, RESET);
    println!();
    println!("  {}# Or download from:{}", DIM, RESET);
    println!("  {}{}/releases/latest{}", CYAN, repo, RESET);
    println!();
}

async fn fetch_latest_version(api_url: &str) -> Result<String, String> {
    let client = auth::HttpClient::new();

    let headers = [
        ("Accept", "application/vnd.github.v3+json"),
        ("User-Agent", "agcp"),
    ];

    let body = client.get(api_url, &headers).await?;
    let body = String::from_utf8_lossy(&body);

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(tag) = json["tag_name"].as_str() {
            return Ok(tag.to_string());
        }
        if let Some(msg) = json["message"].as_str() {
            return Err(msg.to_string());
        }
    }

    Err("Could not parse GitHub API response".to_string())
}

fn compare_versions(a: &str, b: &str) -> bool {
    // Returns true if a > b (a is newer than b)
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };

    let va = parse(a);
    let vb = parse(b);

    for i in 0..va.len().max(vb.len()) {
        let pa = va.get(i).copied().unwrap_or(0);
        let pb = vb.get(i).copied().unwrap_or(0);
        if pa > pb {
            return true;
        }
        if pa < pb {
            return false;
        }
    }
    false
}

async fn run_test_command() {
    println!();
    println!("{}{}Testing AGCP...{}", BOLD, CYAN, RESET);
    println!();

    let config = Config::load().unwrap_or_default();
    let addr = format!("{}:{}", config.host(), config.port());
    let base_url = format!("http://{}", addr);

    // Step 1: Check if server is running
    print!("  Server:  ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let server_ok = addr
        .parse()
        .ok()
        .and_then(|socket_addr| {
            std::net::TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(2))
                .ok()
        })
        .is_some();

    if !server_ok {
        println!("{}{} ✗{}", base_url, RED, RESET);
        println!();
        println!("  {}Server is not running.{}", DIM, RESET);
        println!("  Start it with: {}agcp{}", GREEN, RESET);
        println!();
        std::process::exit(1);
    }
    println!("{} {}✓{}", base_url, GREEN, RESET);

    // Step 2: Show account info
    if let Ok(store) = auth::accounts::AccountStore::load()
        && let Some(account) = store.accounts.first()
    {
        println!("  Account: {}{}{}", CYAN, account.email, RESET);
    }

    // Step 3: Test models endpoint (fast, local)
    print!("  Models:  ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    match test_models_endpoint(&base_url).await {
        Ok(count) => {
            println!("{} models available {}✓{}", count, GREEN, RESET);
        }
        Err(e) => {
            println!("{}✗{}", RED, RESET);
            eprintln!("  {}Error: {}{}", RED, e, RESET);
            std::process::exit(1);
        }
    }

    println!();
    println!("{}● Setup verified!{}", GREEN, RESET);
    println!();
    println!(
        "  {}Tip: The server is ready. API requests will be made when you use your AI tool.{}",
        DIM, RESET
    );
    println!();
}

async fn test_models_endpoint(base_url: &str) -> Result<usize, String> {
    use http_body_util::{BodyExt, Empty};
    use hyper::Request;
    use hyper::body::Bytes;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let url = format!("{}/v1/models", base_url);

    // Use plain HTTP client for localhost
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();

    let req = Request::builder()
        .method("GET")
        .uri(&url)
        .body(Empty::new())
        .map_err(|e| e.to_string())?;

    let response = client.request(req).await.map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?;

    let bytes = body.to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
        && let Some(data) = json["data"].as_array()
    {
        return Ok(data.len());
    }

    Err("Invalid response".to_string())
}

async fn run_doctor_command() {
    println!();
    println!("{}{}AGCP Doctor{}", BOLD, GREEN, RESET);
    println!("{}Running diagnostics...{}", DIM, RESET);
    println!();

    let mut all_ok = true;

    // Check 1: Config file
    let config_path = Config::path();
    if config_path.exists() {
        println!(
            "{}✓{} Config file exists: {}",
            GREEN,
            RESET,
            config_path.display()
        );
    } else {
        println!(
            "{}○{} Config file not found {}{}{}",
            DIM,
            RESET,
            DIM,
            config_path.display(),
            RESET
        );
    }

    // Check 2: Account file
    match Account::load() {
        Ok(Some(account)) => {
            println!("{}✓{} Account configured: {}", GREEN, RESET, account.email);

            // Check 3: Access token
            let spinner = Spinner::new("Checking access token...");
            let http_client = HttpClient::new();
            let mut account = account;
            match account.get_access_token(&http_client).await {
                Ok(token) => {
                    spinner.stop();
                    println!("{}✓{} Access token valid", GREEN, RESET);

                    // Check 4: Project ID
                    if let Some(ref project_id) = account.project_id {
                        println!("{}✓{} Project ID: {}", GREEN, RESET, project_id);
                    } else {
                        println!("{}!{} No project ID configured", YELLOW, RESET);
                        all_ok = false;
                    }

                    // Check 5: API connectivity
                    let spinner = Spinner::new("Testing API connectivity...");
                    match cloudcode::fetch_model_quotas(
                        &http_client,
                        &token,
                        account.project_id.as_deref(),
                    )
                    .await
                    {
                        Ok(quotas) => {
                            spinner.stop();
                            println!(
                                "{}✓{} API connectivity OK ({} models available)",
                                GREEN,
                                RESET,
                                quotas.len()
                            );

                            // Check quota status
                            let low_quota: Vec<_> = quotas
                                .iter()
                                .filter(|q| q.remaining_fraction < 0.2)
                                .collect();
                            if low_quota.is_empty() {
                                println!("{}✓{} All model quotas healthy", GREEN, RESET);
                            } else {
                                for q in low_quota {
                                    let pct = (q.remaining_fraction * 100.0).round() as u32;
                                    println!(
                                        "{}!{} Low quota: {} ({}% remaining)",
                                        YELLOW, RESET, q.model_id, pct
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            spinner.stop();
                            println!("{}✗{} API connectivity failed: {}", RED, RESET, e);
                            all_ok = false;
                        }
                    }
                }
                Err(e) => {
                    spinner.stop();
                    println!("{}✗{} Access token refresh failed: {}", RED, RESET, e);
                    println!(
                        "  {}Try running 'agcp login' to re-authenticate{}",
                        DIM, RESET
                    );
                    all_ok = false;
                }
            }
        }
        Ok(None) => {
            println!("{}✗{} No account configured", RED, RESET);
            println!("  {}Run 'agcp login' to authenticate{}", DIM, RESET);
            all_ok = false;
        }
        Err(e) => {
            println!("{}✗{} Failed to load account: {}", RED, RESET, e);
            all_ok = false;
        }
    }

    // Check 6: Server status
    if let Some(pid) = read_pid() {
        if is_process_running(pid) {
            let config = Config::load().unwrap_or_default();
            println!(
                "{}✓{} Server running (PID: {}, port: {})",
                GREEN,
                RESET,
                pid,
                config.port()
            );
        } else {
            println!("{}○{} Server not running", DIM, RESET);
        }
    } else {
        println!("{}○{} Server not running", DIM, RESET);
    }

    println!();
    if all_ok {
        println!("{}{}All checks passed!{}", BOLD, GREEN, RESET);
    } else {
        println!(
            "{}{}Some issues found. See above for details.{}",
            BOLD, YELLOW, RESET
        );
    }
    println!();
}

async fn run_stats_command() {
    // Check if server is running
    let config = Config::load().unwrap_or_default();
    let addr = format!("{}:{}", config.host(), config.port());

    println!();
    println!("{}{}AGCP Stats{}", BOLD, GREEN, RESET);
    println!();

    // Try to fetch stats from running server using simple HTTP
    match fetch_stats_http(&addr).await {
        Ok(stats) => {
            // Stats are nested under "requests" key
            let requests = &stats["requests"];

            // Display uptime
            let uptime_secs = requests["uptime_seconds"].as_u64().unwrap_or(0);
            println!("{}Uptime:{} {}", BOLD, RESET, format_uptime(uptime_secs));

            // Display request counts
            let total = requests["total_requests"].as_u64().unwrap_or(0);
            println!("{}Requests:{} {}", BOLD, RESET, total);

            // Display per-model stats
            if let Some(models) = requests["models"].as_array()
                && !models.is_empty()
            {
                println!();
                println!("{}By Model:{}", BOLD, RESET);
                for model in models {
                    let name = model["model"].as_str().unwrap_or("unknown");
                    let reqs = model["requests"].as_u64().unwrap_or(0);
                    println!("  {}: {} reqs", name, reqs);
                }
            }

            // Display per-endpoint stats
            if let Some(endpoints) = requests["endpoints"].as_array()
                && !endpoints.is_empty()
            {
                println!();
                println!("{}By Endpoint:{}", BOLD, RESET);
                for endpoint in endpoints {
                    let path = endpoint["endpoint"].as_str().unwrap_or("unknown");
                    let reqs = endpoint["requests"].as_u64().unwrap_or(0);
                    println!("  {}: {} reqs", path, reqs);
                }
            }
        }
        Err(_) => {
            println!("{}○{} Server not running", DIM, RESET);
            println!();
            println!(
                "{}Start the server with '{}agcp{}' to collect stats.{}",
                DIM, YELLOW, DIM, RESET
            );
        }
    }
    println!();
}

async fn fetch_stats_http(addr: &str) -> Result<serde_json::Value, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;

    let request = format!(
        "GET /stats HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| e.to_string())?;

    let response_str = String::from_utf8_lossy(&response);

    // Find the body (after \r\n\r\n)
    if let Some(body_start) = response_str.find("\r\n\r\n") {
        let body = &response_str[body_start + 4..];
        serde_json::from_str(body).map_err(|e| e.to_string())
    } else {
        Err("Invalid HTTP response".to_string())
    }
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

async fn run_accounts_command(args: &[String]) {
    use auth::HttpClient;
    use auth::accounts::{AccountStore, SelectionStrategy};

    fn load_store_or_exit() -> AccountStore {
        match AccountStore::load() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}Failed to load accounts: {}{}", RED, e, RESET);
                std::process::exit(1);
            }
        }
    }

    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("list");

    match subcommand {
        "list" | "ls" => {
            let mut store = match AccountStore::load() {
                Ok(s) => s,
                Err(e) => {
                    let err_str = e.to_string().to_lowercase();
                    if err_str.contains("json")
                        || err_str.contains("parse")
                        || err_str.contains("expected")
                        || err_str.contains("missing field")
                    {
                        if handle_corrupted_accounts_file(&e) {
                            // Try again after reset
                            AccountStore::load().unwrap_or_default()
                        } else {
                            std::process::exit(1);
                        }
                    } else {
                        eprintln!("{}Failed to load accounts: {}{}", RED, e, RESET);
                        std::process::exit(1);
                    }
                }
            };

            if store.accounts.is_empty() {
                println!();
                println!("{}No accounts configured.{}", DIM, RESET);
                println!("Run '{}agcp login{}' to add an account.", GREEN, RESET);
                println!();
                return;
            }

            // Refresh subscription tiers from API
            let http_client = HttpClient::new();
            store.refresh_subscription_tiers(&http_client).await;

            println!();
            println!(
                "{}{}Accounts{} (strategy: {:?})",
                BOLD, GREEN, RESET, store.strategy
            );
            println!();

            for account in &store.accounts {
                let status = if !account.enabled {
                    format!("{}disabled{}", DIM, RESET)
                } else if account.is_invalid {
                    format!("{}invalid{}", RED, RESET)
                } else {
                    format!("{}active{}", GREEN, RESET)
                };

                let active_marker = if store.active_account_id.as_ref() == Some(&account.id) {
                    format!(" {}*{}", YELLOW, RESET)
                } else {
                    String::new()
                };

                println!(
                    "  {}[{}]{} {} {}{}",
                    DIM,
                    &account.id[..8],
                    RESET,
                    account.email,
                    status,
                    active_marker
                );

                if let Some(tier) = &account.subscription_tier {
                    let tier_badge = match tier.as_str() {
                        "ultra" => format!("\x1b[35m{}\x1b[0m", tier),
                        "pro" => format!("\x1b[36m{}\x1b[0m", tier),
                        _ => format!("\x1b[33m{}\x1b[0m", tier),
                    };
                    println!("      {}tier: {}", DIM, tier_badge);
                }
                if account.health_score < 1.0 {
                    println!(
                        "      {}health: {:.0}%{}",
                        DIM,
                        account.health_score * 100.0,
                        RESET
                    );
                }
            }

            // Show legend if there's an active account marker
            if store.active_account_id.is_some() {
                println!("  {}* = active account (sticky mode){}", DIM, RESET);
            }
            println!();
            println!(
                "  {}Tip: Run 'agcp accounts help' for more commands{}",
                DIM, RESET
            );
            println!();
        }

        "add" => {
            println!("{}Use 'agcp login' to add a new account.{}", DIM, RESET);
        }

        "remove" | "rm" => {
            let id = match args.get(1) {
                Some(id) => id,
                None => {
                    eprintln!("{}Usage: agcp accounts remove <id>{}", RED, RESET);
                    eprintln!("{}Get account IDs with 'agcp accounts list'{}", DIM, RESET);
                    std::process::exit(1);
                }
            };

            let mut store = load_store_or_exit();

            // Find account by ID prefix
            let matching: Vec<_> = store
                .accounts
                .iter()
                .filter(|a| a.id.starts_with(id))
                .collect();

            if matching.is_empty() {
                eprintln!(
                    "{}No account found with ID starting with '{}'{}",
                    RED, id, RESET
                );
                std::process::exit(1);
            } else if matching.len() > 1 {
                eprintln!(
                    "{}Multiple accounts match '{}', please be more specific:{}",
                    RED, id, RESET
                );
                for a in matching {
                    eprintln!("  {} - {}", &a.id[..8], a.email);
                }
                std::process::exit(1);
            }

            let full_id = matching[0].id.clone();
            let email = matching[0].email.clone();

            // Check for --force flag
            let force = args.iter().any(|a| a == "--force" || a == "-f");

            if !force {
                // Ask for confirmation
                eprintln!("About to remove account: \x1b[33m{}\x1b[0m", email);
                eprint!("Are you sure? [y/N] ");
                let _ = std::io::Write::flush(&mut std::io::stderr());

                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    std::process::exit(1);
                }

                let confirmed = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !confirmed {
                    println!("{}Cancelled{}", DIM, RESET);
                    return;
                }
            }

            if store.remove_account(&full_id) {
                if let Err(e) = store.save() {
                    eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
                    std::process::exit(1);
                }
                println!("{}Removed account: {}{}", GREEN, email, RESET);
            }
        }

        "enable" => {
            let id = match args.get(1) {
                Some(id) => id,
                None => {
                    eprintln!("{}Usage: agcp accounts enable <id>{}", RED, RESET);
                    std::process::exit(1);
                }
            };

            let mut store = load_store_or_exit();

            if let Some(account) = store.accounts.iter_mut().find(|a| a.id.starts_with(id)) {
                account.enabled = true;
                account.is_invalid = false;
                account.invalid_reason = None;
                let email = account.email.clone();
                if let Err(e) = store.save() {
                    eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
                    std::process::exit(1);
                }
                println!("{}Enabled account: {}{}", GREEN, email, RESET);
            } else {
                eprintln!(
                    "{}No account found with ID starting with '{}'{}",
                    RED, id, RESET
                );
                std::process::exit(1);
            }
        }

        "disable" => {
            let id = match args.get(1) {
                Some(id) => id,
                None => {
                    eprintln!("{}Usage: agcp accounts disable <id>{}", RED, RESET);
                    std::process::exit(1);
                }
            };

            let mut store = load_store_or_exit();

            if let Some(account) = store.accounts.iter_mut().find(|a| a.id.starts_with(id)) {
                account.enabled = false;
                let email = account.email.clone();
                if let Err(e) = store.save() {
                    eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
                    std::process::exit(1);
                }
                println!("{}Disabled account: {}{}", YELLOW, email, RESET);
            } else {
                eprintln!(
                    "{}No account found with ID starting with '{}'{}",
                    RED, id, RESET
                );
                std::process::exit(1);
            }
        }

        "switch" => {
            let id = match args.get(1) {
                Some(id) => id,
                None => {
                    eprintln!("{}Usage: agcp accounts switch <id>{}", RED, RESET);
                    std::process::exit(1);
                }
            };

            let mut store = load_store_or_exit();

            if let Some(account) = store.accounts.iter().find(|a| a.id.starts_with(id)) {
                let full_id = account.id.clone();
                let email = account.email.clone();
                store.set_active_account(&full_id);
                if let Err(e) = store.save() {
                    eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
                    std::process::exit(1);
                }
                println!("{}Switched to account: {}{}", GREEN, email, RESET);
            } else {
                eprintln!(
                    "{}No account found with ID starting with '{}'{}",
                    RED, id, RESET
                );
                std::process::exit(1);
            }
        }

        "strategy" => {
            let strategy_str = match args.get(1) {
                Some(s) => s,
                None => {
                    eprintln!(
                        "{}Usage: agcp accounts strategy <sticky|roundrobin|hybrid>{}",
                        RED, RESET
                    );
                    println!();
                    println!("{}Strategies:{}", BOLD, RESET);
                    println!(
                        "  {}sticky{}     - Stay on current account until rate-limited > 2 min",
                        YELLOW, RESET
                    );
                    println!(
                        "  {}roundrobin{} - Rotate accounts each request",
                        YELLOW, RESET
                    );
                    println!(
                        "  {}hybrid{}     - Smart selection based on health/quota/freshness",
                        YELLOW, RESET
                    );
                    std::process::exit(1);
                }
            };

            let strategy = match strategy_str.to_lowercase().as_str() {
                "sticky" => SelectionStrategy::Sticky,
                "roundrobin" | "round-robin" | "rr" => SelectionStrategy::RoundRobin,
                "hybrid" | "smart" => SelectionStrategy::Hybrid,
                _ => {
                    eprintln!("{}Unknown strategy: {}{}", RED, strategy_str, RESET);
                    eprintln!("{}Valid options: sticky, roundrobin, hybrid{}", DIM, RESET);
                    std::process::exit(1);
                }
            };

            let mut store = load_store_or_exit();

            store.strategy = strategy;
            if let Err(e) = store.save() {
                eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
                std::process::exit(1);
            }
            println!("{}Strategy set to: {:?}{}", GREEN, strategy, RESET);
        }

        "verify" => {
            let http_client = HttpClient::new();

            let mut store = load_store_or_exit();

            if store.accounts.is_empty() {
                println!();
                println!("{}No accounts to verify.{}", DIM, RESET);
                println!("Run '{}agcp login{}' to add an account.", GREEN, RESET);
                println!();
                return;
            }

            println!();
            println!("{}Verifying accounts...{}", BOLD, RESET);
            println!();

            let mut all_ok = true;
            for account in &mut store.accounts {
                match account.get_access_token(&http_client).await {
                    Ok(_) => {
                        println!("  {}✓{} {} - OK", GREEN, RESET, account.email);
                        // Clear any previous invalid state
                        if account.is_invalid {
                            account.is_invalid = false;
                            account.invalid_reason = None;
                        }
                    }
                    Err(e) => {
                        println!("  {}✗{} {} - {}", RED, RESET, account.email, e);
                        account.is_invalid = true;
                        account.invalid_reason = Some(e.to_string());
                        all_ok = false;
                    }
                }
            }

            // Save updated validity state
            if let Err(e) = store.save() {
                eprintln!("{}Failed to save accounts: {}{}", RED, e, RESET);
            }

            println!();
            if all_ok {
                println!("{}All accounts verified successfully.{}", GREEN, RESET);
            } else {
                println!("{}Some accounts failed verification.{}", YELLOW, RESET);
                println!(
                    "{}Run 'agcp login' to re-authenticate invalid accounts.{}",
                    DIM, RESET
                );
            }
            println!();
        }

        "help" | "-h" | "--help" => {
            println!();
            println!("{}Usage: agcp accounts <subcommand>{}", BOLD, RESET);
            println!();
            println!("{}Subcommands:{}", BOLD, RESET);
            println!("  {}list{}      Show all accounts", YELLOW, RESET);
            println!(
                "  {}remove{}    Remove an account by ID prefix",
                YELLOW, RESET
            );
            println!("  {}enable{}    Enable an account", YELLOW, RESET);
            println!("  {}disable{}   Disable an account", YELLOW, RESET);
            println!(
                "  {}switch{}    Set active account (for sticky strategy)",
                YELLOW, RESET
            );
            println!(
                "  {}strategy{}  Set selection strategy (sticky, roundrobin, hybrid)",
                YELLOW, RESET
            );
            println!(
                "  {}verify{}    Verify account tokens are valid",
                YELLOW, RESET
            );
            println!();
            println!("{}Examples:{}", BOLD, RESET);
            println!(
                "  {}agcp accounts list{}                 # List all accounts",
                DIM, RESET
            );
            println!(
                "  {}agcp accounts remove f6c3b4{}        # Remove account by ID prefix",
                DIM, RESET
            );
            println!(
                "  {}agcp accounts strategy roundrobin{}  # Set round-robin strategy",
                DIM, RESET
            );
            println!(
                "  {}agcp accounts verify{}               # Verify all account tokens",
                DIM, RESET
            );
            println!();
        }

        _ => {
            eprintln!("{}Unknown subcommand: {}{}", RED, subcommand, RESET);
            println!();
            println!("{}Usage: agcp accounts <subcommand>{}", BOLD, RESET);
            println!();
            println!("{}Subcommands:{}", BOLD, RESET);
            println!("  {}list{}      Show all accounts", YELLOW, RESET);
            println!("  {}remove{}    Remove an account", YELLOW, RESET);
            println!("  {}enable{}    Enable an account", YELLOW, RESET);
            println!("  {}disable{}   Disable an account", YELLOW, RESET);
            println!(
                "  {}switch{}    Set active account (for sticky strategy)",
                YELLOW, RESET
            );
            println!("  {}strategy{}  Set selection strategy", YELLOW, RESET);
            println!(
                "  {}verify{}    Verify account tokens are valid",
                YELLOW, RESET
            );
            println!();
            std::process::exit(1);
        }
    }
}

fn print_completions(shell: &str) {
    match shell.to_lowercase().as_str() {
        "bash" => print!(
            r#"_agcp() {{
    local cur prev commands
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    commands="login setup accounts config doctor test quota stats logs stop restart status upgrade tui version help completions"

    case "${{prev}}" in
        agcp)
            COMPREPLY=( $(compgen -W \"${{commands}} --port --host --network --foreground --debug --fallback --help --version\" -- \"${{cur}}\") )
            return 0
            ;;
        --port|-p)
            return 0
            ;;
        --host)
            return 0
            ;;
        accounts)
            COMPREPLY=( $(compgen -W "list remove enable disable switch strategy verify" -- "${{cur}}") )
            return 0
            ;;
        logs)
            COMPREPLY=( $(compgen -W "--lines --no-follow" -- "${{cur}}") )
            return 0
            ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish" -- "${{cur}}") )
            return 0
            ;;
    esac

    if [[ ${{cur}} == -* ]]; then
        COMPREPLY=( $(compgen -W \"--port --host --network --foreground --debug --fallback --help --version\" -- \"${{cur}}\") )
    fi
}}
complete -F _agcp agcp
"#
        ),
        "zsh" => print!(
            r#"#compdef agcp

_agcp() {{
    local -a commands
    commands=(
        'login:Authenticate with Google OAuth'
        'setup:Configure AI tools to use AGCP'
        'accounts:Manage multiple accounts'
        'config:Show current configuration'
        'doctor:Check configuration and connectivity'
        'test:Send a test request to verify setup'
        'quota:Show model quota usage'
        'stats:Show request statistics'
        'logs:View server logs'
        'stop:Stop the background server'
        'restart:Restart the background server'
        'status:Check if server is running'
        'upgrade:Check for and install updates'
        'tui:Launch interactive terminal UI'
        'version:Show version information'
        'help:Show help message'
        'completions:Generate shell completions'
    )

    local -a options
    options=(
        '-p[Server port]:port'
        '--port[Server port]:port'
        '--host[Bind address]:host'
        '--network[Listen on all interfaces for LAN access]'
        '--lan[Listen on all interfaces for LAN access]'
        '-f[Run in foreground]'
        '--foreground[Run in foreground]'
        '-d[Enable debug logging]'
        '--debug[Enable debug logging]'
        '--fallback[Enable model fallback on quota exhaustion]'
        '-h[Show help]'
        '--help[Show help]'
        '-V[Show version]'
        '--version[Show version]'
    )

    _arguments -C \
        "1: :->cmd" \
        "*::arg:->args"

    case "$state" in
        cmd)
            _describe -t commands 'agcp commands' commands
            _describe -t options 'agcp options' options
            ;;
        args)
            case $words[1] in
                logs)
                    _arguments \
                        '-n[Show last N lines]:lines' \
                        '--lines[Show last N lines]:lines' \
                        '--no-follow[Do not follow log output]'
                    ;;
                completions)
                    _values 'shell' bash zsh fish
                    ;;
                accounts)
                    _values 'subcommand' list remove enable disable switch strategy verify
                    ;;
            esac
            ;;
    esac
}}

_agcp "$@"
"#
        ),
        "fish" => print!(
            r#"complete -c agcp -f

# Commands
complete -c agcp -n "__fish_use_subcommand" -a login -d "Authenticate with Google OAuth"
complete -c agcp -n "__fish_use_subcommand" -a setup -d "Configure AI tools to use AGCP"
complete -c agcp -n "__fish_use_subcommand" -a accounts -d "Manage multiple accounts"
complete -c agcp -n "__fish_use_subcommand" -a config -d "Show current configuration"
complete -c agcp -n "__fish_use_subcommand" -a doctor -d "Check configuration and connectivity"
complete -c agcp -n "__fish_use_subcommand" -a test -d "Send a test request to verify setup"
complete -c agcp -n "__fish_use_subcommand" -a quota -d "Show model quota usage"
complete -c agcp -n "__fish_use_subcommand" -a stats -d "Show request statistics"
complete -c agcp -n "__fish_use_subcommand" -a logs -d "View server logs"
complete -c agcp -n "__fish_use_subcommand" -a stop -d "Stop the background server"
complete -c agcp -n "__fish_use_subcommand" -a restart -d "Restart the background server"
complete -c agcp -n "__fish_use_subcommand" -a status -d "Check if server is running"
complete -c agcp -n "__fish_use_subcommand" -a upgrade -d "Check for and install updates"
complete -c agcp -n "__fish_use_subcommand" -a tui -d "Launch interactive terminal UI"
complete -c agcp -n "__fish_use_subcommand" -a version -d "Show version information"
complete -c agcp -n "__fish_use_subcommand" -a help -d "Show help message"
complete -c agcp -n "__fish_use_subcommand" -a completions -d "Generate shell completions"

# Global options
complete -c agcp -n "__fish_use_subcommand" -s p -l port -d "Server port" -r
complete -c agcp -n "__fish_use_subcommand" -l host -d "Bind address" -r
complete -c agcp -n "__fish_use_subcommand" -l network -d "Listen on all interfaces (LAN access)"
complete -c agcp -n "__fish_use_subcommand" -l lan -d "Listen on all interfaces (LAN access)"
complete -c agcp -n "__fish_use_subcommand" -s f -l foreground -d "Run in foreground"
complete -c agcp -n "__fish_use_subcommand" -s d -l debug -d "Enable debug logging"
complete -c agcp -n "__fish_use_subcommand" -l fallback -d "Enable model fallback on quota exhaustion"
complete -c agcp -n "__fish_use_subcommand" -s h -l help -d "Show help"
complete -c agcp -n "__fish_use_subcommand" -s V -l version -d "Show version"

# logs subcommand
complete -c agcp -n "__fish_seen_subcommand_from logs" -s n -l lines -d "Show last N lines" -r
complete -c agcp -n "__fish_seen_subcommand_from logs" -l no-follow -d "Do not follow log output"

# completions subcommand
complete -c agcp -n "__fish_seen_subcommand_from completions" -a "bash zsh fish"

# accounts subcommand
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a list -d "Show all accounts"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a remove -d "Remove an account"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a enable -d "Enable an account"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a disable -d "Disable an account"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a switch -d "Set active account"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a strategy -d "Set selection strategy"
complete -c agcp -n "__fish_seen_subcommand_from accounts" -a verify -d "Verify account tokens"
"#
        ),
        _ => {
            eprintln!("Unknown shell: {}. Supported: bash, zsh, fish", shell);
            std::process::exit(1);
        }
    }

    // Print installation instructions to stderr (so they don't interfere with piping)
    eprintln!();
    eprintln!("\x1b[1mInstallation:\x1b[0m");
    match shell.to_lowercase().as_str() {
        "bash" => {
            eprintln!("  Add to your ~/.bashrc:");
            eprintln!("    \x1b[36meval \"$(agcp completions bash)\"\x1b[0m");
            eprintln!();
            eprintln!("  Or save to a file:");
            eprintln!(
                "    \x1b[36magcp completions bash > ~/.local/share/bash-completion/completions/agcp\x1b[0m"
            );
        }
        "zsh" => {
            eprintln!("  Add to your ~/.zshrc:");
            eprintln!("    \x1b[36meval \"$(agcp completions zsh)\"\x1b[0m");
            eprintln!();
            eprintln!("  Or save to a file (ensure fpath includes this directory):");
            eprintln!("    \x1b[36magcp completions zsh > ~/.zfunc/_agcp\x1b[0m");
        }
        "fish" => {
            eprintln!("  Save to fish completions directory:");
            eprintln!(
                "    \x1b[36magcp completions fish > ~/.config/fish/completions/agcp.fish\x1b[0m"
            );
        }
        _ => {}
    }
    eprintln!();
}
