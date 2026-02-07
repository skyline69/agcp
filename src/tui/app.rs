use std::collections::VecDeque;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, ScrollbarState};
use tachyonfx::EffectManager;

use super::effects::EffectKey;
use super::log_reader::LogTailer;
use super::theme;

/// Minimum terminal width for proper display
const MIN_WIDTH: u16 = 60;
/// Minimum terminal height for proper display
const MIN_HEIGHT: u16 = 15;

/// Available tabs in the TUI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Overview,
    Logs,
    Accounts,
    Config,
    Quota,
    About,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[
            Tab::Overview,
            Tab::Logs,
            Tab::Accounts,
            Tab::Config,
            Tab::Quota,
            Tab::About,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Logs => "Logs",
            Tab::Accounts => "Accounts",
            Tab::Config => "Config",
            Tab::Quota => "Quota",
            Tab::About => "About",
        }
    }

    pub fn next(&self) -> Tab {
        match self {
            Tab::Overview => Tab::Logs,
            Tab::Logs => Tab::Accounts,
            Tab::Accounts => Tab::Config,
            Tab::Config => Tab::Quota,
            Tab::Quota => Tab::About,
            Tab::About => Tab::Overview,
        }
    }

    pub fn prev(&self) -> Tab {
        match self {
            Tab::Overview => Tab::About,
            Tab::Logs => Tab::Overview,
            Tab::Accounts => Tab::Logs,
            Tab::Config => Tab::Accounts,
            Tab::Quota => Tab::Config,
            Tab::About => Tab::Quota,
        }
    }
}

/// Update check status
#[derive(Debug, Clone)]
pub enum UpdateStatus {
    /// Haven't checked yet
    NotChecked,
    /// Currently fetching from GitHub
    Checking,
    /// Already on the latest version
    UpToDate,
    /// A newer version is available
    UpdateAvailable { current: String, latest: String },
    /// Check failed
    Error(String),
}

/// Main application state
pub struct App {
    pub running: bool,
    pub current_tab: Tab,
    pub effects: EffectManager<EffectKey>,
    /// Data provider for live stats and accounts
    pub data: super::data::DataProvider,
    /// Log entries for logs view
    pub logs: VecDeque<super::data::LogEntry>,
    /// Scroll offset in logs view (0 = bottom/newest)
    pub log_scroll: usize,
    /// Scrollbar state for logs view
    pub log_scrollbar_state: ScrollbarState,
    /// Whether to auto-scroll to bottom when new logs arrive
    pub log_auto_scroll: bool,
    /// Log tailer for following new log entries
    pub log_tailer: LogTailer,
    /// Accounts list (cached)
    pub accounts: Vec<super::data::AccountInfo>,
    /// Selected account index in accounts view
    pub account_selected: usize,
    /// Whether help overlay is visible
    pub show_help: bool,
    /// Whether startup animation has been triggered
    startup_done: bool,
    /// Previous help state (for detecting transitions)
    prev_show_help: bool,
    /// Flag to trigger tab transition effect on next render
    trigger_tab_effect: bool,
    /// Total elapsed time for animations (in milliseconds)
    pub animation_time_ms: u64,
    /// Daemon start time (parsed from logs)
    pub daemon_start_time: Option<u64>,
    /// Last time logs were refreshed (for throttling)
    last_log_refresh: Instant,
    /// Cached tab areas for click detection (set during render)
    pub tab_areas: Vec<Rect>,
    /// Cached logs area for scroll detection
    pub logs_area: Rect,
    /// Cached scrollbar area for drag detection
    pub scrollbar_area: Rect,
    /// Cached accounts area for click detection
    pub accounts_area: Rect,
    /// Current mouse position
    pub mouse_pos: (u16, u16),
    /// Whether mouse is being dragged on scrollbar
    pub dragging_scrollbar: bool,
    /// Offset from click position to thumb center when drag started
    pub scrollbar_drag_offset: i16,
    /// Hovered tab index (None if not hovering any tab)
    pub hovered_tab: Option<usize>,
    /// Hovered account index (None if not hovering any account)
    pub hovered_account: Option<usize>,
    /// Hovered config field index (None if not hovering any field)
    pub hovered_config: Option<usize>,
    /// Cached quota data fetched from API
    pub quota_data: Vec<crate::cloudcode::quota::ModelQuota>,
    /// Last time quota was refreshed
    last_quota_refresh: Instant,
    /// Whether quota fetch is in progress (to avoid duplicate fetches)
    quota_fetch_pending: bool,
    /// Receiver for background quota fetch results
    quota_receiver: Option<mpsc::Receiver<Vec<crate::cloudcode::quota::ModelQuota>>>,
    /// Scroll offset in recent activity (0 = bottom/newest)
    pub activity_scroll: usize,
    /// Whether to auto-scroll recent activity to bottom
    pub activity_auto_scroll: bool,
    /// Cached recent activity area for scroll detection
    pub activity_area: Rect,
    /// Cached config area for click detection
    pub config_area: Rect,
    // Config editor state
    /// Editable config fields
    pub config_fields: Vec<super::config_editor::ConfigField>,
    /// Currently selected field index
    pub config_selected: usize,
    /// Currently in edit mode
    pub config_editing: bool,
    /// Text buffer for editing
    pub config_edit_buffer: String,
    /// Validation error message (if any)
    pub config_error: Option<String>,
    /// Changes have been saved, restart needed
    pub config_needs_restart: bool,
    /// Startup warnings/errors to display
    pub startup_warnings: Vec<super::widgets::StartupWarning>,
    /// Whether to show the startup warnings popup
    pub show_startup_warnings: bool,
    /// About page: cached inner area for mouse detection
    pub about_area: Rect,
    /// About page: whether the GitHub link is hovered
    pub about_link_hovered: bool,
    /// Update check status for About page
    pub update_status: UpdateStatus,
    /// Receiver for background update check result
    update_receiver: Option<mpsc::Receiver<UpdateStatus>>,
    /// Cached server status (refreshed every 2 seconds)
    pub cached_server_status: super::data::ServerStatus,
    /// Last time server status was checked
    last_status_refresh: Instant,
    /// Cached overview stats (refreshed alongside logs every 500ms)
    pub cached_request_count: u64,
    pub cached_model_usage: Vec<super::data::ModelUsage>,
    pub cached_rate_history: Vec<u64>,
    pub cached_avg_response_ms: Option<u64>,
    pub cached_requests_per_min: f64,
    /// Cached uptime string (refreshed alongside logs every 500ms)
    pub cached_uptime: String,
    /// Last tab area width used for tab_areas calculation (for invalidation)
    cached_tabs_area: Rect,
}

