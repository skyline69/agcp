use std::collections::{HashMap, VecDeque};
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
    Mappings,
    Quota,
    Usage,
    About,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[
            Tab::Overview,
            Tab::Logs,
            Tab::Accounts,
            Tab::Config,
            Tab::Mappings,
            Tab::Quota,
            Tab::Usage,
            Tab::About,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Logs => "Logs",
            Tab::Accounts => "Accounts",
            Tab::Config => "Config",
            Tab::Mappings => "Mappings",
            Tab::Quota => "Quota",
            Tab::Usage => "Usage",
            Tab::About => "About",
        }
    }

    pub fn next(&self) -> Tab {
        match self {
            Tab::Overview => Tab::Logs,
            Tab::Logs => Tab::Accounts,
            Tab::Accounts => Tab::Config,
            Tab::Config => Tab::Mappings,
            Tab::Mappings => Tab::Quota,
            Tab::Quota => Tab::Usage,
            Tab::Usage => Tab::About,
            Tab::About => Tab::Overview,
        }
    }

    pub fn prev(&self) -> Tab {
        match self {
            Tab::Overview => Tab::About,
            Tab::Logs => Tab::Overview,
            Tab::Accounts => Tab::Logs,
            Tab::Config => Tab::Accounts,
            Tab::Mappings => Tab::Config,
            Tab::Quota => Tab::Mappings,
            Tab::Usage => Tab::Quota,
            Tab::About => Tab::Usage,
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

/// Sort mode for accounts list
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSort {
    /// Default order (as loaded)
    Default,
    /// Email A-Z
    EmailAsc,
    /// Email Z-A
    EmailDesc,
    /// By subscription tier (Ultra > Pro > Free)
    Tier,
    /// Quota remaining high to low
    QuotaDesc,
    /// Quota remaining low to high
    QuotaAsc,
}

impl AccountSort {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::EmailAsc,
            Self::EmailAsc => Self::EmailDesc,
            Self::EmailDesc => Self::Tier,
            Self::Tier => Self::QuotaDesc,
            Self::QuotaDesc => Self::QuotaAsc,
            Self::QuotaAsc => Self::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::EmailAsc => "Email A-Z",
            Self::EmailDesc => "Email Z-A",
            Self::Tier => "Tier",
            Self::QuotaDesc => "Quota High-Low",
            Self::QuotaAsc => "Quota Low-High",
        }
    }
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
    /// Account search active
    pub account_search_active: bool,
    /// Account search query
    pub account_search_query: String,
    /// Account sort mode
    pub account_sort: AccountSort,
    /// Filtered + sorted account indices (into self.accounts)
    pub account_display_indices: Vec<usize>,
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
    /// Cached quota data fetched from API, keyed by account ID
    pub quota_data: HashMap<String, Vec<crate::cloudcode::quota::ModelQuota>>,
    /// Last time quota was refreshed
    last_quota_refresh: Instant,
    /// Whether quota fetch is in progress (to avoid duplicate fetches)
    quota_fetch_pending: bool,
    /// Receiver for background quota fetch results
    quota_receiver:
        Option<mpsc::Receiver<HashMap<String, Vec<crate::cloudcode::quota::ModelQuota>>>>,
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
    /// Receiver for background startup warnings collection
    startup_warnings_receiver: Option<mpsc::Receiver<Vec<super::widgets::StartupWarning>>>,
    /// About page: cached inner area for mouse detection
    pub about_area: Rect,
    /// About page: whether the GitHub link is hovered
    pub about_link_hovered: bool,
    /// Update check status for About page
    pub update_status: UpdateStatus,
    /// Receiver for background update check result
    update_receiver: Option<mpsc::Receiver<UpdateStatus>>,
    /// Receiver for background subscription tier refresh
    tier_refresh_receiver: Option<mpsc::Receiver<()>>,
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
    /// Cached token usage stats (fetched from /stats endpoint)
    pub cached_token_stats: Option<super::data::TokenStats>,
    /// Rolling time-series of token usage per model
    pub token_history: super::data::TokenHistory,
    /// Last time token stats were fetched
    last_token_stats_refresh: Instant,
    /// Last time token history was saved to disk
    last_token_history_save: Instant,
    /// Last tab area width used for tab_areas calculation (for invalidation)
    cached_tabs_area: Rect,
    // Log filtering and search state
    /// Level filter toggles: [Debug, Info, Warn, Error] - all enabled by default
    pub log_level_filter: [bool; 4],
    /// Account email filter (None = all accounts)
    pub log_account_filter: Option<String>,
    /// Whether search bar is active
    pub log_search_active: bool,
    /// Search query string
    pub log_search_query: String,
    /// Cached filtered indices into self.logs (indices of entries that pass filters)
    pub log_filtered_indices: Vec<usize>,
    /// Whether the account filter dropdown is open
    pub log_account_dropdown_open: bool,
    /// Selected index in account dropdown (for keyboard navigation)
    pub log_account_dropdown_selected: usize,
    /// Cached toolbar rects for mouse interaction: [Debug, Info, Warn, Error] level badge areas
    pub log_level_badge_areas: [Rect; 4],
    /// Cached toolbar rect for account filter click area
    pub log_account_filter_area: Rect,
    /// Cached dropdown area for mouse interaction
    pub log_dropdown_area: Rect,
    /// Cached dropdown item areas (one per item including "All Accounts")
    pub log_dropdown_item_areas: Vec<Rect>,
    /// Hovered level badge index (None if not hovering)
    pub hovered_log_level: Option<usize>,
    /// Hovered account filter label
    pub hovered_log_account: bool,
    /// Hovered dropdown item index (None if not hovering)
    pub hovered_log_dropdown_item: Option<usize>,
    /// Cached search bar area for click detection
    pub log_search_area: Rect,
    // Mappings tab state
    /// Current mapping preset name
    pub mapping_preset: crate::models::MappingPreset,
    /// Current mapping rules (loaded from config or preset)
    pub mapping_rules: Vec<crate::config::MappingRule>,
    /// Background task model
    pub mapping_background_model: String,
    /// Selected rule index in mappings view
    pub mapping_selected: usize,
    /// Whether we're editing a mapping rule's "from" pattern
    pub mapping_editing_from: bool,
    /// Text buffer for editing mapping "from" pattern
    pub mapping_edit_buffer: String,
    /// Cached mappings content area for mouse click detection
    pub mapping_area: Rect,
    /// Hovered mapping rule index
    pub hovered_mapping: Option<usize>,
    /// Status message for mappings (e.g. "Saved", "Preset loaded")
    pub mapping_status: Option<String>,
    /// Whether mappings have unsaved changes
    pub mapping_dirty: bool,
    /// Status message for daemon control (e.g. "Started", "Stopped", error)
    /// (message, is_error, timestamp for auto-clear)
    pub daemon_status_message: Option<(String, bool, Instant)>,
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

        let data = super::data::DataProvider::new();

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
            account_search_active: false,
            account_search_query: String::new(),
            account_sort: AccountSort::Default,
            account_display_indices: Vec::new(),
            show_help: false,
            startup_done: false,
            prev_show_help: false,
            trigger_tab_effect: false,
            animation_time_ms: 0,
            daemon_start_time,
            last_log_refresh: Instant::now() - Duration::from_secs(1),
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
            quota_data: HashMap::new(),
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
            // Startup warnings deferred -- populated by background thread
            startup_warnings: Vec::new(),
            show_startup_warnings: false,
            startup_warnings_receiver: None,
            about_area: Rect::default(),
            about_link_hovered: false,
            update_status: UpdateStatus::NotChecked,
            update_receiver: None,
            tier_refresh_receiver: None,
            // Server status deferred -- first real check happens via get_cached_server_status()
            cached_server_status: super::data::ServerStatus::Running,
            last_status_refresh: Instant::now() - Duration::from_secs(10),
            cached_request_count: 0,
            cached_model_usage: Vec::new(),
            cached_rate_history: Vec::new(),
            cached_avg_response_ms: None,
            cached_requests_per_min: 0.0,
            cached_uptime: String::from("00:00:00"),
            cached_token_stats: None,
            token_history: super::data::TokenHistory::load(),
            last_token_stats_refresh: Instant::now() - Duration::from_secs(10),
            last_token_history_save: Instant::now(),
            cached_tabs_area: Rect::default(),
            log_level_filter: [true; 4],
            log_account_filter: None,
            log_search_active: false,
            log_search_query: String::new(),
            log_filtered_indices: Vec::new(),
            log_account_dropdown_open: false,
            log_account_dropdown_selected: 0,
            log_level_badge_areas: [Rect::default(); 4],
            log_account_filter_area: Rect::default(),
            log_dropdown_area: Rect::default(),
            log_dropdown_item_areas: Vec::new(),
            hovered_log_level: None,
            hovered_log_account: false,
            hovered_log_dropdown_item: None,
            log_search_area: Rect::default(),
            // Mappings state: load from config
            mapping_preset: {
                let cfg = crate::config::get_config();
                crate::models::MappingPreset::from_name(&cfg.mappings.preset)
            },
            mapping_rules: {
                let cfg = crate::config::get_config();
                let preset = crate::models::MappingPreset::from_name(&cfg.mappings.preset);
                if cfg.mappings.rules.is_empty() && preset != crate::models::MappingPreset::Custom {
                    preset.rules()
                } else {
                    cfg.mappings.rules.clone()
                }
            },
            mapping_background_model: crate::config::get_config()
                .mappings
                .background_task_model
                .clone(),
            mapping_selected: 0,
            mapping_editing_from: false,
            mapping_edit_buffer: String::new(),
            mapping_area: Rect::default(),
            hovered_mapping: None,
            mapping_status: None,
            mapping_dirty: false,
            daemon_status_message: None,
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

        // Rebuild filtered indices if any filter is active
        if self.has_active_log_filter() {
            self.refilter_logs();
        }
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

    /// Refresh token stats from the server's /stats endpoint (every 1 second)
    pub fn maybe_refresh_token_stats(&mut self) {
        if self.last_token_stats_refresh.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_token_stats_refresh = Instant::now();
        self.cached_token_stats = super::data::DataProvider::fetch_token_stats();

        // Update quota period from quota data
        if self.token_history.period_start.is_none()
            && let Some(reset_time) = self.get_earliest_reset_time()
        {
            self.token_history.set_period_from_reset_time(&reset_time);
        }

        // Check for quota period reset
        if let Some(reset_time) = self.get_earliest_reset_time()
            && self.token_history.should_reset(&reset_time)
        {
            self.token_history.reset();
            // Update period_start for new period
            self.token_history.set_period_from_reset_time(&reset_time);
        }

        // Push new snapshot
        if let Some(ref stats) = self.cached_token_stats {
            self.token_history.push(stats);
        }

        // Save to disk periodically (every 30 seconds) to avoid I/O overhead
        if self.last_token_history_save.elapsed() >= Duration::from_secs(30) {
            self.last_token_history_save = Instant::now();
            self.token_history.save();
        }
    }

    /// Get the earliest quota reset time from quota data
    fn get_earliest_reset_time(&self) -> Option<String> {
        self.quota_data
            .values()
            .flat_map(|quotas| quotas.iter())
            .filter_map(|q| q.reset_time.as_ref())
            .min()
            .cloned()
    }

    /// Rebuild the filtered log indices based on current filter/search state
    pub fn refilter_logs(&mut self) {
        self.log_filtered_indices.clear();

        for (i, entry) in self.logs.iter().enumerate() {
            // Check level filter
            let level_idx = match entry.level {
                super::data::LogLevel::Debug => 0,
                super::data::LogLevel::Info => 1,
                super::data::LogLevel::Warn => 2,
                super::data::LogLevel::Error => 3,
            };
            if !self.log_level_filter[level_idx] {
                continue;
            }

            // Check account filter
            if let Some(ref filter_email) = self.log_account_filter {
                // Only filter lines that have an account field; pass through lines without one
                if let Some(ref line_email) = entry.account_email
                    && line_email != filter_email
                {
                    continue;
                }
            }

            // Check search query
            if !self.log_search_query.is_empty() {
                let query_lower = self.log_search_query.to_lowercase();
                if !entry.line.to_lowercase().contains(&query_lower) {
                    continue;
                }
            }

            self.log_filtered_indices.push(i);
        }
    }

    /// Check if any log filter is active (not all levels enabled, or account filter, or search)
    pub fn has_active_log_filter(&self) -> bool {
        !self.log_level_filter.iter().all(|&v| v)
            || self.log_account_filter.is_some()
            || !self.log_search_query.is_empty()
    }

    /// Get the list of unique account emails (from cached accounts + log entries)
    pub fn log_account_emails(&self) -> Vec<String> {
        let mut emails = std::collections::HashSet::new();

        // Add emails from the cached account list (always available)
        for acc in &self.accounts {
            if !acc.email.is_empty() {
                emails.insert(acc.email.clone());
            }
        }

        // Also add any emails seen in log entries (may include accounts no longer configured)
        for entry in self.logs.iter() {
            if let Some(ref email) = entry.account_email {
                emails.insert(email.clone());
            }
        }

        let mut sorted: Vec<String> = emails.into_iter().collect();
        sorted.sort();
        sorted
    }

    /// Toggle a log level filter
    pub fn toggle_log_level(&mut self, level_idx: usize) {
        if level_idx < 4 {
            self.log_level_filter[level_idx] = !self.log_level_filter[level_idx];
            self.refilter_logs();
        }
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

    /// Spawn a background thread to refresh subscription tiers from API
    fn spawn_tier_refresh(&mut self) {
        if self.tier_refresh_receiver.is_some() {
            return; // Already in progress
        }

        let (tx, rx) = mpsc::channel();
        self.tier_refresh_receiver = Some(rx);

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                if let Ok(mut store) = crate::auth::accounts::AccountStore::load() {
                    let http_client = crate::auth::HttpClient::new();
                    store.refresh_subscription_tiers(&http_client).await;
                }
            });
            let _ = tx.send(());
        });
    }

    /// Poll for tier refresh completion and reload accounts if done
    fn poll_tier_refresh(&mut self) {
        if let Some(ref receiver) = self.tier_refresh_receiver {
            match receiver.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.tier_refresh_receiver = None;
                    self.refresh_accounts();
                }
                Err(mpsc::TryRecvError::Empty) => {} // Still in progress
            }
        }
    }

    /// Spawn startup warnings collection in a background thread
    fn spawn_startup_warnings(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.startup_warnings_receiver = Some(rx);

        std::thread::spawn(move || {
            let warnings = super::widgets::startup_warnings::collect_startup_warnings();
            let _ = tx.send(warnings);
        });
    }

    /// Poll for startup warnings completion
    fn poll_startup_warnings(&mut self) {
        if let Some(ref receiver) = self.startup_warnings_receiver {
            match receiver.try_recv() {
                Ok(warnings) => {
                    self.show_startup_warnings = !warnings.is_empty();
                    self.startup_warnings = warnings;
                    self.startup_warnings_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.startup_warnings_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {} // Still in progress
            }
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

    /// Fetch quota data for all enabled accounts synchronously (blocking)
    /// Can be called from any thread — creates its own tokio runtime if needed
    fn fetch_quota_blocking()
    -> Result<HashMap<String, Vec<crate::cloudcode::quota::ModelQuota>>, String> {
        // Create a new runtime for this thread (works from any context)
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create runtime: {}", e))?;

        rt.block_on(async {
            let store = crate::auth::accounts::AccountStore::load().map_err(|e| e.to_string())?;

            let http_client = crate::auth::HttpClient::new();
            let mut result = HashMap::new();

            for account in &store.accounts {
                if !account.enabled || account.is_invalid {
                    continue;
                }

                let mut account_clone = account.clone();
                let access_token = match account_clone.get_access_token(&http_client).await {
                    Ok(token) => token,
                    Err(_) => continue,
                };

                match crate::cloudcode::fetch_model_quotas(
                    &http_client,
                    &access_token,
                    account.project_id.as_deref(),
                )
                .await
                {
                    Ok(quotas) => {
                        result.insert(account.id.clone(), quotas);
                    }
                    Err(_) => continue,
                }
            }

            Ok(result)
        })
    }

    /// Get average quota fraction for a specific account
    pub fn get_account_quota_fraction(&self, account_id: &str) -> Option<f64> {
        self.quota_data.get(account_id).map(|quotas| {
            if quotas.is_empty() {
                1.0
            } else {
                let total: f64 = quotas.iter().map(|q| q.remaining_fraction).sum();
                total / quotas.len() as f64
            }
        })
    }

    /// Get the active account's quota data (for Quota tab and Overview)
    pub fn get_active_quota_data(&self) -> &[crate::cloudcode::quota::ModelQuota] {
        // Find active account and return its quota data
        for acc in &self.accounts {
            if acc.is_active
                && let Some(quotas) = self.quota_data.get(&acc.id)
            {
                return quotas;
            }
        }
        // Fallback: return first available
        self.quota_data
            .values()
            .next()
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get average quota fraction across all accounts (for Overview stats panel)
    pub fn get_overall_quota_fraction(&self) -> Option<f64> {
        if self.quota_data.is_empty() {
            return None;
        }
        let all_quotas: Vec<f64> = self
            .quota_data
            .values()
            .flat_map(|quotas| quotas.iter().map(|q| q.remaining_fraction))
            .collect();
        if all_quotas.is_empty() {
            None
        } else {
            Some(all_quotas.iter().sum::<f64>() / all_quotas.len() as f64)
        }
    }

    /// Rebuild the filtered and sorted account display indices
    pub fn refilter_accounts(&mut self) {
        self.account_display_indices.clear();

        // Filter by search query
        for (i, acc) in self.accounts.iter().enumerate() {
            if !self.account_search_query.is_empty() {
                let query = self.account_search_query.to_lowercase();
                let email_match = acc.email.to_lowercase().contains(&query);
                let tier_match = acc
                    .subscription_tier
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query);
                if !email_match && !tier_match {
                    continue;
                }
            }
            self.account_display_indices.push(i);
        }

        // Sort
        let sort = self.account_sort;
        let quota_data = &self.quota_data;
        let accounts = &self.accounts;
        self.account_display_indices.sort_by(|&a, &b| {
            let acc_a = &accounts[a];
            let acc_b = &accounts[b];
            match sort {
                AccountSort::Default => a.cmp(&b),
                AccountSort::EmailAsc => {
                    acc_a.email.to_lowercase().cmp(&acc_b.email.to_lowercase())
                }
                AccountSort::EmailDesc => {
                    acc_b.email.to_lowercase().cmp(&acc_a.email.to_lowercase())
                }
                AccountSort::Tier => {
                    let tier_rank = |acc: &super::data::AccountInfo| -> u8 {
                        match acc
                            .subscription_tier
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .as_str()
                        {
                            "ultra" => 0,
                            "pro" => 1,
                            _ => 2,
                        }
                    };
                    tier_rank(acc_a).cmp(&tier_rank(acc_b))
                }
                AccountSort::QuotaDesc => {
                    let qa = quota_data
                        .get(&acc_a.id)
                        .map(|qs| {
                            if qs.is_empty() {
                                1.0
                            } else {
                                qs.iter().map(|q| q.remaining_fraction).sum::<f64>()
                                    / qs.len() as f64
                            }
                        })
                        .unwrap_or(acc_a.quota_fraction);
                    let qb = quota_data
                        .get(&acc_b.id)
                        .map(|qs| {
                            if qs.is_empty() {
                                1.0
                            } else {
                                qs.iter().map(|q| q.remaining_fraction).sum::<f64>()
                                    / qs.len() as f64
                            }
                        })
                        .unwrap_or(acc_b.quota_fraction);
                    qb.partial_cmp(&qa).unwrap_or(std::cmp::Ordering::Equal)
                }
                AccountSort::QuotaAsc => {
                    let qa = quota_data
                        .get(&acc_a.id)
                        .map(|qs| {
                            if qs.is_empty() {
                                1.0
                            } else {
                                qs.iter().map(|q| q.remaining_fraction).sum::<f64>()
                                    / qs.len() as f64
                            }
                        })
                        .unwrap_or(acc_a.quota_fraction);
                    let qb = quota_data
                        .get(&acc_b.id)
                        .map(|qs| {
                            if qs.is_empty() {
                                1.0
                            } else {
                                qs.iter().map(|q| q.remaining_fraction).sum::<f64>()
                                    / qs.len() as f64
                            }
                        })
                        .unwrap_or(acc_b.quota_fraction);
                    qa.partial_cmp(&qb).unwrap_or(std::cmp::Ordering::Equal)
                }
            }
        });

        // Clamp selection
        if !self.account_display_indices.is_empty() {
            self.account_selected = self
                .account_selected
                .min(self.account_display_indices.len() - 1);
        }
    }

    /// Check if any account filter is active
    pub fn has_active_account_filter(&self) -> bool {
        !self.account_search_query.is_empty() || self.account_sort != AccountSort::Default
    }

    /// Get the real account index from display index
    pub fn account_real_index(&self, display_idx: usize) -> Option<usize> {
        if self.has_active_account_filter() {
            self.account_display_indices.get(display_idx).copied()
        } else {
            Some(display_idx)
        }
    }

    /// Toggle enabled state of selected account
    fn toggle_account_enabled(&mut self) {
        let real_idx = self.account_real_index(self.account_selected);
        if let Some(idx) = real_idx
            && let Some(acc) = self.accounts.get(idx)
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
        let real_idx = self.account_real_index(self.account_selected);
        if let Some(idx) = real_idx
            && let Some(acc) = self.accounts.get(idx)
            && let Ok(mut store) = crate::auth::accounts::AccountStore::load()
        {
            store.active_account_id = Some(acc.id.clone());
            let _ = store.save();
            self.refresh_accounts();
        }
    }

    /// Cycle the target model for the selected mapping rule
    fn mapping_cycle_target(&mut self, forward: bool) {
        let targets = crate::models::all_target_models();
        if let Some(rule) = self.mapping_rules.get_mut(self.mapping_selected) {
            let current_idx = targets.iter().position(|t| *t == rule.to).unwrap_or(0);
            let new_idx = if forward {
                (current_idx + 1) % targets.len()
            } else {
                (current_idx + targets.len() - 1) % targets.len()
            };
            rule.to = targets[new_idx].to_string();
            self.mapping_preset = crate::models::MappingPreset::Custom;
            self.mapping_dirty = true;
        }
    }

    /// Cycle the background task model
    fn mapping_cycle_background(&mut self) {
        let targets = crate::models::all_target_models();
        let current_idx = targets
            .iter()
            .position(|t| *t == self.mapping_background_model.as_str())
            .unwrap_or(0);
        let new_idx = (current_idx + 1) % targets.len();
        self.mapping_background_model = targets[new_idx].to_string();
        self.mapping_dirty = true;
    }

    /// Save mapping rules to config
    fn mapping_save(&mut self) {
        let mut config = (*crate::config::get_config()).clone();
        config.mappings.preset = self.mapping_preset.name().to_string();
        config.mappings.background_task_model = self.mapping_background_model.clone();
        config.mappings.rules = self.mapping_rules.clone();
        if let Err(e) = config.save() {
            self.mapping_status = Some(format!("Error: {}", e));
            return;
        }
        crate::config::init_config(config);
        self.mapping_dirty = false;
        self.mapping_status = Some("Saved to config.toml".to_string());
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

        // Handle account dropdown when open (blocks other Logs input)
        if self.log_account_dropdown_open {
            match code {
                KeyCode::Esc => {
                    self.log_account_dropdown_open = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.log_account_dropdown_selected > 0 {
                        self.log_account_dropdown_selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let emails = self.log_account_emails();
                    let max = emails.len(); // 0 = "All", 1..=len = emails
                    if self.log_account_dropdown_selected < max {
                        self.log_account_dropdown_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let emails = self.log_account_emails();
                    if self.log_account_dropdown_selected == 0 {
                        self.log_account_filter = None;
                    } else if let Some(email) = emails.get(self.log_account_dropdown_selected - 1) {
                        self.log_account_filter = Some(email.clone());
                    }
                    self.log_account_dropdown_open = false;
                    self.refilter_logs();
                }
                _ => {}
            }
            return;
        }

        // Handle search input when active (captures most keys)
        if self.log_search_active {
            match code {
                KeyCode::Esc => {
                    self.log_search_active = false;
                    // Don't clear query - user might want to keep the filter
                }
                KeyCode::Enter => {
                    self.log_search_active = false;
                }
                KeyCode::Backspace => {
                    self.log_search_query.pop();
                    self.refilter_logs();
                }
                KeyCode::Char(c) => {
                    self.log_search_query.push(c);
                    self.refilter_logs();
                }
                _ => {}
            }
            return;
        }

        // Handle account search input when active
        if self.account_search_active {
            match code {
                KeyCode::Esc => {
                    self.account_search_active = false;
                }
                KeyCode::Enter => {
                    self.account_search_active = false;
                }
                KeyCode::Backspace => {
                    self.account_search_query.pop();
                    self.refilter_accounts();
                }
                KeyCode::Char(c) => {
                    self.account_search_query.push(c);
                    self.refilter_accounts();
                }
                _ => {}
            }
            return;
        }

        // Handle mapping rule "from" pattern editing
        if self.mapping_editing_from {
            match code {
                KeyCode::Esc => {
                    self.mapping_editing_from = false;
                    self.mapping_edit_buffer.clear();
                }
                KeyCode::Enter => {
                    // Apply edited pattern
                    if let Some(rule) = self.mapping_rules.get_mut(self.mapping_selected) {
                        rule.from = self.mapping_edit_buffer.clone();
                        self.mapping_preset = crate::models::MappingPreset::Custom;
                        self.mapping_dirty = true;
                    }
                    self.mapping_editing_from = false;
                    self.mapping_edit_buffer.clear();
                }
                KeyCode::Backspace => {
                    self.mapping_edit_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.mapping_edit_buffer.push(c);
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
            // Cycle target model for selected rule (right arrow)
            KeyCode::Right if self.current_tab == Tab::Mappings => {
                self.mapping_cycle_target(true);
            }
            KeyCode::Left if self.current_tab == Tab::Mappings => {
                self.mapping_cycle_target(false);
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
                self.current_tab = Tab::Mappings;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('6') => {
                self.current_tab = Tab::Quota;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('7') => {
                self.current_tab = Tab::Usage;
                self.trigger_tab_effect = true;
            }
            KeyCode::Char('8') => {
                self.current_tab = Tab::About;
                self.trigger_tab_effect = true;
            }
            // Overview daemon controls
            KeyCode::Char('s') if self.current_tab == Tab::Overview => {
                self.start_daemon();
            }
            KeyCode::Char('x') if self.current_tab == Tab::Overview => {
                self.stop_daemon();
            }
            KeyCode::Char('r') if self.current_tab == Tab::Overview => {
                self.daemon_status_message =
                    Some(("Restarting...".to_string(), false, Instant::now()));
                self.restart_daemon();
                if self.config_error.is_some() {
                    // restart_daemon sets config_error on failure
                    self.daemon_status_message =
                        Some((self.config_error.take().unwrap(), true, Instant::now()));
                } else {
                    self.daemon_status_message =
                        Some(("Restarted".to_string(), false, Instant::now()));
                    // Force immediate status refresh
                    self.last_status_refresh = Instant::now() - std::time::Duration::from_secs(10);
                }
            }
            // Usage tab controls
            KeyCode::Char('r') if self.current_tab == Tab::Usage => {
                self.token_history.reset();
            }
            // Account navigation (when on Accounts tab)
            KeyCode::Up | KeyCode::Char('k') if self.current_tab == Tab::Accounts => {
                if self.account_selected > 0 {
                    self.account_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.current_tab == Tab::Accounts => {
                let count = if self.has_active_account_filter() {
                    self.account_display_indices.len()
                } else {
                    self.accounts.len()
                };
                if self.account_selected < count.saturating_sub(1) {
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
            // Account search
            KeyCode::Char('/') if self.current_tab == Tab::Accounts => {
                self.account_search_active = true;
            }
            // Account sort cycle
            KeyCode::Char('s') if self.current_tab == Tab::Accounts => {
                self.account_sort = self.account_sort.next();
                self.refilter_accounts();
            }
            // Clear account filters
            KeyCode::Char('c') if self.current_tab == Tab::Accounts => {
                self.account_search_query.clear();
                self.account_search_active = false;
                self.account_sort = AccountSort::Default;
                self.account_display_indices.clear();
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
            // Log filter/search keybindings
            KeyCode::Char('/') if self.current_tab == Tab::Logs => {
                self.log_search_active = true;
            }
            KeyCode::Char('d') if self.current_tab == Tab::Logs => {
                self.toggle_log_level(0); // Debug
            }
            KeyCode::Char('i') if self.current_tab == Tab::Logs => {
                self.toggle_log_level(1); // Info
            }
            KeyCode::Char('w') if self.current_tab == Tab::Logs => {
                self.toggle_log_level(2); // Warn
            }
            KeyCode::Char('e') if self.current_tab == Tab::Logs => {
                self.toggle_log_level(3); // Error
            }
            KeyCode::Char('a') if self.current_tab == Tab::Logs => {
                self.log_account_dropdown_open = true;
                // Pre-select current filter in dropdown
                if let Some(ref email) = self.log_account_filter {
                    let emails = self.log_account_emails();
                    self.log_account_dropdown_selected = emails
                        .iter()
                        .position(|e| e == email)
                        .map(|i| i + 1)
                        .unwrap_or(0);
                } else {
                    self.log_account_dropdown_selected = 0;
                }
            }
            KeyCode::Char('c') if self.current_tab == Tab::Logs => {
                // Clear all filters
                self.log_level_filter = [true; 4];
                self.log_account_filter = None;
                self.log_search_query.clear();
                self.log_search_active = false;
                self.log_filtered_indices.clear();
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
            // Mappings navigation
            KeyCode::Up | KeyCode::Char('k') if self.current_tab == Tab::Mappings => {
                if self.mapping_selected > 0 {
                    self.mapping_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.current_tab == Tab::Mappings => {
                if self.mapping_selected < self.mapping_rules.len().saturating_sub(1) {
                    self.mapping_selected += 1;
                }
            }
            // Cycle preset
            KeyCode::Char('p') if self.current_tab == Tab::Mappings => {
                self.mapping_preset = self.mapping_preset.next();
                if self.mapping_preset != crate::models::MappingPreset::Custom {
                    self.mapping_rules = self.mapping_preset.rules();
                    self.mapping_selected = 0;
                }
                self.mapping_dirty = true;
                self.mapping_status = Some(format!("Preset: {}", self.mapping_preset.label()));
            }
            // Edit selected rule's "from" pattern
            KeyCode::Enter if self.current_tab == Tab::Mappings => {
                if let Some(rule) = self.mapping_rules.get(self.mapping_selected) {
                    self.mapping_editing_from = true;
                    self.mapping_edit_buffer = rule.from.clone();
                }
            }
            // Add new rule
            KeyCode::Char('a') if self.current_tab == Tab::Mappings => {
                self.mapping_rules.push(crate::config::MappingRule {
                    from: "pattern-*".to_string(),
                    to: "gemini-3-flash".to_string(),
                });
                self.mapping_selected = self.mapping_rules.len() - 1;
                self.mapping_preset = crate::models::MappingPreset::Custom;
                self.mapping_dirty = true;
                // Start editing immediately
                self.mapping_editing_from = true;
                self.mapping_edit_buffer = "pattern-*".to_string();
            }
            // Delete selected rule
            KeyCode::Char('d') if self.current_tab == Tab::Mappings => {
                if !self.mapping_rules.is_empty() {
                    self.mapping_rules.remove(self.mapping_selected);
                    if self.mapping_selected >= self.mapping_rules.len()
                        && self.mapping_selected > 0
                    {
                        self.mapping_selected -= 1;
                    }
                    self.mapping_preset = crate::models::MappingPreset::Custom;
                    self.mapping_dirty = true;
                }
            }
            // Cycle background task model
            KeyCode::Char('b') if self.current_tab == Tab::Mappings => {
                self.mapping_cycle_background();
            }
            // Save mappings
            KeyCode::Char('s') if self.current_tab == Tab::Mappings => {
                self.mapping_save();
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
        let mut config = (*crate::config::get_config()).clone();

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
        use std::fs::OpenOptions;
        use std::process::Command;

        let config = crate::config::get_config();
        let config_dir = crate::config::Config::dir();
        let pid_path = config_dir.join("agcp.pid");
        let log_path = config_dir.join("agcp.log");

        // Step 1: Stop the running daemon via PID file
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = pid_str.trim().parse::<i32>()
        {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .status();
            }

            // Wait for process to stop (up to 3 seconds)
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                #[cfg(unix)]
                {
                    if unsafe { libc::kill(pid, 0) } != 0 {
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(crate::config::get_addr_path());

        // Small delay to ensure port is released
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Step 2: Start new daemon (same as run_daemon in main.rs)
        let exe = std::env::current_exe().unwrap_or_else(|_| "agcp".into());

        // Open log file for output redirection
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log_file = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(_) => {
                self.config_error = Some("Failed to open log file for daemon".to_string());
                return;
            }
        };

        let mut cmd = Command::new(&exe);
        cmd.arg("--foreground");
        cmd.args(["--port", &config.server.port.to_string()]);
        cmd.args(["--host", &config.server.host]);
        if config.logging.debug {
            cmd.arg("--debug");
        }
        if config.accounts.fallback {
            cmd.arg("--fallback");
        }

        cmd.stdout(log_file.try_clone().unwrap_or(log_file));
        // Re-open for stderr since we may have consumed it
        if let Ok(stderr_file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            cmd.stderr(stderr_file);
        }

        // Detach from terminal
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                // Write new PID
                let _ = std::fs::write(&pid_path, child.id().to_string());
                // Write addr file for the new daemon
                let addr_path = crate::config::get_addr_path();
                let _ = std::fs::write(
                    &addr_path,
                    format!("{}:{}", config.server.host, config.server.port),
                );
            }
            Err(e) => {
                self.config_error = Some(format!("Failed to start daemon: {}", e));
                return;
            }
        }

        self.config_needs_restart = false;

        // Reload config fields to reflect saved state
        self.config_fields =
            super::config_editor::build_config_fields(&crate::config::get_config());
    }

    /// Start the daemon (when it's not running)
    pub fn start_daemon(&mut self) {
        use std::fs::OpenOptions;
        use std::process::Command;

        // Check if already running
        if self.get_cached_server_status().is_running() {
            self.daemon_status_message =
                Some(("Already running".to_string(), true, Instant::now()));
            return;
        }

        let config = crate::config::get_config();
        let config_dir = crate::config::Config::dir();
        let pid_path = config_dir.join("agcp.pid");
        let log_path = config_dir.join("agcp.log");

        let exe = std::env::current_exe().unwrap_or_else(|_| "agcp".into());

        // Open log file for output redirection
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log_file = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(_) => {
                self.daemon_status_message =
                    Some(("Failed to open log file".to_string(), true, Instant::now()));
                return;
            }
        };

        let mut cmd = Command::new(&exe);
        cmd.arg("--foreground");
        cmd.args(["--port", &config.server.port.to_string()]);
        cmd.args(["--host", &config.server.host]);
        if config.logging.debug {
            cmd.arg("--debug");
        }
        if config.accounts.fallback {
            cmd.arg("--fallback");
        }

        cmd.stdout(log_file.try_clone().unwrap_or(log_file));
        if let Ok(stderr_file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            cmd.stderr(stderr_file);
        }

        // Detach from terminal
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                let _ = std::fs::write(&pid_path, child.id().to_string());
                // Write the addr file so other tools/TUI can discover the real port
                let addr_path = crate::config::get_addr_path();
                let _ = std::fs::write(
                    &addr_path,
                    format!("{}:{}", config.server.host, config.server.port),
                );
                self.daemon_status_message = Some(("Started".to_string(), false, Instant::now()));
                // Force immediate status refresh
                self.last_status_refresh = Instant::now() - std::time::Duration::from_secs(10);
            }
            Err(e) => {
                self.daemon_status_message =
                    Some((format!("Failed to start: {}", e), true, Instant::now()));
            }
        }
    }

    /// Stop the daemon (when it's running)
    pub fn stop_daemon(&mut self) {
        let config_dir = crate::config::Config::dir();
        let pid_path = config_dir.join("agcp.pid");

        let pid_str = match std::fs::read_to_string(&pid_path) {
            Ok(s) => s,
            Err(_) => {
                self.daemon_status_message =
                    Some(("Not running".to_string(), true, Instant::now()));
                return;
            }
        };

        let pid: i32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                let _ = std::fs::remove_file(&pid_path);
                self.daemon_status_message =
                    Some(("Not running".to_string(), true, Instant::now()));
                return;
            }
        };

        // Check if actually alive
        #[cfg(unix)]
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        #[cfg(not(unix))]
        let alive = true;

        if !alive {
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(crate::config::get_addr_path());
            self.daemon_status_message = Some(("Not running".to_string(), true, Instant::now()));
            return;
        }

        // Send SIGTERM
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .status();
        }

        // Wait for process to stop (up to 2 seconds)
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            #[cfg(unix)]
            {
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
            }
        }

        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(crate::config::get_addr_path());
        self.daemon_status_message = Some(("Stopped".to_string(), false, Instant::now()));
        // Force immediate status refresh
        self.last_status_refresh = Instant::now() - std::time::Duration::from_secs(10);
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
                // Handle dropdown item clicks first (when dropdown is open)
                if self.log_account_dropdown_open {
                    // Check if click is inside the dropdown
                    if self.is_in_rect(column, row, self.log_dropdown_area) {
                        for (i, item_area) in self.log_dropdown_item_areas.iter().enumerate() {
                            if self.is_in_rect(column, row, *item_area) {
                                let emails = self.log_account_emails();
                                if i == 0 {
                                    self.log_account_filter = None;
                                } else if let Some(email) = emails.get(i - 1) {
                                    self.log_account_filter = Some(email.clone());
                                }
                                self.log_account_dropdown_open = false;
                                self.refilter_logs();
                                return;
                            }
                        }
                    } else {
                        // Click outside dropdown closes it
                        self.log_account_dropdown_open = false;
                    }
                    return;
                }

                // Check logs toolbar clicks (when on Logs tab)
                if self.current_tab == Tab::Logs {
                    // Check level badge clicks
                    for i in 0..4 {
                        if self.is_in_rect(column, row, self.log_level_badge_areas[i]) {
                            self.toggle_log_level(i);
                            return;
                        }
                    }

                    // Check account filter click
                    if self.is_in_rect(column, row, self.log_account_filter_area) {
                        self.log_account_dropdown_open = !self.log_account_dropdown_open;
                        if self.log_account_dropdown_open {
                            // Pre-select current filter in dropdown
                            if let Some(ref email) = self.log_account_filter {
                                let emails = self.log_account_emails();
                                self.log_account_dropdown_selected = emails
                                    .iter()
                                    .position(|e| e == email)
                                    .map(|i| i + 1)
                                    .unwrap_or(0);
                            } else {
                                self.log_account_dropdown_selected = 0;
                            }
                        }
                        return;
                    }

                    // Check search area click (activates search)
                    if self.is_in_rect(column, row, self.log_search_area) {
                        self.log_search_active = true;
                        return;
                    }
                }

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
                    let relative_row = row.saturating_sub(self.accounts_area.y) as usize;
                    let count = if self.has_active_account_filter() {
                        self.account_display_indices.len()
                    } else {
                        self.accounts.len()
                    };
                    if relative_row < count {
                        self.account_selected = relative_row;
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

                // Check mapping rule clicks
                if self.current_tab == Tab::Mappings
                    && self.is_in_rect(column, row, self.mapping_area)
                {
                    let relative_row = row.saturating_sub(self.mapping_area.y) as usize;
                    if relative_row < self.mapping_rules.len() {
                        self.mapping_selected = relative_row;
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

        // Check log filter toolbar hover states
        self.hovered_log_level = None;
        self.hovered_log_account = false;
        self.hovered_log_dropdown_item = None;

        if self.current_tab == Tab::Logs {
            // Check dropdown item hover (when open)
            if self.log_account_dropdown_open {
                for (i, item_area) in self.log_dropdown_item_areas.iter().enumerate() {
                    if self.is_in_rect(column, row, *item_area) {
                        self.hovered_log_dropdown_item = Some(i);
                        // Also update keyboard selection to follow mouse
                        self.log_account_dropdown_selected = i;
                        break;
                    }
                }
            }

            // Check level badge hover
            for i in 0..4 {
                if self.is_in_rect(column, row, self.log_level_badge_areas[i]) {
                    self.hovered_log_level = Some(i);
                    break;
                }
            }

            // Check account filter hover
            if self.is_in_rect(column, row, self.log_account_filter_area) {
                self.hovered_log_account = true;
            }
        }

        // Check account hover
        self.hovered_account = None;
        if self.current_tab == Tab::Accounts && self.is_in_rect(column, row, self.accounts_area) {
            let relative_row = row.saturating_sub(self.accounts_area.y) as usize;
            let count = if self.has_active_account_filter() {
                self.account_display_indices.len()
            } else {
                self.accounts.len()
            };
            if relative_row < count {
                self.hovered_account = Some(relative_row);
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

        // Check mapping rule hover
        self.hovered_mapping = None;
        if self.current_tab == Tab::Mappings && self.is_in_rect(column, row, self.mapping_area) {
            let relative_row = row.saturating_sub(self.mapping_area.y) as usize;
            if relative_row < self.mapping_rules.len() {
                self.hovered_mapping = Some(relative_row);
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
    app.spawn_tier_refresh();
    app.spawn_startup_warnings();
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

        // Refresh token usage stats periodically (every 5 seconds, only on relevant tabs)
        if matches!(app.current_tab, Tab::Usage | Tab::Overview) {
            app.maybe_refresh_token_stats();
        }

        // Poll for background tier refresh completion
        app.poll_tier_refresh();

        // Poll for background startup warnings
        app.poll_startup_warnings();

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
        Tab::Mappings => super::views::mappings::render(frame, content_area, app),
        Tab::Quota => super::views::quota::render(frame, content_area, app.get_active_quota_data()),
        Tab::Usage => super::views::usage::render(frame, content_area, app),
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