impl App {
    pub fn new() -> Self {
        let log_path = super::data::DataProvider::get_log_path();

        // Single pass: read last N log lines and find server start line
        let (logs, server_start_line) =
            super::log_reader::read_last_lines_and_start(&log_path, 500);
        let daemon_start_time =
            server_start_line.and_then(|line| super::data::parse_daemon_start_from_line(&line));

        let log_count = logs.len();

        // Collect startup warnings
        let startup_warnings = super::widgets::startup_warnings::collect_startup_warnings();
        let show_startup_warnings = !startup_warnings.is_empty();

        let data = super::data::DataProvider::new();
        let initial_status = data.get_server_status();

        // Compute initial cached stats from startup logs
        let now = super::data::current_time_secs();
        let (initial_request_count, initial_model_usage) =
            super::data::count_requests_from_logs(&logs);
        let initial_rate_history = super::data::build_rate_history(&logs, now);
        let initial_avg_response_ms = super::data::calculate_avg_response_time(&logs);
        let initial_requests_per_min = super::data::calculate_requests_per_min(&logs, now);

        Self {
            running: true,
            current_tab: Tab::default(),
            effects: EffectManager::default(),
            data,
            logs,
            log_scroll: 0,
            log_scrollbar_state: ScrollbarState::new(log_count),
            log_auto_scroll: true,
            log_tailer: LogTailer::new(&log_path),
            accounts: super::data::DataProvider::new().get_accounts(),
            account_selected: 0,
            show_help: false,
            startup_done: false,
            prev_show_help: false,
            trigger_tab_effect: false,
            animation_time_ms: 0,
            daemon_start_time,
            last_log_refresh: Instant::now(),
            tab_areas: Vec::new(),
            logs_area: Rect::default(),
            scrollbar_area: Rect::default(),
            accounts_area: Rect::default(),
            mouse_pos: (0, 0),
            dragging_scrollbar: false,
            scrollbar_drag_offset: 0,
            hovered_tab: None,
            hovered_account: None,
            hovered_config: None,
            quota_data: Vec::new(),
            last_quota_refresh: Instant::now() - Duration::from_secs(120), // Trigger immediate fetch
            quota_fetch_pending: false,
            quota_receiver: None,
            activity_scroll: 0,
            activity_auto_scroll: true,
            activity_area: Rect::default(),
            config_area: Rect::default(),
            config_fields: super::config_editor::build_config_fields(&crate::config::get_config()),
            config_selected: 0,
            config_editing: false,
            config_edit_buffer: String::new(),
            config_error: None,
            config_needs_restart: false,
            startup_warnings,
            show_startup_warnings,
            about_area: Rect::default(),
            about_link_hovered: false,
            update_status: UpdateStatus::NotChecked,
            update_receiver: None,
            cached_server_status: initial_status,
            last_status_refresh: Instant::now(),
            cached_request_count: initial_request_count,
            cached_model_usage: initial_model_usage,
            cached_rate_history: initial_rate_history,
            cached_avg_response_ms: initial_avg_response_ms,
            cached_requests_per_min: initial_requests_per_min,
            cached_uptime: String::from("00:00:00"),
            cached_tabs_area: Rect::default(),
        }
    }

    /// Refresh logs from file and update cached stats
    pub fn refresh_logs(&mut self) {
        let new_entries = self.log_tailer.read_new_lines();
        let new_count = new_entries.len();
        super::log_reader::append_entries(&mut self.logs, new_entries);

        // If we were at the bottom (auto-scroll) and new logs came in, stay at bottom
        if new_count > 0 {
            if self.log_auto_scroll {
                self.log_scroll = 0;
            }
            if self.activity_auto_scroll {
                self.activity_scroll = 0;
            }
        }

        // Update daemon start time if we find a newer server start
        if let Some(new_start) = super::data::find_daemon_start_time(&self.logs) {
            // Only update if this is a newer start (server restarted)
            if self.daemon_start_time.is_none_or(|old| new_start > old) {
                self.daemon_start_time = Some(new_start);
            }
        }

        // Recompute cached overview stats (avoids 4 full log scans per frame)
        self.refresh_cached_stats();

        // Update cached uptime string (avoids format! allocation per frame)
        self.cached_uptime = self.get_daemon_uptime();
    }

    /// Recompute overview stats from logs (called once per log refresh, not per frame)
    fn refresh_cached_stats(&mut self) {
        let now = super::data::current_time_secs();
        let (count, models) = super::data::count_requests_from_logs(&self.logs);
        self.cached_request_count = count;
        self.cached_model_usage = models;
        self.cached_rate_history = super::data::build_rate_history(&self.logs, now);
        self.cached_avg_response_ms = super::data::calculate_avg_response_time(&self.logs);
        self.cached_requests_per_min = super::data::calculate_requests_per_min(&self.logs, now);
    }

    /// Get daemon uptime as a formatted string (HH:MM:SS)
    pub fn get_daemon_uptime(&self) -> String {
        if let Some(start) = self.daemon_start_time {
            let now = super::data::current_time_secs();
            let elapsed = now.saturating_sub(start);
            let hours = elapsed / 3600;
            let mins = (elapsed % 3600) / 60;
            let secs = elapsed % 60;
            format!("{:02}:{:02}:{:02}", hours, mins, secs)
        } else {
            "00:00:00".to_string()
        }
    }

    /// Refresh account list from disk
    pub fn refresh_accounts(&mut self) {
        self.accounts = self.data.get_accounts();
        if self.account_selected >= self.accounts.len() {
            self.account_selected = self.accounts.len().saturating_sub(1);
        }
    }

    /// Check if quota needs refresh and fetch if so (non-blocking)
    pub fn maybe_refresh_quota(&mut self) {
        // Check for results from a pending background fetch
        if let Some(ref receiver) = self.quota_receiver {
            match receiver.try_recv() {
                Ok(quotas) => {
                    self.quota_data = quotas;
                    self.quota_fetch_pending = false;
                    self.quota_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Background thread finished without sending (fetch failed)
                    self.quota_fetch_pending = false;
                    self.quota_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still in progress, keep waiting
                }
            }
        }

        // Refresh every 60 seconds
        if self.last_quota_refresh.elapsed() < Duration::from_secs(60) {
            return;
        }

        if self.quota_fetch_pending {
            return;
        }

        self.quota_fetch_pending = true;
        self.last_quota_refresh = Instant::now();

        // Spawn quota fetch on a background thread to avoid blocking the render loop
        let (tx, rx) = mpsc::channel();
        self.quota_receiver = Some(rx);

        std::thread::spawn(move || {
            if let Ok(quotas) = Self::fetch_quota_blocking() {
                let _ = tx.send(quotas);
            }
        });
    }

    /// Get cached server status, refreshing every 2 seconds
    pub fn get_cached_server_status(&mut self) -> super::data::ServerStatus {
        if self.last_status_refresh.elapsed() >= Duration::from_secs(2) {
            self.cached_server_status = self.data.get_server_status();
            self.last_status_refresh = Instant::now();
        }
        self.cached_server_status
    }

    /// Trigger update check if not already done. Called when About tab is shown.
    pub fn maybe_check_for_updates(&mut self) {
        // Poll for results from a pending check
        if let Some(ref receiver) = self.update_receiver {
            match receiver.try_recv() {
                Ok(status) => {
                    self.update_status = status;
                    self.update_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.update_status = UpdateStatus::Error("Update check failed".to_string());
                    self.update_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {} // Still in progress
            }
        }

        // Only trigger once
        if !matches!(self.update_status, UpdateStatus::NotChecked) {
            return;
        }

        self.update_status = UpdateStatus::Checking;

        let (tx, rx) = mpsc::channel();
        self.update_receiver = Some(rx);

        std::thread::spawn(move || {
            let status = Self::fetch_update_status();
            let _ = tx.send(status);
        });
    }

    /// Fetch latest version from GitHub and compare (runs on background thread)
    fn fetch_update_status() -> UpdateStatus {
        let current_version = env!("CARGO_PKG_VERSION");
        let repo = env!("CARGO_PKG_REPOSITORY");

        let repo_path = repo
            .trim_end_matches('/')
            .strip_prefix("https://github.com/")
            .unwrap_or("skyline69/agcp");

        let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo_path);

        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => return UpdateStatus::Error(format!("Runtime error: {}", e)),
        };

        let result = rt.block_on(async {
            let client = crate::auth::HttpClient::new();
            let headers = [
                ("Accept", "application/vnd.github.v3+json"),
                ("User-Agent", "agcp"),
            ];
            let body = client.get(&api_url, &headers).await?;
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
        });

        match result {
            Ok(latest_raw) => {
                let latest = latest_raw.strip_prefix('v').unwrap_or(&latest_raw);
                let current = current_version.strip_prefix('v').unwrap_or(current_version);

                if current == latest {
                    UpdateStatus::UpToDate
                } else if Self::version_is_newer(latest, current) {
                    UpdateStatus::UpdateAvailable {
                        current: current.to_string(),
                        latest: latest.to_string(),
                    }
                } else {
                    // Running a newer version than latest release
                    UpdateStatus::UpToDate
                }
            }
            Err(e) => UpdateStatus::Error(e),
        }
    }

    /// Returns true if version `a` is newer than version `b`
    fn version_is_newer(a: &str, b: &str) -> bool {
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

    /// Fetch quota data synchronously (blocking)
    /// Can be called from any thread — creates its own tokio runtime if needed
    fn fetch_quota_blocking() -> Result<Vec<crate::cloudcode::quota::ModelQuota>, String> {
        // Create a new runtime for this thread (works from any context)
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create runtime: {}", e))?;

        rt.block_on(async {
            // Load account
            let mut account = crate::auth::accounts::Account::load()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "No account configured".to_string())?;

            let http_client = crate::auth::HttpClient::new();
            let access_token = account
                .get_access_token(&http_client)
                .await
                .map_err(|e| e.to_string())?;

            crate::cloudcode::fetch_model_quotas(
                &http_client,
                &access_token,
                account.project_id.as_deref(),
            )
            .await
        })
    }

    /// Toggle enabled state of selected account
    fn toggle_account_enabled(&mut self) {
        if let Some(acc) = self.accounts.get(self.account_selected)
            && let Ok(mut store) = crate::auth::accounts::AccountStore::load()
            && let Some(account) = store.accounts.iter_mut().find(|a| a.id == acc.id)
        {
            account.enabled = !account.enabled;
            let _ = store.save();
            self.refresh_accounts();
        }
    }

    /// Set selected account as active
    fn set_active_account(&mut self) {
        if let Some(acc) = self.accounts.get(self.account_selected)
            && let Ok(mut store) = crate::auth::accounts::AccountStore::load()
        {
            store.active_account_id = Some(acc.id.clone());
            let _ = store.save();
            self.refresh_accounts();
        }
    }

    /// Handle keyboard input
    pub fn handle_key(&mut self, code: KeyCode) {
        // Handle startup warnings popup first (blocks other input)
        if self.show_startup_warnings {
            match code {
                KeyCode::Enter | KeyCode::Esc => {
                    self.show_startup_warnings = false;
                }
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Char('q') => self.running = false,
            // Config-specific Esc (must come before general Esc)
            KeyCode::Esc if self.current_tab == Tab::Config && self.config_editing => {
                self.config_cancel_edit();
            }
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    self.running = false;
                }
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            // Text input while editing config (MUST come before tab number shortcuts)
            KeyCode::Char(c) if self.current_tab == Tab::Config && self.config_editing => {
                // Filter input: only allow valid characters for the field type
                if let Some(field) = self.config_fields.get(self.config_selected) {
                    if field.is_numeric() {
                        // Only allow digits and '.' for floats
                        if c.is_ascii_digit()
                            || (c == '.'
                                && matches!(
                                    field.field_type,
                                    super::config_editor::FieldType::Float { .. }
                                ))
                        {
                            self.config_edit_buffer.push(c);
                            self.config_error = None;
                        }
                    } else {
                        // Allow any character for text fields
                        self.config_edit_buffer.push(c);
                        self.config_error = None;
                    }
                }
            }
            KeyCode::Backspace if self.current_tab == Tab::Config && self.config_editing => {
                self.config_edit_buffer.pop();
                self.config_error = None;
            }
            // Config-specific Left/Right for enum cycling (only when on enum field)
            KeyCode::Left
                if self.current_tab == Tab::Config
                    && !self.config_editing
                    && self.config_selected_is_enum() =>
            {
                self.config_cycle_enum(false);
            }
            KeyCode::Right
                if self.current_tab == Tab::Config
                    && !self.config_editing
                    && self.config_selected_is_enum() =>
            {
                self.config_cycle_enum(true);
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.current_tab = self.current_tab.next();
                self.trigger_tab_effect = true;
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.current_tab = self.current_tab.prev();
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('1') => {
                self.current_tab = Tab::Overview;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('2') => {
                self.current_tab = Tab::Logs;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('3') => {
                self.current_tab = Tab::Accounts;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('4') => {
                self.current_tab = Tab::Config;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('5') => {
                self.current_tab = Tab::Quota;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('6') => {
                self.current_tab = Tab::About;
                self.trigger_tab_effect = true;
            }
            // Account navigation (when on Accounts tab)
            KeyCode::Up | KeyCode::Char('k') if self.current_tab == Tab::Accounts => {
                if self.account_selected > 0 {
                    self.account_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.current_tab == Tab::Accounts => {
                if self.account_selected < self.accounts.len().saturating_sub(1) {
                    self.account_selected += 1;
                }
            }
            // Toggle account enabled (when on Accounts tab)
            KeyCode::Char('e') if self.current_tab == Tab::Accounts => {
                self.toggle_account_enabled();
            }
            // Set as active account
            KeyCode::Enter if self.current_tab == Tab::Accounts => {
                self.set_active_account();
            }
            // Refresh accounts
            KeyCode::Char('r') if self.current_tab == Tab::Accounts => {
                self.refresh_accounts();
            }
            // Log scrolling (when on Logs tab)
            KeyCode::Up | KeyCode::Char('k') if self.current_tab == Tab::Logs => {
                self.scroll_logs_up(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.current_tab == Tab::Logs => {
                self.scroll_logs_down(1);
            }
            KeyCode::PageUp if self.current_tab == Tab::Logs => {
                self.scroll_logs_up(10);
            }
            KeyCode::PageDown if self.current_tab == Tab::Logs => {
                self.scroll_logs_down(10);
            }
            KeyCode::Home if self.current_tab == Tab::Logs => {
                self.log_scroll = self.logs.len().saturating_sub(1);
                self.log_auto_scroll = false;
                self.update_scrollbar();
            }
            KeyCode::End if self.current_tab == Tab::Logs => {
                self.log_scroll = 0;
                self.log_auto_scroll = true;
                self.update_scrollbar();
            }
            // Config navigation (when on Config tab and not editing)
            KeyCode::Up | KeyCode::Char('k')
                if self.current_tab == Tab::Config && !self.config_editing =>
            {
                self.config_prev();
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.current_tab == Tab::Config && !self.config_editing =>
            {
                self.config_next();
            }
            // Start editing / toggle bool / confirm edit
            KeyCode::Enter if self.current_tab == Tab::Config => {
                if self.config_editing {
                    self.config_confirm_edit();
                } else if let Some(field) = self.config_fields.get(self.config_selected) {
                    match &field.field_type {
                        super::config_editor::FieldType::Bool => self.config_toggle_bool(),
                        _ => self.config_start_edit(),
                    }
                }
            }
            // Toggle with space for booleans
            KeyCode::Char(' ') if self.current_tab == Tab::Config && !self.config_editing => {
                self.config_toggle_bool();
            }
            // Save config
            KeyCode::Char('s') if self.current_tab == Tab::Config && !self.config_editing => {
                if let Err(e) = self.config_save() {
                    self.config_error = Some(e);
                }
            }
            // Restart daemon
            KeyCode::Char('r')
                if self.current_tab == Tab::Config
                    && self.config_needs_restart
                    && !self.config_editing =>
            {
                self.restart_daemon();
            }
            _ => {}
        }
    }

    /// Scroll logs up (towards older entries)
    pub fn scroll_logs_up(&mut self, amount: usize) {
        let max_scroll = self.logs.len().saturating_sub(1);
        self.log_scroll = (self.log_scroll + amount).min(max_scroll);
        self.log_auto_scroll = false; // User scrolled up, disable auto-scroll
        self.update_scrollbar();
    }

    /// Scroll logs down (towards newer entries)
    pub fn scroll_logs_down(&mut self, amount: usize) {
        self.log_scroll = self.log_scroll.saturating_sub(amount);
        // Re-enable auto-scroll if we're at the bottom
        if self.log_scroll == 0 {
            self.log_auto_scroll = true;
        }
        self.update_scrollbar();
    }

    /// Scroll activity up (towards older entries)
    pub fn scroll_activity_up(&mut self, amount: usize) {
        let max_scroll = self.logs.len().saturating_sub(1);
        self.activity_scroll = (self.activity_scroll + amount).min(max_scroll);
        self.activity_auto_scroll = false; // User scrolled up, disable auto-scroll
    }

    /// Scroll activity down (towards newer entries)
    pub fn scroll_activity_down(&mut self, amount: usize) {
        self.activity_scroll = self.activity_scroll.saturating_sub(amount);
        // Re-enable auto-scroll if we're at the bottom
        if self.activity_scroll == 0 {
            self.activity_auto_scroll = true;
        }
    }

    /// Move to previous config field
    pub fn config_prev(&mut self) {
        if self.config_selected > 0 {
            self.config_selected -= 1;
        }
    }

    /// Move to next config field
    pub fn config_next(&mut self) {
        if self.config_selected < self.config_fields.len().saturating_sub(1) {
            self.config_selected += 1;
        }
    }

    /// Start editing current field
    pub fn config_start_edit(&mut self) {
        if let Some(field) = self.config_fields.get(self.config_selected) {
            self.config_editing = true;
            self.config_edit_buffer = field.value.clone();
            self.config_error = None;
        }
    }

    /// Cancel editing
    pub fn config_cancel_edit(&mut self) {
        self.config_editing = false;
        self.config_edit_buffer.clear();
        self.config_error = None;
    }

    /// Confirm edit and validate
    pub fn config_confirm_edit(&mut self) {
        if let Some(field) = self.config_fields.get_mut(self.config_selected) {
            let old_value = field.value.clone();
            field.value = self.config_edit_buffer.clone();

            if let Err(e) = field.validate() {
                field.value = old_value;
                self.config_error = Some(e);
                return;
            }

            self.config_editing = false;
            self.config_edit_buffer.clear();
            self.config_error = None;
        }
    }

    /// Toggle boolean field
    pub fn config_toggle_bool(&mut self) {
        if let Some(field) = self.config_fields.get_mut(self.config_selected)
            && matches!(field.field_type, super::config_editor::FieldType::Bool)
        {
            field.value = if field.value == "true" {
                "false"
            } else {
                "true"
            }
            .to_string();
        }
    }

    /// Cycle enum field to next value
    pub fn config_cycle_enum(&mut self, forward: bool) {
        if let Some(field) = self.config_fields.get_mut(self.config_selected)
            && let super::config_editor::FieldType::Enum(ref values) = field.field_type
            && let Some(idx) = values.iter().position(|&v| v == field.value)
        {
            let new_idx = if forward {
                (idx + 1) % values.len()
            } else {
                (idx + values.len() - 1) % values.len()
            };
            field.value = values[new_idx].to_string();
        }
    }

    /// Check if any config field has been modified
    pub fn config_has_changes(&self) -> bool {
        self.config_fields.iter().any(|f| f.is_modified())
    }

    /// Check if the currently selected config field is an enum
    fn config_selected_is_enum(&self) -> bool {
        self.config_fields
            .get(self.config_selected)
            .is_some_and(|f| matches!(f.field_type, super::config_editor::FieldType::Enum(_)))
    }

    /// Apply config fields back to a Config struct
    fn apply_fields_to_config(&self) -> crate::config::Config {
        let mut config = crate::config::get_config();

        for field in &self.config_fields {
            match (field.section, field.key) {
                ("server", "port") => {
                    if let Ok(v) = field.value.parse() {
                        config.server.port = v;
                    }
                }
                ("server", "host") => {
                    config.server.host = field.value.clone();
                }
                ("server", "request_timeout_secs") => {
                    if let Ok(v) = field.value.parse() {
                        config.server.request_timeout_secs = v;
                    }
                }
                ("logging", "debug") => {
                    config.logging.debug = field.value == "true";
                }
                ("logging", "log_requests") => {
                    config.logging.log_requests = field.value == "true";
                }
                ("accounts", "strategy") => {
                    config.accounts.strategy = field.value.clone();
                }
                ("accounts", "quota_threshold") => {
                    if let Ok(v) = field.value.parse() {
                        config.accounts.quota_threshold = v;
                    }
                }
                ("accounts", "fallback") => {
                    config.accounts.fallback = field.value == "true";
                }
                ("cache", "enabled") => {
                    config.cache.enabled = field.value == "true";
                }
                ("cache", "ttl_seconds") => {
                    if let Ok(v) = field.value.parse() {
                        config.cache.ttl_seconds = v;
                    }
                }
                ("cache", "max_entries") => {
                    if let Ok(v) = field.value.parse() {
                        config.cache.max_entries = v;
                    }
                }
                ("cloudcode", "timeout_secs") => {
                    if let Ok(v) = field.value.parse() {
                        config.cloudcode.timeout_secs = v;
                    }
                }
                ("cloudcode", "max_retries") => {
                    if let Ok(v) = field.value.parse() {
                        config.cloudcode.max_retries = v;
                    }
                }
                ("cloudcode", "max_concurrent_requests") => {
                    if let Ok(v) = field.value.parse() {
                        config.cloudcode.max_concurrent_requests = v;
                    }
                }
                ("cloudcode", "min_request_interval_ms") => {
                    if let Ok(v) = field.value.parse() {
                        config.cloudcode.min_request_interval_ms = v;
                    }
                }
                _ => {}
            }
        }

        config
    }

    /// Save config to disk
    pub fn config_save(&mut self) -> Result<(), String> {
        let config = self.apply_fields_to_config();
        config.save().map_err(|e| e.to_string())?;

        // Mark fields as no longer modified
        for field in &mut self.config_fields {
            field.original = field.value.clone();
        }

        self.config_needs_restart = true;
        Ok(())
    }

    /// Restart the daemon
    pub fn restart_daemon(&mut self) {
        use std::process::Command;

        // Stop the daemon first
        let _ = Command::new("pkill").args(["-f", "agcp.*daemon"]).status();

        // Small delay to let it stop
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Start daemon in background
        let exe = std::env::current_exe().unwrap_or_else(|_| "agcp".into());
        let _ = Command::new(&exe).arg("daemon").spawn();

        self.config_needs_restart = false;

        // Reload config fields to reflect saved state
        self.config_fields =
            super::config_editor::build_config_fields(&crate::config::get_config());
    }

    /// Update scrollbar state to match current scroll position
    fn update_scrollbar(&mut self) {
        // Note: The actual scrollbar state is set in logs.rs during render
        // with correct viewport_content_length. This is just a placeholder
        // to keep the state roughly in sync for drag calculations.
        let max_scroll = self.logs.len().saturating_sub(1);
        let position = max_scroll.saturating_sub(self.log_scroll);
        self.log_scrollbar_state = self
            .log_scrollbar_state
            .content_length(self.logs.len())
            .position(position);
    }

    /// Handle mouse events
    pub fn handle_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        // Always update mouse position for hover detection
        self.mouse_pos = (column, row);

        // Update hover states
        self.update_hover_state(column, row);

        match kind {
            // Scroll wheel
            MouseEventKind::ScrollUp => {
                if self.current_tab == Tab::Logs && self.is_in_rect(column, row, self.logs_area) {
                    self.scroll_logs_up(3);
                } else if self.current_tab == Tab::Overview
                    && self.is_in_rect(column, row, self.activity_area)
                {
                    self.scroll_activity_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.current_tab == Tab::Logs && self.is_in_rect(column, row, self.logs_area) {
                    self.scroll_logs_down(3);
                } else if self.current_tab == Tab::Overview
                    && self.is_in_rect(column, row, self.activity_area)
                {
                    self.scroll_activity_down(3);
                }
            }
            // Left click down
            MouseEventKind::Down(MouseButton::Left) => {
                // Check scrollbar drag start
                if self.current_tab == Tab::Logs
                    && self.is_in_rect(column, row, self.scrollbar_area)
                {
                    self.dragging_scrollbar = true;
                    // Calculate offset from click to current thumb position
                    let thumb_y = self.get_scrollbar_thumb_y();
                    self.scrollbar_drag_offset = thumb_y as i16 - row as i16;
                    return;
                }

                // Check tab clicks
                for (i, tab_area) in self.tab_areas.iter().enumerate() {
                    if self.is_in_rect(column, row, *tab_area)
                        && let Some(tab) = Tab::all().get(i)
                    {
                        self.current_tab = *tab;
                        self.trigger_tab_effect = true;
                        return;
                    }
                }

                // Check account clicks (when on Accounts tab)
                if self.current_tab == Tab::Accounts
                    && self.is_in_rect(column, row, self.accounts_area)
                {
                    // Calculate which account was clicked based on row
                    let relative_row = row.saturating_sub(self.accounts_area.y + 1); // +1 for border
                    let clicked_index = relative_row as usize;
                    if clicked_index < self.accounts.len() {
                        self.account_selected = clicked_index;
                    }
                }

                // Check config field clicks
                if self.current_tab == Tab::Config
                    && self.is_in_rect(column, row, self.config_area)
                    && !self.config_editing
                {
                    let relative_row = row.saturating_sub(self.config_area.y);
                    if let Some(idx) = self.row_to_config_index(relative_row as usize) {
                        self.config_selected = idx;
                    }
                }

                // Check about page link click
                if self.current_tab == Tab::About && self.about_link_hovered {
                    super::views::about::open_url(super::views::about::GITHUB_URL);
                }
            }
            // Mouse drag (while button held)
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_scrollbar {
                    // Apply the offset so thumb stays under cursor
                    let adjusted_row = (row as i16 + self.scrollbar_drag_offset) as u16;
                    self.scroll_to_position(adjusted_row);
                }
            }
            // Mouse button release
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging_scrollbar = false;
            }
            _ => {}
        }
    }

    /// Update hover state based on mouse position
    fn update_hover_state(&mut self, column: u16, row: u16) {
        // Check tab hover
        self.hovered_tab = None;
        for (i, tab_area) in self.tab_areas.iter().enumerate() {
            if self.is_in_rect(column, row, *tab_area) {
                self.hovered_tab = Some(i);
                break;
            }
        }

        // Check account hover
        self.hovered_account = None;
        if self.current_tab == Tab::Accounts && self.is_in_rect(column, row, self.accounts_area) {
            let relative_row = row.saturating_sub(self.accounts_area.y + 1);
            let hovered_index = relative_row as usize;
            if hovered_index < self.accounts.len() {
                self.hovered_account = Some(hovered_index);
            }
        }

        // Check config field hover
        self.hovered_config = None;
        if self.current_tab == Tab::Config
            && self.is_in_rect(column, row, self.config_area)
            && !self.config_editing
        {
            let relative_row = row.saturating_sub(self.config_area.y);
            if let Some(idx) = self.row_to_config_index(relative_row as usize) {
                self.hovered_config = Some(idx);
            }
        }

        // Check about page link hover
        self.about_link_hovered = false;
        if self.current_tab == Tab::About && self.is_in_rect(column, row, self.about_area) {
            let link_row = super::views::about::get_link_row(self.about_area.height);
            let relative_row = row.saturating_sub(self.about_area.y);
            if relative_row == link_row {
                // Check if within the link text horizontally
                let link_width = super::views::about::GITHUB_URL.len() as u16;
                let link_start_x =
                    self.about_area.x + (self.about_area.width.saturating_sub(link_width)) / 2;
                let link_end_x = link_start_x + link_width;
                if column >= link_start_x && column < link_end_x {
                    self.about_link_hovered = true;
                }
            }
        }
    }

    /// Scroll to a position based on scrollbar click/drag
    fn scroll_to_position(&mut self, row: u16) {
        if self.scrollbar_area.height == 0 || self.logs.is_empty() {
            return;
        }

        // Calculate relative position in scrollbar (0.0 = top, 1.0 = bottom)
        let relative_y = row.saturating_sub(self.scrollbar_area.y) as f64;
        let scrollbar_height = self.scrollbar_area.height as f64;
        let ratio = (relative_y / scrollbar_height).clamp(0.0, 1.0);

        // Convert to scroll position (0 = bottom/newest, max = top/oldest)
        let max_scroll = self.logs.len().saturating_sub(1);
        // Invert because scrollbar top = oldest (high scroll), bottom = newest (low scroll)
        self.log_scroll = ((1.0 - ratio) * max_scroll as f64) as usize;

        // Update auto-scroll state
        self.log_auto_scroll = self.log_scroll == 0;
        self.update_scrollbar();
    }

    /// Get the current Y position of the scrollbar thumb
    fn get_scrollbar_thumb_y(&self) -> u16 {
        if self.scrollbar_area.height == 0 || self.logs.is_empty() {
            return self.scrollbar_area.y;
        }

        let max_scroll = self.logs.len().saturating_sub(1);
        if max_scroll == 0 {
            return self.scrollbar_area.y;
        }

        // Calculate ratio (inverted: log_scroll=0 means bottom, log_scroll=max means top)
        let ratio = 1.0 - (self.log_scroll as f64 / max_scroll as f64);

        // Account for begin/end symbols (▲ and ▼) which take 1 row each
        let track_height = self.scrollbar_area.height.saturating_sub(2);
        let thumb_offset = (ratio * track_height as f64) as u16;

        // +1 to skip the ▲ symbol at top
        self.scrollbar_area.y + 1 + thumb_offset
    }

    /// Check if a position is within a rect
    fn is_in_rect(&self, x: u16, y: u16, rect: Rect) -> bool {
        x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
    }

    /// Convert a row offset to config field index (accounting for headers)
    fn row_to_config_index(&self, row: usize) -> Option<usize> {
        let mut current_row = 0;
        let mut current_section = "";

        for (idx, field) in self.config_fields.iter().enumerate() {
            if field.section != current_section {
                if !current_section.is_empty() {
                    current_row += 1; // blank line
                }
                current_row += 1; // section header
                current_section = field.section;
            }

            if current_row == row {
                return Some(idx);
            }
            current_row += 1;
        }

        None
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the TUI application
pub fn run() -> io::Result<()> {
    // Setup terminal with mouse capture
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Create app state
    let mut app = App::new();
    let mut last_frame = Instant::now();

    // Main loop
    while app.running {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();

        // Throttle log refresh to ~1 per second for performance
        // (reading logs every frame at 60fps is too expensive)
        if app.last_log_refresh.elapsed() >= Duration::from_millis(500) {
            app.refresh_logs();
            app.last_log_refresh = Instant::now();
        }

        // Refresh quota data periodically (every 60 seconds)
        app.maybe_refresh_quota();

        // Draw
        terminal.draw(|frame| {
            render(frame, &mut app, elapsed);
        })?;

        // Handle events (with timeout for ~60fps)
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key.code);
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse.kind, mouse.column, mouse.row);
                }
                _ => {}
            }
        }
    }

    // Restore terminal
    io::stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}

/// Render the UI
fn render(frame: &mut Frame, app: &mut App, elapsed: Duration) {
    let area = frame.area();

    // Check minimum terminal size
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = Paragraph::new(format!(
            "Terminal too small\n\nMinimum: {}x{}\nCurrent: {}x{}",
            MIN_WIDTH, MIN_HEIGHT, area.width, area.height
        ))
        .alignment(Alignment::Center)
        .style(theme::warning());

        frame.render_widget(msg, area);
        return;
    }

    // Clear with background color
    frame.render_widget(Block::default().style(theme::base()), area);

    // Accumulate elapsed time for animations (time-based, not frame-based)
    app.animation_time_ms = app
        .animation_time_ms
        .wrapping_add(elapsed.as_millis() as u64);

    // Main layout: Header | Tabs | Content | Footer
    let chunks = Layout::vertical([
        Constraint::Length(1), // Header
        Constraint::Length(1), // Tabs
        Constraint::Fill(1),   // Content
        Constraint::Length(1), // Footer
    ])
    .split(area);

    let content_area = chunks[2];

    // Trigger startup effect on first render
    if !app.startup_done {
        app.startup_done = true;
        let effect = super::effects::startup_sweep(area);
        app.effects.add_unique_effect(EffectKey::Startup, effect);
    }

    // Trigger tab transition effect if needed
    if app.trigger_tab_effect {
        app.trigger_tab_effect = false;
        let effect = super::effects::tab_appear(content_area);
        app.effects
            .add_unique_effect(EffectKey::TabTransition, effect);
    }

    // Trigger help overlay effect on transition
    if app.show_help != app.prev_show_help {
        app.prev_show_help = app.show_help;
        if app.show_help {
            let effect = super::effects::help_fade_in(area);
            app.effects
                .add_unique_effect(EffectKey::HelpOverlay, effect);
        }
    }

    // Header - use cached uptime (refreshed every 500ms alongside logs)
    let status = app.get_cached_server_status();
    let header = super::widgets::Header::new(
        status.is_running(),
        &app.cached_uptime,
        app.animation_time_ms,
    );
    frame.render_widget(header, chunks[0]);

    // Tabs - only recalculate clickable areas when terminal size changes
    let tabs_area = chunks[1];
    if app.cached_tabs_area != tabs_area {
        app.cached_tabs_area = tabs_area;
        app.tab_areas = calculate_tab_areas(tabs_area);
    }
    let tabs = super::widgets::TabBar::new(app.current_tab).hovered(app.hovered_tab);
    frame.render_widget(tabs, tabs_area);

    // Store content area for scroll detection
    if app.current_tab == Tab::Logs {
        app.logs_area = content_area;
    }

    // Content area - render based on current tab
    match app.current_tab {
        Tab::Overview => super::views::overview::render(frame, content_area, app),
        Tab::Logs => super::views::logs::render(frame, content_area, app),
        Tab::Accounts => {
            app.accounts_area = content_area;
            super::views::accounts::render(frame, content_area, app);
        }
        Tab::Config => super::views::config::render(frame, content_area, app),
        Tab::Quota => super::views::quota::render(frame, content_area, &app.quota_data),
        Tab::About => {
            // Trigger update check on first visit to About tab
            app.maybe_check_for_updates();
            // Store the inner area for mouse detection
            let block = ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL);
            app.about_area = block.inner(content_area);
            super::views::about::render(
                frame,
                content_area,
                app.animation_time_ms,
                app.about_link_hovered,
                &app.update_status,
            );
        }
    }

    // Footer
    let footer = super::widgets::Footer::for_tab(app.current_tab);
    frame.render_widget(footer, chunks[3]);

    // Help overlay
    if app.show_help {
        super::widgets::help::render(frame, area);
    }

    // Startup warnings popup (rendered on top of everything)
    if app.show_startup_warnings {
        super::widgets::startup_warnings::render(frame, area, &app.startup_warnings);
    }

    // Process effects
    app.effects
        .process_effects(elapsed.into(), frame.buffer_mut(), area);
}

/// Calculate clickable areas for each tab
fn calculate_tab_areas(tabs_area: Rect) -> Vec<Rect> {
    let tab_names = Tab::all();
    let mut areas = Vec::with_capacity(tab_names.len());

    // Tab format: " TabName │ TabName │ ..." with padding
    // Each tab takes: 1 space + name length + 1 space + 1 divider = name.len() + 3
    let mut x = tabs_area.x;
    for tab in tab_names {
        let name_len = tab.name().len() as u16;
        let tab_width = name_len + 3; // padding + divider

        areas.push(Rect {
            x,
            y: tabs_area.y,
            width: tab_width,
            height: 1,
        });

        x += tab_width;
    }

    areas
}
